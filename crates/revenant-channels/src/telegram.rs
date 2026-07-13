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
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex as AsyncMutex;

const CHANNEL: &str = "telegram";
/// Keep the "typing…" indicator alive while a turn runs (Telegram's action
/// lasts ~5s, so refresh a bit under that).
const TYPING_REFRESH: Duration = Duration::from_secs(4);
/// Telegram's hard per-message limit; longer replies are split.
const TG_MAX: usize = 4000;

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

    /// Send pre-rendered Telegram HTML (parse_mode=HTML). Callers fall back to
    /// `send_message` (plain) if this errors on malformed markup.
    pub async fn send_html(&self, chat_id: i64, html: &str) -> Result<i64> {
        let msg: Message = self
            .call(
                "sendMessage",
                json!({
                    "chat_id": chat_id,
                    "text": html,
                    "parse_mode": "HTML",
                    "disable_web_page_preview": true
                }),
            )
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
                    "reply_markup": { "inline_keyboard": [
                        [
                            { "text": "✅ Approve once", "callback_data": format!("apr:{approval_id}:y") },
                            { "text": "❌ Deny", "callback_data": format!("apr:{approval_id}:n") },
                        ],
                        [
                            { "text": "✅✅ Run all for this task", "callback_data": format!("apr:{approval_id}:a") },
                        ],
                    ]},
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

#[derive(Clone)]
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
            // Handle each update on its own task so the poll loop keeps
            // fetching — one slow turn (or a slow store/typing call) can never
            // stall inbound processing or make the bot look hung.
            for update in updates {
                offset = offset.max(update.update_id + 1);
                let this = self.clone();
                tokio::spawn(async move {
                    let runtime = this.manager.runtime().clone();
                    if let Some(message) = update.message {
                        this.handle_message(&runtime, message).await;
                    }
                    if let Some(callback) = update.callback_query {
                        this.handle_callback(&runtime, callback).await;
                    }
                });
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

        // /help and /start work in any state so commands are discoverable.
        if text == "/help" || text == "/start" {
            let msg = if allowed {
                "Commands:\n\
                 /stop — stop the turn I'm running\n\
                 /persona <name|off> — switch my voice (no name lists them)\n\
                 /help — this list\n\n\
                 Otherwise just talk to me — I stream replies as I go, and you can send more mid-turn to steer or queue it."
            } else {
                "This agent is private. Pair with a code from `revenant pair` on the host:\n/pair <code>"
            };
            let _ = self.client.send_message(chat_id, msg).await;
            return;
        }

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
                // /stop — halt the running turn for this chat.
                if text.trim() == "/stop" {
                    let reply = if runtime.cancel(session_id) {
                        "🛑 stopping…"
                    } else {
                        "nothing running to stop"
                    };
                    let _ = self.client.send_message(chat_id, reply).await;
                    return;
                }
                // /persona <name|off> — switch the voice for this chat.
                if let Some(rest) = text.strip_prefix("/persona") {
                    let choice = rest.trim();
                    let persona = if choice.is_empty() || choice == "off" || choice == "default" {
                        None
                    } else {
                        Some(choice)
                    };
                    let _ = runtime.store.session_set_persona(session_id, persona).await;
                    let names: Vec<String> =
                        runtime.personalities.list().into_iter().map(|p| p.name).collect();
                    let reply = match persona {
                        Some(p) => format!("voice → {p}"),
                        None => format!("voice → default\navailable: {}", names.join(", ")),
                    };
                    let _ = self.client.send_message(chat_id, &reply).await;
                    return;
                }
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
        // y = approve once · a = approve + grant the rest of this task · n = deny
        let approve = verdict == "y" || verdict == "a";
        let grant = verdict == "a";
        match runtime.approvals.resolve_scoped(approval_id, approve, grant, "telegram").await {
            Ok(true) => {
                let toast = match (approve, grant) {
                    (true, true) => "approved — running the rest of this task without asking",
                    (true, false) => "approved",
                    _ => "denied",
                };
                self.client.answer_callback(&cb.id, toast).await;
                if let Some(message) = &cb.message {
                    let stamp = match (approve, grant) {
                        (true, true) => "✅✅ approved for this task",
                        (true, false) => "✅ approved",
                        _ => "❌ denied",
                    };
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
        // session_id -> chat_id memo (telegram sessions).
        let mut chats: HashMap<i64, i64> = HashMap::new();
        // Sessions with an in-flight turn → their chat. Shared with a ticker
        // that keeps the native "typing…" indicator alive, so a slow turn still
        // feels live WITHOUT a mutating placeholder message.
        let active: Arc<AsyncMutex<HashMap<i64, i64>>> = Arc::new(AsyncMutex::new(HashMap::new()));
        // Delta accumulator — a fallback only, used if TurnCompleted arrives
        // with no text. We no longer edit a live message.
        let mut buffers: HashMap<i64, String> = HashMap::new();

        {
            let active = active.clone();
            let client = self.client.clone();
            tokio::spawn(async move {
                let mut tick = tokio::time::interval(TYPING_REFRESH);
                loop {
                    tick.tick().await;
                    let targets: Vec<i64> = active.lock().await.values().copied().collect();
                    for chat_id in targets {
                        client.typing(chat_id).await;
                    }
                }
            });
        }

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
                // A turn begins: show "typing…" (kept alive by the ticker) and
                // mark the session active. NO placeholder message — the reply
                // arrives as one clean message when ready, like a real chat.
                Event::TurnStarted { session_id } => {
                    buffers.remove(&session_id);
                    if let Some(chat_id) = self.chat_for(&runtime, &mut chats, session_id).await {
                        active.lock().await.insert(session_id, chat_id);
                        self.client.typing(chat_id).await;
                    }
                }
                // Accumulate deltas silently (fallback only) — never edit a
                // live message; that "replacing bubble" is what felt off.
                Event::TurnDelta { session_id, text } => {
                    buffers.entry(session_id).or_default().push_str(&text);
                }
                Event::TurnCompleted { session_id, text, .. } => {
                    active.lock().await.remove(&session_id);
                    let buffered = buffers.remove(&session_id).unwrap_or_default();
                    let final_text = if text.is_empty() { buffered } else { text };
                    if let Some(chat_id) = self.chat_for(&runtime, &mut chats, session_id).await {
                        if final_text.trim().is_empty() {
                            let _ = self.client.send_message(chat_id, "(no response)").await;
                        } else {
                            self.send_long(chat_id, &final_text).await;
                        }
                    }
                }
                Event::TurnFailed { session_id, error } => {
                    active.lock().await.remove(&session_id);
                    buffers.remove(&session_id);
                    if let Some(chat_id) = self.chat_for(&runtime, &mut chats, session_id).await {
                        let _ = self.client.send_message(chat_id, &format!("⚠️ {error}")).await;
                    }
                }
                // Owner stopped the turn — flush any partial text so nothing is
                // lost, then confirm.
                Event::TurnCancelled { session_id } => {
                    active.lock().await.remove(&session_id);
                    let buffered = buffers.remove(&session_id).unwrap_or_default();
                    if let Some(chat_id) = self.chat_for(&runtime, &mut chats, session_id).await {
                        if !buffered.trim().is_empty() {
                            self.send_long(chat_id, &buffered).await;
                        }
                        let _ = self.client.send_message(chat_id, "🛑 stopped").await;
                    }
                }
                // Tool activity just refreshes "typing…" — the chat stays clean
                // (tools are visible in the TUI / web UI).
                Event::ToolStarted { session_id, .. } => {
                    if let Some(chat_id) = self.chat_for(&runtime, &mut chats, session_id).await {
                        self.client.typing(chat_id).await;
                    }
                }
                // A mid-turn message was folded into the running turn — a quiet
                // ack so the user knows it landed and is being factored in.
                Event::ContextFolded { session_id, note } => {
                    if let Some(chat_id) = self.chat_for(&runtime, &mut chats, session_id).await {
                        let _ = self
                            .client
                            .send_message(chat_id, &format!("✏️ folding that in: {}", clip(&note, 200)))
                            .await;
                    }
                }
                // …or triaged as a separate task, to run right after this one.
                Event::TaskQueued { session_id, task } => {
                    if let Some(chat_id) = self.chat_for(&runtime, &mut chats, session_id).await {
                        let _ = self
                            .client
                            .send_message(
                                chat_id,
                                &format!("🧵 queued as a separate task (after this one): {}", clip(&task, 200)),
                            )
                            .await;
                    }
                }
                // Loop results pushed to telegram go to every paired chat.
                Event::LoopCompleted { name, channel_out, text, .. }
                    if channel_out.contains("telegram") =>
                {
                    let peers = runtime.store.peers_list(CHANNEL).await.unwrap_or_default();
                    for peer in peers {
                        if let Ok(chat_id) = peer.parse::<i64>() {
                            let _ = self
                                .client
                                .send_message(chat_id, &format!("🔁 {name}\n\n{}", clip(&text, 3800)))
                                .await;
                        }
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
                // A reminder/timer came due → the owner's pocket.
                Event::ReminderFired { message } => {
                    for peer in runtime.store.peers_list(CHANNEL).await.unwrap_or_default() {
                        if let Ok(chat_id) = peer.parse::<i64>() {
                            let _ = self.client.send_message(chat_id, &format!("⏰ {message}")).await;
                        }
                    }
                }
                // Auto-update news → every paired chat (the owner's pocket).
                Event::UpdateAvailable { current, latest, channel } => {
                    let from = current.unwrap_or_else(|| "source".into());
                    let msg = format!(
                        "⬆️ Update available: {from} → {latest} ({channel}).\nRun `revenant update` on the host to take it."
                    );
                    for peer in runtime.store.peers_list(CHANNEL).await.unwrap_or_default() {
                        if let Ok(chat_id) = peer.parse::<i64>() {
                            let _ = self.client.send_message(chat_id, &msg).await;
                        }
                    }
                }
                Event::UpdateInstalled { tag, restarting } => {
                    let msg = format!(
                        "✅ Auto-updated to {tag}.{}",
                        if restarting { " Restarting now." } else { " Restart to apply." }
                    );
                    for peer in runtime.store.peers_list(CHANNEL).await.unwrap_or_default() {
                        if let Ok(chat_id) = peer.parse::<i64>() {
                            let _ = self.client.send_message(chat_id, &msg).await;
                        }
                    }
                }
                // Spend crossed a budget threshold → the owner's pocket.
                Event::BudgetAlert { pct, spent, budget } => {
                    let msg = format!("💸 Spend alert: {spent} today ({pct}% of {budget} daily budget).");
                    for peer in runtime.store.peers_list(CHANNEL).await.unwrap_or_default() {
                        if let Ok(chat_id) = peer.parse::<i64>() {
                            let _ = self.client.send_message(chat_id, &msg).await;
                        }
                    }
                }
                _ => {}
            }
        }
    }

    /// Send a reply as one or more messages (Telegram caps a message near
    /// 4096 chars). Splitting on line boundaries keeps it readable and, unlike
    /// an edited bubble, each part actually notifies — it reads as a chat.
    async fn send_long(&self, chat_id: i64, text: &str) {
        for part in split_message(text, TG_MAX) {
            // Render markdown → Telegram HTML so **bold**, `code`, code blocks
            // and [links](url) look right. Fall back to the raw text if the
            // markup is somehow malformed (Telegram rejects bad HTML).
            let html = md_to_telegram_html(&part);
            if self.client.send_html(chat_id, &html).await.is_err()
                && self.client.send_message(chat_id, &part).await.is_err()
            {
                break;
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

/// Render the agent's GitHub-flavored markdown into the small HTML subset
/// Telegram supports (`<b> <i> <code> <pre> <a>`). Code regions are pulled out
/// FIRST so their contents aren't re-interpreted, everything is HTML-escaped,
/// then inline markup is applied and the code re-inserted. Unmatched markers
/// stay literal (safe) — and the caller falls back to plain text if Telegram
/// ever rejects the result.
fn md_to_telegram_html(md: &str) -> String {
    use regex::Regex;
    let esc = |s: &str| s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;");

    // 1. Stash code so later passes can't touch it. NUL-delimited tokens
    //    (NUL never appears in chat text).
    let mut stash: Vec<String> = Vec::new();
    let fence = Regex::new(r"(?s)```[a-zA-Z0-9_+.-]*\n?(.*?)```").unwrap();
    let inline = Regex::new(r"`([^`\n]+)`").unwrap();

    let mut out = String::new();
    let mut last = 0;
    for m in fence.captures_iter(md) {
        let whole = m.get(0).unwrap();
        out.push_str(&md[last..whole.start()]);
        out.push_str(&format!("\u{0}{}\u{0}", stash.len()));
        stash.push(format!("<pre>{}</pre>", esc(&m[1])));
        last = whole.end();
    }
    out.push_str(&md[last..]);

    // Inline code next (same parking).
    let mut tmp = String::new();
    let mut last = 0;
    for m in inline.captures_iter(&out) {
        let whole = m.get(0).unwrap();
        tmp.push_str(&out[last..whole.start()]);
        tmp.push_str(&format!("\u{0}{}\u{0}", stash.len()));
        stash.push(format!("<code>{}</code>", esc(&m[1])));
        last = whole.end();
    }
    tmp.push_str(&out[last..]);

    // 2. Escape the prose, then apply the inline markup Telegram understands.
    let mut s = esc(&tmp);
    s = Regex::new(r"\*\*([^*\n]+)\*\*").unwrap().replace_all(&s, "<b>$1</b>").into_owned();
    s = Regex::new(r#"\[([^\]]+)\]\((https?://[^)\s]+)\)"#)
        .unwrap()
        .replace_all(&s, r#"<a href="$2">$1</a>"#)
        .into_owned();
    s = Regex::new(r"(?m)^\s*#{1,6}\s+(.*)$").unwrap().replace_all(&s, "<b>$1</b>").into_owned();
    s = Regex::new(r"(?m)^(\s*)[-*]\s+").unwrap().replace_all(&s, "$1• ").into_owned();

    // 3. Re-insert the parked code (tokens survived escaping — they're NULs).
    for (i, frag) in stash.iter().enumerate() {
        s = s.replace(&format!("\u{0}{i}\u{0}"), frag);
    }
    s
}

/// Split a reply into Telegram-sized pieces, preferring line boundaries so a
/// message is never chopped mid-line. A single line longer than `max` is
/// hard-wrapped. Returns `[text]` unchanged when it already fits.
fn split_message(text: &str, max: usize) -> Vec<String> {
    if text.chars().count() <= max {
        return vec![text.to_string()];
    }
    let mut parts = Vec::new();
    let mut cur = String::new();
    let flush = |cur: &mut String, parts: &mut Vec<String>| {
        let trimmed = cur.trim_end_matches('\n');
        if !trimmed.is_empty() {
            parts.push(trimmed.to_string());
        }
        cur.clear();
    };
    for line in text.split_inclusive('\n') {
        if line.chars().count() > max {
            flush(&mut cur, &mut parts);
            let mut rest = line;
            while rest.chars().count() > max {
                let cut = rest.char_indices().nth(max).map(|(i, _)| i).unwrap_or(rest.len());
                parts.push(rest[..cut].to_string());
                rest = &rest[cut..];
            }
            cur.push_str(rest);
        } else {
            if cur.chars().count() + line.chars().count() > max {
                flush(&mut cur, &mut parts);
            }
            cur.push_str(line);
        }
    }
    flush(&mut cur, &mut parts);
    parts
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

    #[test]
    fn md_to_telegram_html_renders_the_common_markup() {
        let h = md_to_telegram_html("**bold** and `code` and [x](https://a.com)");
        assert!(h.contains("<b>bold</b>"));
        assert!(h.contains("<code>code</code>"));
        assert!(h.contains(r#"<a href="https://a.com">x</a>"#));
    }

    #[test]
    fn md_to_telegram_html_escapes_and_protects_code() {
        // Markup INSIDE a code block must not be interpreted, and <>& escaped.
        let h = md_to_telegram_html("```\nif a < b && **x** { }\n```");
        assert!(h.contains("<pre>"));
        assert!(h.contains("&lt; b &amp;&amp; **x**"), "code stays literal + escaped: {h}");
        assert!(!h.contains("<b>x</b>"), "no bold inside code");
        // Prose angle brackets are escaped (no injection).
        let h2 = md_to_telegram_html("a <script> tag");
        assert!(h2.contains("&lt;script&gt;"));
    }

    #[test]
    fn md_to_telegram_html_leaves_unmatched_markers_literal() {
        // A lone ** must not produce an unclosed tag (would 400 Telegram).
        let h = md_to_telegram_html("2 ** 3 = 8 is not bold");
        assert!(!h.contains("<b>"), "no stray bold tag: {h}");
    }

    #[test]
    fn split_message_keeps_short_text_whole() {
        assert_eq!(split_message("hi there", 100), vec!["hi there".to_string()]);
    }

    #[test]
    fn split_message_chunks_on_line_boundaries_under_limit() {
        let text = "line one\nline two\nline three";
        let parts = split_message(text, 12);
        assert!(parts.len() >= 2, "expected multiple parts, got {parts:?}");
        for p in &parts {
            assert!(p.chars().count() <= 12, "part too long: {p:?}");
        }
        // Every original line survives intact across the parts.
        let joined = parts.join("\n");
        for line in ["line one", "line two", "line three"] {
            assert!(joined.contains(line));
        }
    }

    #[test]
    fn split_message_hard_wraps_a_giant_line() {
        let text = "x".repeat(50);
        let parts = split_message(&text, 20);
        assert_eq!(parts.len(), 3); // 20 + 20 + 10
        assert_eq!(parts.iter().map(|p| p.chars().count()).sum::<usize>(), 50);
    }
}
