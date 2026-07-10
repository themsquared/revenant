//! revenant-client: typed client for the /v1 control plane, shared by the
//! CLI and TUI. Surfaces never touch the DB or the runtime directly.

use anyhow::{bail, Context, Result};
use eventsource_stream::Eventsource;
use futures::stream::BoxStream;
use futures::StreamExt;
use revenant_core::Event;
use serde::Deserialize;
use serde_json::json;

#[derive(Clone)]
pub struct Client {
    http: reqwest::Client,
    base: String,
    token: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SessionView {
    pub id: i64,
    pub channel: String,
    pub peer: String,
    pub kind: String,
    pub last_active: i64,
    pub message_count: i64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ApprovalView {
    pub id: String,
    pub kind: String,
    pub payload: String,
    pub requested_at: i64,
    pub ttl_s: i64,
}

impl ApprovalView {
    /// The human summary embedded in the payload by the broker.
    pub fn summary(&self) -> String {
        serde_json::from_str::<serde_json::Value>(&self.payload)
            .ok()
            .and_then(|v| v.get("summary").and_then(|s| s.as_str()).map(String::from))
            .unwrap_or_else(|| self.kind.clone())
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct SpendView {
    pub model: String,
    pub tokens_in: i64,
    pub tokens_out: i64,
    pub requests: i64,
}

impl Client {
    pub fn new(base: impl Into<String>, token: impl Into<String>) -> Self {
        Client {
            http: reqwest::Client::new(),
            base: base.into(),
            token: token.into(),
        }
    }

    /// Standard construction: token from `~/.revenant/token`, base URL from
    /// `REVENANT_URL` (default local daemon).
    pub fn from_env(home: &revenant_core::home::Home) -> Result<Self> {
        let token = std::fs::read_to_string(home.root().join("token"))
            .context("reading ~/.revenant/token — run `revenant init`")?
            .trim()
            .to_string();
        let base = std::env::var("REVENANT_URL")
            .unwrap_or_else(|_| "http://127.0.0.1:7717".to_string());
        Ok(Client::new(base, token))
    }

    fn req(&self, method: reqwest::Method, path: &str) -> reqwest::RequestBuilder {
        self.http
            .request(method, format!("{}{}", self.base, path))
            .bearer_auth(&self.token)
    }

    pub async fn health(&self) -> Result<serde_json::Value> {
        Ok(self
            .req(reqwest::Method::GET, "/v1/health")
            .timeout(std::time::Duration::from_secs(3))
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?)
    }

    pub async fn create_session(&self, peer: &str) -> Result<i64> {
        let resp: serde_json::Value = self
            .req(reqwest::Method::POST, "/v1/sessions")
            .json(&json!({ "peer": peer }))
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        resp.get("id").and_then(|v| v.as_i64()).context("no session id in response")
    }

    pub async fn sessions(&self) -> Result<Vec<SessionView>> {
        #[derive(Deserialize)]
        struct Resp {
            sessions: Vec<SessionView>,
        }
        Ok(self
            .req(reqwest::Method::GET, "/v1/sessions")
            .send()
            .await?
            .error_for_status()?
            .json::<Resp>()
            .await?
            .sessions)
    }

    pub async fn send_message(&self, session_id: i64, text: &str, tier: Option<&str>) -> Result<()> {
        let mut body = json!({ "text": text });
        if let Some(tier) = tier {
            body["tier"] = json!(tier);
        }
        let resp = self
            .req(reqwest::Method::POST, &format!("/v1/sessions/{session_id}/messages"))
            .json(&body)
            .send()
            .await?;
        if !resp.status().is_success() {
            bail!("send failed ({}): {}", resp.status(), resp.text().await.unwrap_or_default());
        }
        Ok(())
    }

    pub async fn approvals_pending(&self) -> Result<Vec<ApprovalView>> {
        #[derive(Deserialize)]
        struct Resp {
            approvals: Vec<ApprovalView>,
        }
        Ok(self
            .req(reqwest::Method::GET, "/v1/approvals")
            .send()
            .await?
            .error_for_status()?
            .json::<Resp>()
            .await?
            .approvals)
    }

    pub async fn decide(&self, approval_id: &str, approve: bool, resolver: &str) -> Result<bool> {
        let resp: serde_json::Value = self
            .req(
                reqwest::Method::POST,
                &format!("/v1/approvals/{approval_id}/decision"),
            )
            .json(&json!({ "approve": approve, "resolver": resolver }))
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        Ok(resp.get("applied").and_then(|v| v.as_bool()).unwrap_or(false))
    }

    pub async fn spend(&self, window: &str) -> Result<Vec<SpendView>> {
        #[derive(Deserialize)]
        struct Resp {
            by_model: Vec<SpendView>,
        }
        Ok(self
            .req(reqwest::Method::GET, &format!("/v1/spend?window={window}"))
            .send()
            .await?
            .error_for_status()?
            .json::<Resp>()
            .await?
            .by_model)
    }

    /// Set (or clear, with None) a session's personality.
    pub async fn set_persona(&self, session_id: i64, persona: Option<&str>) -> Result<()> {
        self.req(reqwest::Method::POST, &format!("/v1/sessions/{session_id}/persona"))
            .json(&json!({ "persona": persona }))
            .send()
            .await?
            .error_for_status()?;
        Ok(())
    }

    pub async fn personalities(&self) -> Result<Vec<String>> {
        #[derive(Deserialize)]
        struct P {
            name: String,
        }
        #[derive(Deserialize)]
        struct Resp {
            personalities: Vec<P>,
        }
        Ok(self
            .req(reqwest::Method::GET, "/v1/personalities")
            .send()
            .await?
            .error_for_status()?
            .json::<Resp>()
            .await?
            .personalities
            .into_iter()
            .map(|p| p.name)
            .collect())
    }

    /// Mint a one-time channel pairing code.
    pub async fn create_pairing(&self) -> Result<String> {
        let resp: serde_json::Value = self
            .req(reqwest::Method::POST, "/v1/channels/pairings")
            .json(&json!({}))
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        resp.get("code")
            .and_then(|c| c.as_str())
            .map(String::from)
            .context("no code in pairing response")
    }

    /// Live event stream. Reconnects are the caller's concern (M1).
    pub async fn events(&self) -> Result<BoxStream<'static, Result<Event>>> {
        let resp = self
            .req(reqwest::Method::GET, "/v1/events")
            .send()
            .await?
            .error_for_status()?;
        Ok(resp
            .bytes_stream()
            .eventsource()
            .filter_map(|item| async {
                match item {
                    Ok(sse) => match serde_json::from_str::<Event>(&sse.data) {
                        Ok(event) => Some(Ok(event)),
                        Err(_) => None, // keep-alives etc.
                    },
                    Err(err) => Some(Err(anyhow::anyhow!("event stream error: {err}"))),
                }
            })
            .boxed())
    }
}
