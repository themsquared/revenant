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
    /// Plain string, or an array of system blocks carrying `cache_control`
    /// breakpoints (see `system_with_cache`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub system: Option<serde_json::Value>,
    pub messages: Vec<WireMessage>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub tools: Vec<revenant_core::ToolSpec>,
    /// e.g. `{"type":"tool","name":"record_memory"}` to force an extraction
    /// tool call (structured output).
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub tool_choice: Option<serde_json::Value>,
    pub stream: bool,
}

/// Build a system value with a cache breakpoint after the stable prefix.
/// The provider caches tools + the stable block; the dynamic tail (per-turn
/// retrieved memories etc.) stays uncached by design.
pub fn system_with_cache(stable: &str, dynamic: Option<&str>) -> serde_json::Value {
    let mut blocks = vec![serde_json::json!({
        "type": "text",
        "text": stable,
        "cache_control": { "type": "ephemeral" },
    })];
    if let Some(dynamic) = dynamic.filter(|d| !d.is_empty()) {
        blocks.push(serde_json::json!({ "type": "text", "text": dynamic }));
    }
    serde_json::Value::Array(blocks)
}

#[derive(Debug, Clone, Default)]
pub struct StreamOutcome {
    /// Concatenated text of all text blocks (for display/persist shortcuts).
    pub text: String,
    /// The full assistant content, including tool_use blocks in order.
    pub content: Vec<ContentBlock>,
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

