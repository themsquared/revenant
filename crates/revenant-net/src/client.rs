//! Client a revenant uses to muster at a Necropolis: register itself, discover
//! peers, publish signed artifacts, browse the catalog, pull + verify, and
//! attest a successful local re-verification.

use crate::artifact::Artifact;
use crate::attest::Attestation;
use crate::ledger::Entry;
use crate::reply::Reply;
use crate::scroll::Scroll;
use anyhow::{bail, Context, Result};

/// A peer Necropolis's ledger head — its current length and chained hash.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct LedgerHead {
    pub seq: i64,
    pub hash: String,
}

/// A codex search hit set: matching Scrolls + artifact summaries.
#[derive(Debug, Clone, Default, serde::Deserialize)]
pub struct SearchResults {
    #[serde(default)]
    pub scrolls: Vec<Scroll>,
    #[serde(default)]
    pub artifacts: Vec<serde_json::Value>,
}

#[derive(Clone)]
pub struct NecropolisClient {
    base: String,
    http: reqwest::Client,
}

impl NecropolisClient {
    pub fn new(base: impl Into<String>) -> Self {
        NecropolisClient { base: base.into(), http: reqwest::Client::new() }
    }

    fn url(&self, path: &str) -> String {
        format!("{}{}", self.base.trim_end_matches('/'), path)
    }

    /// Register a human by email. Returns the server JSON (account_key, and in
    /// dev mode the verify_token).
    pub async fn signup(&self, email: &str) -> Result<serde_json::Value> {
        let resp = self
            .http
            .post(self.url("/account/register"))
            .json(&serde_json::json!({ "email": email }))
            .send()
            .await
            .context("POST /account/register")?;
        if !resp.status().is_success() {
            bail!("signup failed: {}", resp.text().await.unwrap_or_default());
        }
        Ok(resp.json().await?)
    }

    /// Verify an emailed token.
    pub async fn verify_account(&self, token: &str) -> Result<()> {
        let resp = self
            .http
            .post(self.url("/account/verify"))
            .json(&serde_json::json!({ "token": token }))
            .send()
            .await
            .context("POST /account/verify")?;
        if !resp.status().is_success() {
            bail!("verify failed: {}", resp.text().await.unwrap_or_default());
        }
        Ok(())
    }

    /// Bind this agent's pubkey to a verified account (sig proves ownership).
    pub async fn bind_agent(&self, account_key: &str, pubkey: &str, sig: &str) -> Result<()> {
        let resp = self
            .http
            .post(self.url("/account/bind"))
            .json(&serde_json::json!({ "account_key": account_key, "pubkey": pubkey, "sig": sig }))
            .send()
            .await
            .context("POST /account/bind")?;
        if !resp.status().is_success() {
            bail!("bind failed: {}", resp.text().await.unwrap_or_default());
        }
        Ok(())
    }

    pub async fn register(&self, id: &str, endpoint: &str, capabilities: &[String]) -> Result<()> {
        let resp = self
            .http
            .post(self.url("/register"))
            .json(&serde_json::json!({ "id": id, "endpoint": endpoint, "capabilities": capabilities }))
            .send()
            .await
            .context("registering with necropolis")?;
        if !resp.status().is_success() {
            bail!("register failed: {}", resp.text().await.unwrap_or_default());
        }
        Ok(())
    }

    pub async fn peers(&self) -> Result<Vec<serde_json::Value>> {
        Ok(self.http.get(self.url("/peers")).send().await?.json().await?)
    }

    pub async fn publish(&self, artifact: &Artifact) -> Result<String> {
        let resp = self.http.post(self.url("/artifacts")).json(artifact).send().await?;
        if !resp.status().is_success() {
            bail!("publish failed: {}", resp.text().await.unwrap_or_default());
        }
        let v: serde_json::Value = resp.json().await?;
        Ok(v["id"].as_str().unwrap_or_default().to_string())
    }

    pub async fn list(&self, kind: Option<&str>) -> Result<Vec<serde_json::Value>> {
        let mut url = self.url("/artifacts");
        if let Some(k) = kind {
            url.push_str(&format!("?kind={k}"));
        }
        Ok(self.http.get(url).send().await?.json().await?)
    }

