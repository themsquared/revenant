//! revenant-mcp: a thin MCP client over the gateway's MCP multiplex endpoint.
//!
//! Speaks MCP streamable-HTTP JSON-RPC directly (initialize → tools/list →
//! tools/call), no SDK — same hand-rolled style as the LLM and Telegram
//! clients. One session, serialized calls, re-initialized on failure. Every
//! configured MCP server's tools become agent tools through here.

use anyhow::{bail, Context, Result};
use revenant_core::ToolSpec;
use serde_json::{json, Value};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;

/// A discovered MCP tool, mapped toward our ToolSpec.
#[derive(Debug, Clone)]
pub struct McpTool {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
}

impl McpTool {
    pub fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: self.name.clone(),
            description: self.description.clone(),
            input_schema: self.input_schema.clone(),
        }
    }
}

pub struct McpClient {
    http: reqwest::Client,
    endpoint: String,
    session: Mutex<Option<String>>,
    next_id: std::sync::atomic::AtomicU64,
}

impl McpClient {
    pub fn new(endpoint: impl Into<String>) -> Arc<Self> {
        Arc::new(McpClient {
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(120))
                .build()
                .expect("reqwest client"),
            endpoint: endpoint.into(),
            session: Mutex::new(None),
            next_id: std::sync::atomic::AtomicU64::new(1),
        })
    }

    fn id(&self) -> u64 {
        self.next_id.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    }

    /// POST a JSON-RPC request; returns the parsed `result` (handles both a
    /// plain JSON body and an SSE `data:` framing). `session` is attached and
    /// captured from the response header.
    async fn rpc(&self, method: &str, params: Value, session: Option<&str>) -> Result<(Value, Option<String>)> {
        let body = json!({ "jsonrpc": "2.0", "id": self.id(), "method": method, "params": params });
        let mut req = self
            .http
            .post(&self.endpoint)
            .header("content-type", "application/json")
            .header("accept", "application/json, text/event-stream");
        if let Some(sid) = session {
            req = req.header("mcp-session-id", sid);
        }
        let resp = req.json(&body).send().await.with_context(|| format!("MCP {method}"))?;
        let status = resp.status();
        let new_session = resp
            .headers()
            .get("mcp-session-id")
            .and_then(|v| v.to_str().ok())
            .map(String::from);
        if !status.is_success() {
            let text = resp.text().await.unwrap_or_default();
            bail!("MCP {method} returned {status}: {}", truncate(&text, 300));
        }
        let text = resp.text().await?;
        let value = parse_jsonrpc(&text).with_context(|| format!("parsing MCP {method} response"))?;
        if let Some(err) = value.get("error") {
            bail!("MCP {method} error: {}", err);
        }
        Ok((value.get("result").cloned().unwrap_or(Value::Null), new_session))
    }

    /// A notification (no id, no response expected).
    async fn notify(&self, method: &str, session: &str) -> Result<()> {
        let body = json!({ "jsonrpc": "2.0", "method": method });
        let _ = self
            .http
            .post(&self.endpoint)
            .header("content-type", "application/json")
            .header("accept", "application/json, text/event-stream")
            .header("mcp-session-id", session)
            .json(&body)
            .send()
            .await;
        Ok(())
    }

    /// Establish (or re-establish) a session. Returns the session id.
    async fn ensure_session(&self) -> Result<String> {
        {
            let guard = self.session.lock().await;
            if let Some(sid) = guard.as_ref() {
                return Ok(sid.clone());
            }
        }
        let (_, sid) = self
            .rpc(
                "initialize",
                json!({
                    "protocolVersion": "2025-06-18",
                    "capabilities": {},
                    "clientInfo": { "name": "revenant", "version": env!("CARGO_PKG_VERSION") }
                }),
                None,
            )
            .await?;
        let sid = sid.context("MCP server returned no session id")?;
        self.notify("notifications/initialized", &sid).await?;
        *self.session.lock().await = Some(sid.clone());
        Ok(sid)
    }

    /// Discover all tools the gateway multiplex exposes.
    pub async fn list_tools(&self) -> Result<Vec<McpTool>> {
        let sid = self.ensure_session().await?;
        let (result, _) = self.rpc("tools/list", json!({}), Some(&sid)).await?;
        let tools = result
            .get("tools")
            .and_then(|t| t.as_array())
            .cloned()
            .unwrap_or_default();
        Ok(tools
            .into_iter()
            .filter_map(|t| {
                Some(McpTool {
                    name: t.get("name")?.as_str()?.to_string(),
                    description: t
                        .get("description")
                        .and_then(|d| d.as_str())
                        .unwrap_or("")
                        .to_string(),
                    input_schema: t
                        .get("inputSchema")
                        .cloned()
                        .unwrap_or_else(|| json!({ "type": "object" })),
                })
            })
            .collect())
    }

    /// Call a tool; returns its text content (concatenated). Re-inits the
    /// session once on failure (the gateway may have expired it).
    pub async fn call_tool(&self, name: &str, args: Value) -> Result<String> {
        match self.try_call(name, args.clone()).await {
            Ok(text) => Ok(text),
            Err(_) => {
                *self.session.lock().await = None; // force re-init
                self.try_call(name, args).await
            }
        }
    }

    async fn try_call(&self, name: &str, args: Value) -> Result<String> {
        let sid = self.ensure_session().await?;
        let (result, _) = self
            .rpc("tools/call", json!({ "name": name, "arguments": args }), Some(&sid))
            .await?;
        // MCP returns content: [{type:"text", text:"…"}, …]; may set isError.
        let is_error = result.get("isError").and_then(|v| v.as_bool()).unwrap_or(false);
        let text = result
            .get("content")
            .and_then(|c| c.as_array())
            .map(|blocks| {
                blocks
                    .iter()
                    .filter_map(|b| b.get("text").and_then(|t| t.as_str()))
                    .collect::<Vec<_>>()
                    .join("\n")
            })
            .unwrap_or_default();
        if is_error {
            bail!("tool reported an error: {text}");
        }
        Ok(if text.is_empty() { "(no output)".into() } else { text })
    }
}

/// Extract the JSON-RPC object from a response that may be a bare JSON body or
/// an SSE stream (`event: message\ndata: {…}`).
fn parse_jsonrpc(text: &str) -> Result<Value> {
    let trimmed = text.trim_start();
    if trimmed.starts_with('{') {
        return Ok(serde_json::from_str(trimmed)?);
    }
    // SSE: take the last non-empty `data:` line that parses as an object.
    for line in text.lines().rev() {
        if let Some(data) = line.strip_prefix("data:") {
            let data = data.trim();
            if let Ok(v) = serde_json::from_str::<Value>(data) {
                return Ok(v);
            }
        }
    }
    bail!("no JSON-RPC payload in response")
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
