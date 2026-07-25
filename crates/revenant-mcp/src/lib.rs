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

/// What the owner decided when a server asked for a value. Mirrors the three
/// actions MCP elicitation defines; the security semantics live in
/// revenant-security (this crate deliberately knows nothing about the broker).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ElicitReply {
    Accept(Value),
    Decline,
    Cancel,
}

impl ElicitReply {
    /// The MCP `elicitation/create` result shape. `content` is present only on
    /// accept — a declined or cancelled request must carry no data at all.
    fn to_result(&self) -> Value {
        match self {
            ElicitReply::Accept(content) => json!({ "action": "accept", "content": content }),
            ElicitReply::Decline => json!({ "action": "decline" }),
            ElicitReply::Cancel => json!({ "action": "cancel" }),
        }
    }
}

/// Asked when a server wants a value from the owner mid-tool-call. Implemented
/// outside this crate (the daemon wires it to the approval broker) so the wire
/// layer stays free of policy.
#[async_trait::async_trait]
pub trait ElicitationHandler: Send + Sync {
    /// `message` is the server's human-facing prompt; `schema` its requested
    /// shape. Returning [`ElicitReply::Cancel`] is always a safe default.
    async fn elicit(&self, message: &str, schema: &Value) -> ElicitReply;
}

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
    /// Set to accept `elicitation/create`. Absent = the capability is NOT
    /// declared and such requests are refused, so a server never believes it
    /// can collect input through us when nothing is listening.
    elicitation: Option<Arc<dyn ElicitationHandler>>,
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
            elicitation: None,
        })
    }

    /// Same client, but willing to collect input from the owner on a server's
    /// behalf. Declares the `elicitation` capability at initialize.
    pub fn with_elicitation(
        endpoint: impl Into<String>,
        handler: Arc<dyn ElicitationHandler>,
    ) -> Arc<Self> {
        let mut client = Arc::try_unwrap(Self::new(endpoint)).ok().expect("fresh Arc");
        client.elicitation = Some(handler);
        Arc::new(client)
    }

    fn id(&self) -> u64 {
        self.next_id.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    }

    /// POST a JSON-RPC request; returns the parsed `result` (handles both a
    /// plain JSON body and an SSE `data:` framing). `session` is attached and
    /// captured from the response header.
    async fn rpc(&self, method: &str, params: Value, session: Option<&str>) -> Result<(Value, Option<String>)> {
        let req_id = self.id();
        let body = json!({ "jsonrpc": "2.0", "id": req_id, "method": method, "params": params });
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
        let frames = parse_frames(&text);
        if frames.is_empty() {
            bail!("no JSON-RPC payload in MCP {method} response");
        }

        // Serve anything the server asked of us in this stream BEFORE looking
        // for our own reply — a server that blocks on an elicitation will not
        // send the reply until we answer.
        let session_for_replies = new_session.as_deref().or(session);
        let mut answer: Option<Value> = None;
        for frame in &frames {
            // NB: `srv_id` is the SERVER's request id, distinct from our
            // `req_id` below — they can collide numerically, which is exactly
            // why `replies_to` also requires the absence of a `method`.
            if let Some((srv_id, srv_method)) = inbound_request(frame) {
                self.serve_inbound(&srv_method, frame, srv_id, session_for_replies).await;
            } else if replies_to(frame, req_id) {
                answer = Some(frame.clone());
            }
        }

        let value = answer.with_context(|| {
            format!("MCP {method}: no reply matching request id {req_id} in {} frame(s)", frames.len())
        })?;
        if let Some(err) = value.get("error") {
            bail!("MCP {method} error: {}", err);
        }
        Ok((value.get("result").cloned().unwrap_or(Value::Null), new_session))
    }

    /// Answer a server-initiated request. Every branch replies — an unanswered
    /// request leaves the server blocked, so "unsupported" is an explicit
    /// JSON-RPC error, never silence.
    async fn serve_inbound(
        &self,
        method: &str,
        frame: &Value,
        id: Value,
        session: Option<&str>,
    ) {
        let params = frame.get("params").cloned().unwrap_or(Value::Null);
        let outcome: std::result::Result<Value, (i64, String)> = match method {
            "ping" => Ok(json!({})),
            "elicitation/create" => match &self.elicitation {
                Some(handler) => {
                    let message =
                        params.get("message").and_then(|m| m.as_str()).unwrap_or_default();
                    let schema = params
                        .get("requestedSchema")
                        .cloned()
                        .unwrap_or_else(|| json!({ "type": "object" }));
                    // The owner decides. A handler that declines/cancels is a
                    // normal, expected outcome — not an error to the server.
                    Ok(handler.elicit(message, &schema).await.to_result())
                }
                // Capability not declared ⇒ a conforming server should not ask.
                None => Err((-32601, "elicitation not supported by this client".into())),
            },
            // We declare no roots and no sampling capability, so both are
            // refused rather than half-answered.
            "roots/list" => Err((-32601, "roots not supported by this client".into())),
            "sampling/createMessage" => {
                Err((-32601, "sampling not supported by this client".into()))
            }
            other => Err((-32601, format!("method not found: {other}"))),
        };

        let reply = match outcome {
            Ok(result) => json!({ "jsonrpc": "2.0", "id": id, "result": result }),
            Err((code, message)) => {
                tracing::debug!("mcp: refusing server request {method}: {message}");
                json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
            }
        };

        let mut req = self
            .http
            .post(&self.endpoint)
            .header("content-type", "application/json")
            .header("accept", "application/json, text/event-stream");
        if let Some(sid) = session {
            req = req.header("mcp-session-id", sid);
        }
        if let Err(err) = req.json(&reply).send().await {
            tracing::warn!("mcp: failed to answer server request {method}: {err}");
        }
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
                    // Declare ONLY what we actually serve. Claiming elicitation
                    // without a handler would invite requests we then refuse.
                    "capabilities": if self.elicitation.is_some() {
                        json!({ "elicitation": {} })
                    } else {
                        json!({})
                    },
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

/// Every JSON-RPC frame in a response body, IN ORDER — a bare JSON body is one
/// frame, an SSE stream is one frame per `data:` line.
///
/// The old version returned only the last frame that happened to parse, which
/// silently discarded anything the server sent alongside its reply. A
/// server-initiated request (`elicitation/create`, `ping`, …) arriving in the
/// same stream was therefore either dropped or mistaken for the reply, and the
/// server sat waiting for a response that would never come. Frames have to be
/// separated and matched by id, not guessed at by position.
fn parse_frames(text: &str) -> Vec<Value> {
    let trimmed = text.trim_start();
    if trimmed.starts_with('{') {
        return serde_json::from_str(trimmed).map(|v| vec![v]).unwrap_or_default();
    }
    text.lines()
        .filter_map(|line| line.strip_prefix("data:"))
        .filter_map(|data| serde_json::from_str::<Value>(data.trim()).ok())
        .collect()
}

/// Is this frame a request the SERVER is making of US? Requests carry both a
/// `method` and an `id`; a notification has a method but no id (nothing to
/// answer), and a reply to us has an id but no method.
fn inbound_request(frame: &Value) -> Option<(Value, String)> {
    let method = frame.get("method")?.as_str()?.to_string();
    let id = frame.get("id")?.clone();
    Some((id, method))
}

/// Does this frame answer the request we sent with `id`?
fn replies_to(frame: &Value, id: u64) -> bool {
    frame.get("method").is_none()
        && frame.get("id").and_then(|v| v.as_u64()) == Some(id)
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

#[cfg(test)]
mod tests {
    use super::*;

    /// The bug this replaced: the old parser returned only the LAST frame that
    /// happened to parse, so anything the server sent alongside its reply was
    /// dropped — including a request it was blocking on.
    #[test]
    fn every_frame_is_parsed_in_order() {
        let body = "event: message\n\
                    data: {\"jsonrpc\":\"2.0\",\"id\":7,\"method\":\"elicitation/create\",\"params\":{\"message\":\"Region?\"}}\n\
                    \n\
                    event: message\n\
                    data: {\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"content\":[]}}\n";
        let frames = parse_frames(body);
        assert_eq!(frames.len(), 2, "both frames must survive parsing");
        assert_eq!(frames[0]["method"], "elicitation/create", "order preserved");
        assert!(frames[1].get("result").is_some());

        // A bare JSON body is still a single frame.
        let single = parse_frames("{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{}}");
        assert_eq!(single.len(), 1);
        // Garbage yields nothing rather than panicking.
        assert!(parse_frames("not json at all").is_empty());
    }

    /// Requests, notifications and replies must be told apart by SHAPE, never by
    /// position: a request has method+id, a notification has method and no id, a
    /// reply has id and no method.
    #[test]
    fn frames_are_classified_by_shape() {
        let request: Value =
            serde_json::from_str("{\"id\":7,\"method\":\"ping\"}").unwrap();
        let notification: Value =
            serde_json::from_str("{\"method\":\"notifications/progress\"}").unwrap();
        let reply: Value = serde_json::from_str("{\"id\":1,\"result\":{}}").unwrap();

        assert_eq!(inbound_request(&request).map(|(_, m)| m), Some("ping".to_string()));
        assert!(inbound_request(&notification).is_none(), "no id ⇒ nothing to answer");
        assert!(inbound_request(&reply).is_none(), "no method ⇒ not a request");
    }

    /// The adversarial case that makes id-matching alone insufficient: a server
    /// request can carry the SAME id number as our in-flight request. Only the
    /// presence of `method` distinguishes them, so a server asking with id 1
    /// must never be consumed as the reply to our request 1.
    #[test]
    fn a_server_request_is_never_mistaken_for_our_reply() {
        let colliding_request: Value =
            serde_json::from_str("{\"id\":1,\"method\":\"elicitation/create\"}").unwrap();
        let our_reply: Value = serde_json::from_str("{\"id\":1,\"result\":{}}").unwrap();

        assert!(!replies_to(&colliding_request, 1), "same id, but it is a REQUEST");
        assert!(replies_to(&our_reply, 1));
        // And a reply to a different request is not ours.
        assert!(!replies_to(&our_reply, 2));
    }

    /// Declining or cancelling must carry no data. A server must not be able to
    /// read a refusal as an empty-but-present value.
    #[test]
    fn only_accept_carries_content() {
        let accepted = ElicitReply::Accept(json!({ "region": "us-east-1" })).to_result();
        assert_eq!(accepted["action"], "accept");
        assert_eq!(accepted["content"]["region"], "us-east-1");

        for refusal in [ElicitReply::Decline, ElicitReply::Cancel] {
            let out = refusal.to_result();
            assert!(out.get("content").is_none(), "{out} must not carry content");
        }
        assert_eq!(ElicitReply::Decline.to_result()["action"], "decline");
        assert_eq!(ElicitReply::Cancel.to_result()["action"], "cancel");
    }
}
