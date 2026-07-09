//! revenant-llm: Anthropic Messages wire client, pointed at the gateway.
//!
//! The harness speaks exactly one protocol — Anthropic Messages — to
//! agentgateway, which cross-translates to whatever upstream the tier alias
//! maps to. `model` always carries a tier alias, never a real model name.

use anyhow::{bail, Context, Result};
use eventsource_stream::Eventsource;
use futures::StreamExt;
use revenant_core::{ContentBlock, Role, Usage};
use serde::{Deserialize, Serialize};
use std::time::Duration;

#[derive(Debug, Clone, Serialize)]
pub struct WireMessage {
    pub role: &'static str,
    pub content: Vec<ContentBlock>,
}

impl WireMessage {
    pub fn new(role: Role, content: Vec<ContentBlock>) -> Self {
        WireMessage { role: role.as_str_static(), content }
    }
}

trait RoleExt {
    fn as_str_static(&self) -> &'static str;
}
impl RoleExt for Role {
    fn as_str_static(&self) -> &'static str {
        match self {
            Role::User => "user",
            Role::Assistant => "assistant",
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct MessagesRequest {
    pub model: String,
    pub max_tokens: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub system: Option<String>,
    pub messages: Vec<WireMessage>,
    pub stream: bool,
}

#[derive(Debug, Clone, Default)]
pub struct StreamOutcome {
    /// Concatenated text of all text blocks.
    pub text: String,
    pub stop_reason: Option<String>,
    pub usage: Usage,
    /// The real model the gateway routed to (from message_start), useful for
    /// spend attribution and failover visibility.
    pub routed_model: Option<String>,
}

#[derive(Clone)]
pub struct LlmClient {
    http: reqwest::Client,
    base_url: String,
}

impl LlmClient {
    pub fn new(base_url: impl Into<String>) -> Self {
        let http = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(5))
            // No overall timeout: streaming responses can be long-lived.
            .build()
            .expect("reqwest client");
        LlmClient { http, base_url: base_url.into() }
    }

    fn headers(req: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        // The gateway injects real provider credentials; these satisfy the
        // Anthropic API shape.
        req.header("x-api-key", "revenant-local")
            .header("anthropic-version", "2023-06-01")
            .header("content-type", "application/json")
    }

    /// Stream a Messages call, invoking `on_delta` for each text delta.
    pub async fn stream_message(
        &self,
        req: &MessagesRequest,
        mut on_delta: impl FnMut(&str),
    ) -> Result<StreamOutcome> {
        let url = format!("{}/v1/messages", self.base_url);
        let resp = Self::headers(self.http.post(&url))
            .json(req)
            .send()
            .await
            .with_context(|| format!("POST {url}"))?;

        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            bail!("gateway returned {status}: {}", truncate(&body, 600));
        }

        let mut outcome = StreamOutcome::default();
        let mut stream = resp.bytes_stream().eventsource();

        while let Some(event) = stream.next().await {
            let event = event.context("reading SSE stream")?;
            let data: serde_json::Value = match serde_json::from_str(&event.data) {
                Ok(v) => v,
                Err(_) => continue, // e.g. ping payloads
            };
            match event.event.as_str() {
                "message_start" => {
                    if let Some(msg) = data.get("message") {
                        outcome.routed_model =
                            msg.get("model").and_then(|m| m.as_str()).map(String::from);
                        if let Some(u) = msg.get("usage") {
                            merge_usage(&mut outcome.usage, u);
                        }
                    }
                }
                "content_block_delta" => {
                    if let Some(text) = data
                        .pointer("/delta/text")
                        .and_then(|t| t.as_str())
                    {
                        outcome.text.push_str(text);
                        on_delta(text);
                    }
                }
                "message_delta" => {
                    if let Some(u) = data.get("usage") {
                        merge_usage(&mut outcome.usage, u);
                    }
                    if let Some(reason) = data.pointer("/delta/stop_reason").and_then(|r| r.as_str())
                    {
                        outcome.stop_reason = Some(reason.to_string());
                    }
                }
                "error" => {
                    let msg = data
                        .pointer("/error/message")
                        .and_then(|m| m.as_str())
                        .unwrap_or("unknown stream error");
                    bail!("stream error from gateway: {msg}");
                }
                _ => {} // message_stop, ping, content_block_start/stop
            }
        }
        Ok(outcome)
    }

    /// Exact token count for a prospective request (compaction trigger).
    pub async fn count_tokens(
        &self,
        model: &str,
        system: Option<&str>,
        messages: &[WireMessage],
    ) -> Result<u64> {
        #[derive(Deserialize)]
        struct CountResponse {
            input_tokens: u64,
        }
        let url = format!("{}/v1/messages/count_tokens", self.base_url);
        let body = serde_json::json!({
            "model": model,
            "system": system,
            "messages": messages,
        });
        let resp = Self::headers(self.http.post(&url)).json(&body).send().await?;
        if !resp.status().is_success() {
            bail!("count_tokens returned {}", resp.status());
        }
        Ok(resp.json::<CountResponse>().await?.input_tokens)
    }

    /// Readiness probe: does the gateway answer on the LLM data path?
    pub async fn models_ready(&self) -> bool {
        let url = format!("{}/v1/models", self.base_url);
        matches!(
            self.http.get(&url).timeout(Duration::from_secs(2)).send().await,
            Ok(resp) if resp.status().is_success()
        )
    }
}

fn merge_usage(usage: &mut Usage, v: &serde_json::Value) {
    let get = |key: &str| v.get(key).and_then(|x| x.as_u64());
    if let Some(n) = get("input_tokens") {
        usage.input_tokens = n;
    }
    if let Some(n) = get("output_tokens") {
        usage.output_tokens = n;
    }
    if let Some(n) = get("cache_read_input_tokens") {
        usage.cache_read_input_tokens = n;
    }
    if let Some(n) = get("cache_creation_input_tokens") {
        usage.cache_creation_input_tokens = n;
    }
}

fn truncate(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut end = max;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}