    /// Minimal liveness + credit check: a 1-token request against `model`.
    /// `Ok(())` if the provider accepts it; a humanized error (credit / auth /
    /// rate / overload) otherwise. Used by `revenant doctor`.
    pub async fn ping(&self, model: &str) -> Result<()> {
        let url = format!("{}/v1/messages", self.base_url);
        let req = serde_json::json!({
            "model": model, "max_tokens": 1,
            "messages": [{ "role": "user", "content": "hi" }]
        });
        let resp = Self::headers(self.http.post(&url))
            .json(&req)
            .send()
            .await
            .with_context(|| format!("POST {url}"))?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            bail!("{}", humanize_gateway_error(status.as_u16(), &body));
        }
        Ok(())
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
            bail!("{}", humanize_gateway_error(status.as_u16(), &body));
        }

        let mut outcome = StreamOutcome::default();
        let mut stream = resp.bytes_stream().eventsource();

        // In-progress content block accumulator.
        enum Pending {
            Text(String),
            ToolUse { id: String, name: String, json: String },
        }
        let mut pending: Option<Pending> = None;

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
                "content_block_start" => {
                    let block = data.get("content_block");
                    match block.and_then(|b| b.get("type")).and_then(|t| t.as_str()) {
                        Some("tool_use") => {
                            pending = Some(Pending::ToolUse {
                                id: block
                                    .and_then(|b| b.get("id"))
                                    .and_then(|v| v.as_str())
                                    .unwrap_or_default()
                                    .to_string(),
                                name: block
                                    .and_then(|b| b.get("name"))
                                    .and_then(|v| v.as_str())
                                    .unwrap_or_default()
                                    .to_string(),
                                json: String::new(),
                            });
                        }
                        _ => pending = Some(Pending::Text(String::new())),
                    }
                }
                "content_block_delta" => match data.pointer("/delta/type").and_then(|t| t.as_str())
                {
                    Some("text_delta") => {
                        if let Some(text) = data.pointer("/delta/text").and_then(|t| t.as_str()) {
                            outcome.text.push_str(text);
                            if let Some(Pending::Text(buf)) = &mut pending {
                                buf.push_str(text);
                            }
                            on_delta(text);
                        }
                    }
                    Some("input_json_delta") => {
                        if let Some(part) =
                            data.pointer("/delta/partial_json").and_then(|t| t.as_str())
                        {
                            if let Some(Pending::ToolUse { json, .. }) = &mut pending {
                                json.push_str(part);
                            }
                        }
                    }
                    _ => {}
                },
                "content_block_stop" => {
                    match pending.take() {
                        Some(Pending::Text(text)) if !text.is_empty() => {
                            outcome.content.push(ContentBlock::Text { text, cache_control: None });
                        }
                        Some(Pending::ToolUse { id, name, json }) => {
                            let input: serde_json::Value = if json.trim().is_empty() {
                                serde_json::json!({})
                            } else {
                                serde_json::from_str(&json).with_context(|| {
                                    format!("tool_use input for '{name}' is not valid JSON")
                                })?
                            };
                            outcome.content.push(ContentBlock::ToolUse { id, name, input });
                        }
                        _ => {}
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
                _ => {} // message_stop, ping
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

    /// OpenAI-shape embeddings via the gateway (`POST /v1/embeddings`).
    pub async fn embeddings(&self, model: &str, inputs: &[String]) -> Result<Vec<Vec<f32>>> {
        #[derive(Deserialize)]
        struct EmbeddingItem {
            embedding: Vec<f32>,
        }
        #[derive(Deserialize)]
        struct EmbeddingsResponse {
            data: Vec<EmbeddingItem>,
        }
        let url = format!("{}/v1/embeddings", self.base_url);
        let resp = Self::headers(self.http.post(&url))
            .json(&serde_json::json!({ "model": model, "input": inputs }))
            .send()
            .await
            .with_context(|| format!("POST {url}"))?;
        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            bail!("embeddings returned {status}: {}", truncate(&body, 300));
        }
        Ok(resp
            .json::<EmbeddingsResponse>()
            .await?
            .data
            .into_iter()
            .map(|item| item.embedding)
            .collect())
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

/// Turn a provider/gateway HTTP error into a plain-English, actionable message.
/// This is what a user sees in chat/Telegram when a turn fails — no raw JSON,
/// just what's wrong and how to fix it. Falls back to the trimmed body.
pub fn humanize_gateway_error(status: u16, body: &str) -> String {
    let b = body.to_lowercase();
    if b.contains("credit balance is too low") || b.contains("insufficient") && b.contains("credit")
    {
        return "Out of API credits. Add credits in your provider console — for Anthropic: \
console.anthropic.com → Settings → Billing (this is separate from a Claude subscription's \
\"extra usage\"). Or switch a tier to a provider that has credit."
            .to_string();
    }
    if status == 401 || b.contains("invalid x-api-key") || b.contains("authentication") {
        return "The provider rejected the API key. Check the key in ~/.revenant/secrets.env \
(and that it's for the right provider), then restart.".to_string();
    }
    if status == 429 || b.contains("rate limit") || b.contains("rate_limit") {
        return "Rate-limited by the provider. It'll retry automatically; if it keeps happening, \
slow down or raise your account's rate limit.".to_string();
    }
    if b.contains("overloaded") || status == 529 {
        return "The model provider is overloaded right now. Try again in a moment — a multi-target \
tier will fail over automatically.".to_string();
    }
    if status == 404 && b.contains("model") {
        return "The configured model wasn't found at the provider. Check the model name in your \
tier config (`revenant render` shows what's sent).".to_string();
    }
    // Unknown: keep it short and honest.
    format!("Provider error {status}: {}", truncate(body, 400))
}

#[cfg(test)]
mod humanize_tests {
    use super::*;

    #[test]
    fn credit_error_is_actionable() {
        let msg = humanize_gateway_error(
            400,
            r#"{"error":{"message":"Your credit balance is too low to access the Anthropic API."}}"#,
        );
        assert!(msg.contains("Out of API credits"));
        assert!(msg.contains("Billing"));
        assert!(!msg.contains("{"), "should not leak raw JSON");
    }

    #[test]
    fn auth_and_rate_and_unknown() {
        assert!(humanize_gateway_error(401, "").contains("rejected the API key"));
        assert!(humanize_gateway_error(429, "").contains("Rate-limited"));
        assert!(humanize_gateway_error(500, "boom").contains("Provider error 500"));
    }
}