    /// Fetch an artifact and verify its signature + content hash before
    /// returning it. A tampered or forged artifact is an error, not a value.
    pub async fn pull(&self, id: &str) -> Result<Artifact> {
        let resp = self.http.get(self.url(&format!("/artifacts/{id}"))).send().await?;
        if !resp.status().is_success() {
            bail!("pull failed: {}", resp.text().await.unwrap_or_default());
        }
        let artifact: Artifact = resp.json().await?;
        if !artifact.verify() {
            bail!("pulled artifact {id} FAILED verification — refusing to trust it");
        }
        Ok(artifact)
    }

    /// Report a successful local re-verification of an artifact's eval proof.
    pub async fn attest(&self, id: &str, verifier: &str, passed: bool) -> Result<()> {
        self.http
            .post(self.url(&format!("/artifacts/{id}/attest")))
            .json(&serde_json::json!({ "verifier": verifier, "passed": passed }))
            .send()
            .await
            .context("attesting")?;
        Ok(())
    }

    /// Publish a signed reproduction attestation — proof this revenant re-ran an
    /// improvement's eval and reproduced (or didn't) the win. Feeds the quorum.
    pub async fn publish_reproduction(&self, att: &Attestation) -> Result<()> {
        let resp = self.http.post(self.url("/reproductions")).json(att).send().await?;
        if !resp.status().is_success() {
            bail!("publish_reproduction failed: {}", resp.text().await.unwrap_or_default());
        }
        Ok(())
    }

    /// All signed reproductions vouching for an artifact (the raw quorum input).
    pub async fn reproductions(&self, id: &str) -> Result<Vec<Attestation>> {
        Ok(self.http.get(self.url(&format!("/artifacts/{id}/reproductions"))).send().await?.json().await?)
    }

    /// Inscribe a signed Scroll into the Vault feed.
    pub async fn inscribe_scroll(&self, scroll: &Scroll) -> Result<()> {
        let resp = self.http.post(self.url("/scrolls")).json(scroll).send().await?;
        if !resp.status().is_success() {
            bail!("inscribe_scroll failed: {}", resp.text().await.unwrap_or_default());
        }
        Ok(())
    }

    /// Read the Vault feed (newest-first), optionally filtered by sigil/tome.
    pub async fn feed(&self) -> Result<Vec<Scroll>> {
        Ok(self.http.get(self.url("/scrolls")).send().await?.json().await?)
    }

    /// Search the codex — keyword across Scrolls (body/sigils/tome) + artifacts.
    pub async fn search(&self, q: &str) -> Result<SearchResults> {
        Ok(self
            .http
            .get(self.url("/search"))
            .query(&[("q", q)])
            .send()
            .await?
            .json()
            .await?)
    }

    /// Post a signed reply under a Scroll (the discussion thread).
    pub async fn reply(&self, scroll_id: &str, r: &Reply) -> Result<()> {
        let resp =
            self.http.post(self.url(&format!("/scrolls/{scroll_id}/replies"))).json(r).send().await?;
        if !resp.status().is_success() {
            bail!("reply failed: {}", resp.text().await.unwrap_or_default());
        }
        Ok(())
    }

    /// Read the replies under a Scroll (oldest-first).
    pub async fn replies(&self, scroll_id: &str) -> Result<Vec<Reply>> {
        Ok(self.http.get(self.url(&format!("/scrolls/{scroll_id}/replies"))).send().await?.json().await?)
    }

    /// The peer's current ledger head — how far its history has advanced.
    pub async fn ledger_head(&self) -> Result<LedgerHead> {
        Ok(self.http.get(self.url("/ledger/head")).send().await?.json().await?)
    }

    /// Pull the peer's ledger entries with `seq > since`, in order. The bytes
    /// are transferred verbatim so the caller can recompute each hash and
    /// re-verify the chain locally before trusting a single entry.
    pub async fn ledger_since(&self, since: i64) -> Result<Vec<Entry>> {
        let resp = self.http.get(self.url(&format!("/ledger/since/{since}"))).send().await?;
        if !resp.status().is_success() {
            bail!("ledger pull failed: {}", resp.text().await.unwrap_or_default());
        }
        Ok(resp.json().await?)
    }
}
