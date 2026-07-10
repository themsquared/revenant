//! Telegram channel: long-polling adapter with pairing, streamed replies via
//! throttled message edits, and inline-keyboard approvals.
//!
//! The Bot API client is deliberately hand-rolled (~8 methods over reqwest)
//! — a framework is dead weight for a harness targeting a Pi, and owning the
//! 429 handling matters.

use anyhow::{bail, Context, Result};
use revenant_agent::{SessionManager, SessionMsg};
use revenant_core::{Event, Tier};
use serde::Deserialize;
use serde_json::json;
use std::collections::HashMap;
use std::time::{Duration, Instant};

const CHANNEL: &str = "telegram";
/// Streaming-edit throttle: at most one editMessageText per this interval.
const EDIT_INTERVAL: Duration = Duration::from_millis(1500);
/// Don't bother editing for fewer than this many new chars.
const EDIT_MIN_DELTA: usize = 48;

// ---- thin Bot API client ----

#[derive(Clone)]
pub struct TelegramClient {
    http: reqwest::Client,
    base: String,
}

#[derive(Debug, Deserialize)]
struct ApiResponse<T> {
    ok: bool,
    result: Option<T>,
    description: Option<String>,
    parameters: Option<ApiParameters>,
}

#[derive(Debug, Deserialize)]
struct ApiParameters {
    retry_after: Option<u64>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Update {
    pub update_id: i64,
    pub message: Option<Message>,
    pub callback_query: Option<CallbackQuery>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Message {
    pub message_id: i64,
    pub chat: Chat,
    pub text: Option<String>,
    pub from: Option<User>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Chat {
    pub id: i64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct User {
    pub first_name: Option<String>,
    pub username: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct CallbackQuery {
    pub id: String,
    pub data: Option<String>,
    pub message: Option<Message>,
}

impl TelegramClient {
    pub fn new(token: &str) -> Self {
        TelegramClient {
            http: reqwest::Client::builder()
                .connect_timeout(Duration::from_secs(10))
                .build()
                .expect("reqwest client"),
            base: format!("https://api.telegram.org/bot{token}"),
        }
    }

    /// Call a Bot API method, honoring 429 retry_after exactly once.
    async fn call<T: serde::de::DeserializeOwned>(
        &self,
        method: &str,
        body: serde_json::Value,
    ) -> Result<T> {
        for attempt in 0..2 {
            let resp = self
                .http
                .post(format!("{}/{method}", self.base))
                .json(&body)
                .timeout(Duration::from_secs(70)) // > long-poll timeout
                .send()
                .await
                .with_context(|| format!("telegram {method}"))?;
            let parsed: ApiResponse<T> = resp.json().await?;
            if parsed.ok {
                return parsed.result.context("telegram: ok but no result");
            }
            let retry_after = parsed.parameters.and_then(|p| p.retry_after);
            if let (0, Some(wait)) = (attempt, retry_after) {
                tracing::warn!("telegram 429 on {method}; waiting {wait}s");
                tokio::time::sleep(Duration::from_secs(wait)).await;
                continue;
            }
            bail!(
                "telegram {method} failed: {}",
                parsed.description.unwrap_or_else(|| "unknown error".into())
            );
        }
        unreachable!()
    }

    pub async fn get_me(&self) -> Result<serde_json::Value> {
        self.call("getMe", json!({})).await
    }

    pub async fn get_updates(&self, offset: i64) -> Result<Vec<Update>> {
        self.call(
            "getUpdates",
            json!({ "offset": offset, "timeout": 50, "allowed_updates": ["message", "callback_query"] }),
        )
        .await
    }

    pub async fn send_message(&self, chat_id: i64, text: &str) -> Result<i64> {
        let msg: Message = self
            .call("sendMessage", json!({ "chat_id": chat_id, "text": text }))
            .await?;
        Ok(msg.message_id)
    }

    pub async fn send_approval(&self, chat_id: i64, text: &str, approval_id: &str) -> Result<i64> {
        let msg: Message = self
            .call(
                "sendMessage",
                json!({
                    "chat_id": chat_id,
                    "text": text,
                    "reply_markup": { "inline_keyboard": [[
                        { "text": "✅ Approve", "callback_data": format!("apr:{approval_id}:y") },
                        { "text": "❌ Deny", "callback_data": format!("apr:{approval_id}:n") },
                    ]]},
                }),
            )
            .await?;
        Ok(msg.message_id)
    }

    pub async fn edit_message(&self, chat_id: i64, message_id: i64, text: &str) -> Result<()> {
        // "message is not modified" errors are harmless — swallow them.
        let result: Result<serde_json::Value> = self
            .call(
                "editMessageText",
                json!({ "chat_id": chat_id, "message_id": message_id, "text": text }),
            )
            .await;
        match result {
            Ok(_) => Ok(()),
            Err(err) if err.to_string().contains("not modified") => Ok(()),
            Err(err) => Err(err),
        }
    }

    pub async fn typing(&self, chat_id: i64) {
        let _: Result<bool> = self
            .call("sendChatAction", json!({ "chat_id": chat_id, "action": "typing" }))
            .await;
    }

    pub async fn answer_callback(&self, callback_id: &str, text: &str) {
        let _: Result<bool> = self
            .call(
                "answerCallbackQuery",
                json!({ "callback_query_id": callback_id, "text": text }),
            )
            .await;
    }
}

// ---- adapter ----

/// Per-session streaming state: where the placeholder message lives and
/// what's been accumulated.
struct StreamState {
    chat_id: i64,
    message_id: i64,
    buffer: String,
    last_edit: Instant,
    edited_len: usize,
}

pub struct TelegramChannel {
    pub client: TelegramClient,
    pub manager: SessionManager,
    pub default_tier: Tier,
}

impl TelegramChannel {
    /// Run the adapter until the process exits: one task long-polls inbound
    /// updates, another mirrors bus events out to chats.
    pub async fn run(self) -> Result<()> {
        let me = self.client.get_me().await.context("telegram getMe (bad token?)")?;
        tracing::info!(
            "telegram connected as @{}",
            me.get("username").and_then(|u| u.as_str()).unwrap_or("?")
        );

        let outbound = OutboundMirror {
            client: self.client.clone(),
            manager: self.manager.clone(),
        };
        tokio::spawn(outbound.run());

        self.poll_loop().await
    }

    async fn poll_loop(&self) -> Result<()> {
        let runtime = self.manager.runtime().clone();
        let mut offset = 0i64;
        loop {
            let updates = match self.client.get_updates(offset).await {
                Ok(updates) => updates,
                Err(err) => {
                    tracing::warn!("telegram poll failed: {err:#}; retrying in 5s");
                    tokio::time::sleep(Duration::from_secs(5)).await;
                    continue;
                }
            };
            for update in updates {
                offset = offset.max(update.update_id + 1);
                if let Some(message) = update.message {
                    self.handle_message(&runtime, message).await;
                }
                if let Some(callback) = update.callback_query {
                    self.handle_callback(&runtime, callback).await;
                }
            }
        }
    }

    async fn handle_message(&self, runtime: &revenant_agent::AgentRuntime, message: Message) {
        let chat_id = message.chat.id;
        let peer = chat_id.to_string();
        let Some(text) = message.text else { return };
        let text = text.trim().to_string();
        if text.is_empty() {
            return;
        }

        let allowed = runtime
            .store
            .peer_allowed(CHANNEL, &peer)
            .await
            .unwrap_or(false);

        // Pairing flow for unknown chats.
        if !allowed {
            if let Some(code) = text.strip_prefix("/pair ").map(str::trim) {
                let claimed = runtime
                    .store
                    .pairing_claim(code, CHANNEL, &peer)
                    .await
                    .unwrap_or(false);
                let reply = if claimed {
                    let name = message
                        .from
                        .and_then(|u| u.username.or(u.first_name))
                        .unwrap_or_default();
                    tracing::info!("telegram paired chat {chat_id} ({name})");
                    "Paired. I'm listening — say anything."
                } else {
                    "That code is invalid or expired. Mint a fresh one with `revenant pair`."
                };
                let _ = self.client.send_message(chat_id, reply).await;
            } else {
                let _ = self
                    .client
                    .send_message(
                        chat_id,
                        "This agent is private. Pair with a code from `revenant pair` on the host: /pair <code>",
                    )
                    .await;
            }
            return;
        }

        // Paired: route to the session actor.
        match runtime.store.ensure_session(CHANNEL, &peer, "chat").await {
            Ok(session_id) => {
                self.client.typing(chat_id).await;
                if let Err(err) = self
                    .manager
                    .submit(session_id, SessionMsg::UserInput { content: text, tier: self.default_tier })
                    .await
                {
                    tracing::error!("telegram -> session submit failed: {err:#}");
                    let _ = self
                        .client
                        .send_message(chat_id, "something broke routing that — try again")
                        .await;
                }
            }
            Err(err) => tracing::error!("ensure_session failed: {err:#}"),
        }
    }

    async fn handle_callback(&self, runtime: &revenant_agent::AgentRuntime, cb: CallbackQuery) {
        let Some(data) = cb.data.as_deref() else { return };
        // apr:<approval_id>:<y|n>
        let mut parts = data.splitn(3, ':');
        if parts.next() != Some("apr") {
            return;
        }
        let (Some(approval_id), Some(verdict)) = (parts.next(), parts.next()) else { return };
        // Only paired chats may approve.
        let peer = cb
            .message
            .as_ref()
            .map(|m| m.chat.id.to_string())
            .unwrap_or_default();
        if !runtime.store.peer_allowed(CHANNEL, &peer).await.unwrap_or(false) {
            self.client.answer_callback(&cb.id, "not paired").await;
            return;
        }
        let approve = verdict == "y";
        match runtime.approvals.resolve(approval_id, approve, "telegram").await {
            Ok(true) => {
                self.client
                    .answer_callback(&cb.id, if approve { "approved" } else { "denied" })
                    .await;
                if let Some(message) = &cb.message {
                    let stamp = if approve { "✅ approved" } else { "❌ denied" };
                    let original = message.text.clone().unwrap_or_default();
                    let _ = self
                        .client
                        .edit_message(message.chat.id, message.message_id, &format!("{original}\n\n{stamp} via Telegram"))
                        .await;
                }
            }
            Ok(false) => self.client.answer_callback(&cb.id, "already resolved").await,
            Err(err) => {
                tracing::error!("approval resolve failed: {err:#}");
                self.client.answer_callback(&cb.id, "error").await;
            }
        }
    }
}

/// Mirrors bus events out to Telegram: streamed replies via throttled edits,
/// approval prompts to every paired chat.
struct OutboundMirror {
    client: TelegramClient,
    manager: SessionManager,
}

impl OutboundMirror {
    async fn run(self) {
        let runtime = self.manager.runtime().clone();
        let mut rx = runtime.events.subscribe();
        // session_id -> stream state, for telegram-bound sessions only.
        let mut streams: HashMap<i64, StreamState> = HashMap::new();
        // session_id -> chat_id memo (telegram sessions).
        let mut chats: HashMap<i64, i64> = HashMap::new();

        loop {
            let event = match rx.recv().await {
                Ok(event) => event,
                Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                    tracing::warn!("telegram mirror lagged {n} events");
                    continue;
                }
                Err(_) => break,
            };

            match event {
                // A new turn NEVER inherits the previous turn's message.
                // Belt-and-suspenders against missed TurnCompleted events
                // (e.g. broadcast lag): drop any stale stream state; the old
                // message keeps whatever it last showed.
                Event::TurnStarted { session_id } => {
                    if streams.remove(&session_id).is_some() {
                        tracing::debug!(
                            session_id,
                            "dropped stale stream state at turn start"
                        );
                    }
                }
                Event::TurnDelta { session_id, text } => {
                    let Some(chat_id) = self.chat_for(&runtime, &mut chats, session_id).await
                    else {
                        continue;
                    };
                    let state = match streams.get_mut(&session_id) {
                        Some(state) => state,
                        None => {
                            let Ok(message_id) = self.client.send_message(chat_id, "…").await
                            else {
                                continue;
                            };
                            streams.insert(
                                session_id,
                                StreamState {
                                    chat_id,
                                    message_id,
                                    buffer: String::new(),
                                    last_edit: Instant::now(),
                                    edited_len: 0,
                                },
                            );
                            streams.get_mut(&session_id).unwrap()
                        }
                    };
                    state.buffer.push_str(&text);
                    let grown = state.buffer.len().saturating_sub(state.edited_len);
                    if state.last_edit.elapsed() >= EDIT_INTERVAL && grown >= EDIT_MIN_DELTA {
                        let preview = format!("{}▌", clip(&state.buffer, 3900));
                        if self
                            .client
                            .edit_message(state.chat_id, state.message_id, &preview)
                            .await
                            .is_ok()
                        {
                            state.last_edit = Instant::now();
                            state.edited_len = state.buffer.len();
                        }
                    }
                }
                Event::TurnCompleted { session_id, text, .. } => {
                    if let Some(state) = streams.remove(&session_id) {
                        let final_text =
                            if text.is_empty() { state.buffer.clone() } else { text.clone() };
                        let _ = self
                            .client
                            .edit_message(state.chat_id, state.message_id, &clip(&final_text, 4000))
                            .await;
                    } else if let Some(chat_id) =
                        self.chat_for(&runtime, &mut chats, session_id).await
                    {
                        // Turn produced no deltas we saw (e.g. mirror started
                        // mid-turn) — send the final text outright.
                        if !text.is_empty() {
                            let _ = self.client.send_message(chat_id, &clip(&text, 4000)).await;
                        }
                    }
                }
                Event::TurnFailed { session_id, error } => {
                    if let Some(state) = streams.remove(&session_id) {
                        let _ = self
                            .client
                            .edit_message(state.chat_id, state.message_id, &format!("⚠️ {error}"))
                            .await;
                    }
                }
                Event::ToolStarted { session_id, summary, .. } => {
                    if let Some(chat_id) = self.chat_for(&runtime, &mut chats, session_id).await {
                        self.client.typing(chat_id).await;
                        let _ = summary; // keep the chat clean; tools show in TUI/web
                    }
                }
                // Approvals go to EVERY paired chat, whatever session they
                // came from — the owner's pocket is the point.
                Event::ApprovalCreated { id, summary, .. } => {
                    let peers = runtime.store.peers_list(CHANNEL).await.unwrap_or_default();
                    for peer in peers {
                        if let Ok(chat_id) = peer.parse::<i64>() {
                            let _ = self
                                .client
                                .send_approval(chat_id, &format!("⚠️ Approval needed:\n{summary}"), &id)
                                .await;
                        }
                    }
                }
                _ => {}
            }
        }
    }

    /// Resolve a session to its Telegram chat id (only telegram-channel
    /// sessions map; others are ignored by the mirror).
    async fn chat_for(
        &self,
        runtime: &revenant_agent::AgentRuntime,
        memo: &mut HashMap<i64, i64>,
        session_id: i64,
    ) -> Option<i64> {
        if let Some(&chat_id) = memo.get(&session_id) {
            return if chat_id == 0 { None } else { Some(chat_id) };
        }
        let row = runtime
            .store
            .with(move |conn| {
                use rusqlite::OptionalExtension;
                conn.query_row(
                    "SELECT channel, peer FROM sessions WHERE id = ?1",
                    [session_id],
                    |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)),
                )
                .optional()
            })
            .await
            .ok()
            .flatten();
        let chat_id = match row {
            Some((channel, peer)) if channel == CHANNEL => peer.parse::<i64>().unwrap_or(0),
            _ => 0,
        };
        memo.insert(session_id, chat_id);
        if chat_id == 0 {
            None
        } else {
            Some(chat_id)
        }
    }
}

fn clip(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut end = max;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &s[..end])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clip_respects_char_boundaries() {
        assert_eq!(clip("hello", 10), "hello");
        let clipped = clip("héllo wörld", 6);
        assert!(clipped.ends_with('…'));
    }
}
