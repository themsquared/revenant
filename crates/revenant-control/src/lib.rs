//! revenant-control: the /v1 REST+SSE control plane.
//!
//! One API for every surface (CLI chat, TUI, web UI, future channels).
//! Loopback bind, bearer token ALWAYS required — no cookies, no CSRF
//! surface. Everything is curl-able.

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::sse::{Event as SseEvent, KeepAlive, Sse};
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json, Router};
use futures::stream::Stream;
use revenant_agent::{SessionManager, SessionMsg};
use revenant_core::Tier;
use serde::Deserialize;
use serde_json::json;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use tokio_stream::wrappers::BroadcastStream;
use tokio_stream::StreamExt;

#[derive(Clone)]
pub struct AppState {
    pub manager: SessionManager,
    pub token: String,
    pub default_tier: Tier,
    pub gateway_probe: revenant_llm::LlmClient,
    pub home: revenant_core::home::Home,
    /// Gateway admin/analytics port — for the authoritative spend view.
    pub admin_port: u16,
    event_seq: Arc<AtomicU64>,
    /// A2A envelope replay guard: nonce → first-seen ts, pruned past freshness.
    a2a_nonces: Arc<std::sync::Mutex<std::collections::HashMap<String, i64>>>,
    /// Cached network standing per sender pubkey: (reputation, fetched_ts).
    a2a_rep: Arc<std::sync::Mutex<std::collections::HashMap<String, (f64, i64)>>>,
    /// Cached set of this account's bound agent pubkeys: (set, fetched_ts).
    a2a_kin: Arc<std::sync::Mutex<(Vec<String>, i64)>>,
    /// Rolling-hour call timestamps per capability-limited sender.
    a2a_rate: Arc<std::sync::Mutex<std::collections::HashMap<String, Vec<i64>>>>,
}

impl AppState {
    pub fn new(
        manager: SessionManager,
        token: String,
        default_tier: Tier,
        gateway_probe: revenant_llm::LlmClient,
        home: revenant_core::home::Home,
        admin_port: u16,
    ) -> Self {
        AppState {
            manager,
            token,
            default_tier,
            gateway_probe,
            home,
            admin_port,
            event_seq: Arc::new(AtomicU64::new(1)),
            a2a_nonces: Arc::new(std::sync::Mutex::new(std::collections::HashMap::new())),
            a2a_rep: Arc::new(std::sync::Mutex::new(std::collections::HashMap::new())),
            a2a_kin: Arc::new(std::sync::Mutex::new((Vec::new(), 0))),
            a2a_rate: Arc::new(std::sync::Mutex::new(std::collections::HashMap::new())),
        }
    }
}

#[derive(rust_embed::Embed)]
#[folder = "../../web/dist"]
struct WebAssets;

pub fn router(state: AppState) -> Router {
    // /v1/* requires the bearer token; static UI assets do not (the browser
    // needs the HTML/JS before it can authenticate).
    let api = Router::new()
        .route("/v1/health", get(health))
        .route("/v1/events", get(events))
        .route("/v1/sessions", get(sessions_list).post(session_create))
        .route("/v1/sessions/:id/messages", get(messages_list).post(message_send))
        .route("/v1/sessions/:id/cancel", post(session_cancel))
        .route("/v1/code", post(code))
        .route("/v1/approvals", get(approvals_pending))
        .route("/v1/approvals/:id/decision", post(approval_decide))
        .route("/v1/skills", get(skills_list))
        .route("/v1/tools", get(tools_list))
        .route("/v1/subagents", get(subagents_list))
        .route("/v1/agents", get(agents_list))
        .route("/v1/agents/:name", get(agent_get).put(agent_put))
        .route("/v1/personalities", get(personalities_list))
        .route("/v1/sessions/:id/persona", post(session_set_persona))
        .route("/v1/config", get(config_get))
        .route("/v1/loops", get(loops_list))
        .route("/v1/loops/:id/runs", get(loop_runs))
        .route("/v1/loops/:id", axum::routing::delete(loop_delete).patch(loop_patch))
        .route("/v1/spend", get(spend))
        .route("/v1/budget", get(budget))
        .route("/v1/introspect", post(introspect))
        .route("/v1/analytics", get(analytics))
        .route("/v1/memory/status", get(memory_status))
        .route("/v1/gateway/status", get(gateway_status))
        .route("/v1/channels/pairings", post(pairing_create))
        // Necropolis (the horde network) — read proxies + signed clickops.
        .route("/v1/net/quests", get(net_quests))
        .route("/v1/net/quests/:id", get(net_quest))
        .route("/v1/net/quests/:id/claim", post(net_claim))
        .route("/v1/net/quests/:id/solve", post(net_solve))
        .route("/v1/net/quests/:id/accept", post(net_accept))
        .route("/v1/net/quests/:id/close", post(net_close))
        .route("/v1/net/boost", post(net_boost))
        .route("/v1/net/leaderboard", get(net_leaderboard))
        .route("/v1/net/bazaar", get(net_bazaar))
        .route("/v1/net/bazaar/:id/install", post(net_install))
        .route("/v1/net/me", get(net_me))
        .route("/v1/net/horde", get(net_horde))
        // Distributed thinking: orchestrate a run across the private board.
        .route("/v1/net/horde/run", post(net_horde_run_start))
        .route("/v1/net/horde/run/:run", get(net_horde_run_get))
        .route("/v1/net/horde/synthesize", post(net_horde_synthesize))
        .layer(axum::middleware::from_fn_with_state(state.clone(), auth))
        .with_state(state.clone());

    // The A2A agent card is discovery — served unauthenticated (loopback).
    // /a2a itself does NOT sit behind the bearer: it authenticates each request
    // with a signed envelope (sender identity + freshness + nonce) and scales
    // capability by the sender's standing — see a2a_message.
    Router::new()
        .route("/.well-known/agent-card.json", get(agent_card))
        .route("/a2a", post(a2a_message))
        .with_state(state)
        .merge(api)
        .fallback(serve_ui)
}

/// A2A agent card (well-known discovery document) describing revenant as an
/// agent other agents can call. Shape follows the A2A AgentCard schema.
async fn agent_card(State(state): State<AppState>) -> Json<serde_json::Value> {
    let skills: Vec<serde_json::Value> = state
        .manager
        .runtime()
        .skills
        .list()
        .into_iter()
        .map(|s| json!({ "id": s.name, "name": s.name, "description": s.description, "tags": [] }))
        .collect();
    let base = std::env::var("REVENANT_URL").unwrap_or_else(|_| "http://127.0.0.1:7717".to_string());
    Json(json!({
        "protocolVersion": "0.3.0",
        "name": "revenant",
        "description": "A lean, security-first personal agent. Gateway-native: keys, spend, and \
            data boundaries enforced beneath the agent. Send it a task; it does not stop.",
        "url": format!("{base}/a2a"),
        "version": env!("CARGO_PKG_VERSION"),
        "capabilities": { "streaming": false, "pushNotifications": false },
        "defaultInputModes": ["text/plain"],
        "defaultOutputModes": ["text/plain"],
        "securitySchemes": {
            "bearer": { "type": "http", "scheme": "bearer" }
        },
        "security": [{ "bearer": [] }],
        "skills": if skills.is_empty() {
            json!([{ "id": "chat", "name": "chat", "description": "General assistance, tools, memory.", "tags": ["general"] }])
        } else {
            json!(skills)
        },
    }))
}

#[derive(Deserialize)]
struct A2aRpc {
    #[serde(default)]
    id: serde_json::Value,
    method: String,
    #[serde(default)]
    params: serde_json::Value,
}

/// How the receiver treats a verified A2A sender, by standing.
const A2A_TRUST_REP: f64 = 10.0; // ≥ this network reputation → full turn
const A2A_REJECT_REP: f64 = 0.0; // < this → rejected outright
const A2A_LIMITED_PER_HOUR: usize = 10; // rate cap for capability-limited senders
const A2A_CACHE_SECS: i64 = 300; // reputation / kin-roster cache TTL

/// The trust tier a verified sender lands in.
enum A2aTier {
    /// Same account, explicitly trusted, or high reputation: full agent turn.
    Full,
    /// Validly signed but unknown/low standing: no-tools reply, rate-capped.
    Limited,
    /// Negative standing: rejected.
    Rejected(f64),
}

/// Resolve a verified sender's trust tier: kin (bound to this account) and
/// configured peers get Full; otherwise network reputation decides. Lookups
/// are cached; an unreachable Necropolis fails CLOSED to Limited, never Full.
async fn a2a_tier(state: &AppState, sender: &str) -> A2aTier {
    let now = net_now();
    // Explicitly trusted pubkeys from config.
    let trusted = revenant_core::config::Config::from_toml(
        &std::fs::read_to_string(state.home.config_path()).unwrap_or_default(),
    )
    .map(|c| c.network.a2a_trusted)
    .unwrap_or_default();
    if trusted.iter().any(|p| p == sender) {
        return A2aTier::Full;
    }
    // Kin: agents bound to the same account (via the account key on this node).
    let kin_fresh = { state.a2a_kin.lock().unwrap().1 + A2A_CACHE_SECS > now };
    if !kin_fresh {
        if let Ok((c, _)) = net_ctx(state) {
            if let Ok(key) = std::fs::read_to_string(state.home.root().join("account.key")) {
                if let Ok(agents) = c.account_agents(key.trim()).await {
                    let pks = agents
                        .iter()
                        .filter_map(|a| a.get("agent").and_then(|x| x.as_str()).map(String::from))
                        .collect();
                    *state.a2a_kin.lock().unwrap() = (pks, now);
                }
            }
        }
    }
    if state.a2a_kin.lock().unwrap().0.iter().any(|p| p == sender) {
        return A2aTier::Full;
    }
    // Network reputation, cached. Unreachable directory → unknown → Limited.
    let cached = state.a2a_rep.lock().unwrap().get(sender).copied();
    let rep = match cached {
        Some((r, at)) if at + A2A_CACHE_SECS > now => r,
        _ => {
            let fetched = match net_ctx(state) {
                Ok((c, _)) => c.reputation().await.ok().and_then(|m| m.get(sender).copied()),
                Err(_) => None,
            }
            .unwrap_or(0.0);
            state.a2a_rep.lock().unwrap().insert(sender.to_string(), (fetched, now));
            fetched
        }
    };
    if rep >= A2A_TRUST_REP {
        A2aTier::Full
    } else if rep < A2A_REJECT_REP {
        A2aTier::Rejected(rep)
    } else {
        A2aTier::Limited
    }
}

/// A2A JSON-RPC endpoint. Every request must carry a signed envelope (sender
/// pubkey + ts + nonce + signature over the exact body bytes) — the bearer
/// token proves nothing about identity, so it is not used here. Capability
/// scales with the sender's standing: kin/trusted/high-rep senders get a full
/// agent turn; unknown-but-verified senders get a rate-capped, no-tools reply;
/// negative-standing senders are refused. Implements `message/send`.
async fn a2a_message(
    State(state): State<AppState>,
    headers: axum::http::HeaderMap,
    body: axum::body::Bytes,
) -> (StatusCode, Json<serde_json::Value>) {
    use revenant_net::a2a as env;
    let rpc_err = |status: StatusCode, id: serde_json::Value, code: i64, msg: &str| {
        (status, Json(json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": msg } })))
    };
    let hdr = |name: &str| headers.get(name).and_then(|v| v.to_str().ok()).map(str::to_string);

    // 1. The envelope must be present, fresh, unreplayed, and validly signed
    //    over exactly these body bytes.
    let (Some(sender), Some(ts), Some(nonce), Some(sig)) =
        (hdr(env::HDR_AGENT), hdr(env::HDR_TS), hdr(env::HDR_NONCE), hdr(env::HDR_SIG))
    else {
        return rpc_err(
            StatusCode::UNAUTHORIZED,
            json!(null),
            -32000,
            "missing signed envelope (x-rev-agent/x-rev-ts/x-rev-nonce/x-rev-sig)",
        );
    };
    let ts: i64 = ts.parse().unwrap_or(0);
    let now = net_now();
    if (now - ts).abs() > env::A2A_FRESHNESS_SECS {
        return rpc_err(StatusCode::UNAUTHORIZED, json!(null), -32000, "envelope timestamp outside freshness window");
    }
    {
        let mut nonces = state.a2a_nonces.lock().unwrap();
        nonces.retain(|_, seen| now - *seen <= env::A2A_FRESHNESS_SECS);
        if nonces.contains_key(&nonce) {
            return rpc_err(StatusCode::UNAUTHORIZED, json!(null), -32000, "replayed envelope nonce");
        }
        nonces.insert(nonce.clone(), now);
    }
    if !env::verify(&sender, &body, ts, &nonce, &sig) {
        return rpc_err(StatusCode::UNAUTHORIZED, json!(null), -32000, "envelope signature verification failed");
    }

    // 2. Parse the JSON-RPC body (only after authentication).
    let rpc: A2aRpc = match serde_json::from_slice(&body) {
        Ok(r) => r,
        Err(e) => return rpc_err(StatusCode::BAD_REQUEST, json!(null), -32700, &format!("parse error: {e}")),
    };
    if rpc.method != "message/send" {
        return rpc_err(StatusCode::OK, rpc.id, -32601, &format!("unsupported A2A method '{}'", rpc.method));
    }
    let text: String = rpc
        .params
        .pointer("/message/parts")
        .and_then(|p| p.as_array())
        .map(|parts| {
            parts
                .iter()
                .filter_map(|p| p.get("text").and_then(|t| t.as_str()))
                .collect::<Vec<_>>()
                .join("\n")
        })
        .unwrap_or_default();
    if text.trim().is_empty() {
        return rpc_err(StatusCode::OK, rpc.id, -32602, "message has no text parts");
    }

    // 3. Authorization: capability scales with the sender's standing.
    let reply = |id: serde_json::Value, text: String, tier: &str| {
        (
            StatusCode::OK,
            Json(json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": {
                    "role": "agent",
                    "parts": [{ "kind": "text", "text": text }],
                    "kind": "message",
                    "metadata": { "trust": tier }
                }
            })),
        )
    };
    match a2a_tier(&state, &sender).await {
        A2aTier::Rejected(rep) => {
            tracing::warn!("a2a: rejected sender {} (reputation {rep:.1})", &sender[..12.min(sender.len())]);
            rpc_err(StatusCode::FORBIDDEN, rpc.id, -32001, "sender's network standing is negative — refused")
        }
        A2aTier::Limited => {
            // Rate-cap unknown senders, then answer WITHOUT tools or owner
            // context — a plain model reply, never an agent turn.
            {
                let mut rate = state.a2a_rate.lock().unwrap();
                let hits = rate.entry(sender.clone()).or_default();
                hits.retain(|t| now - *t < 3600);
                if hits.len() >= A2A_LIMITED_PER_HOUR {
                    return rpc_err(StatusCode::TOO_MANY_REQUESTS, rpc.id, -32002, "rate limit for unrecognized senders");
                }
                hits.push(now);
            }
            let sys = "You are a revenant answering a message from an agent you don't know. You have \
NO tools and NO access to your owner's data in this reply. Be brief and helpful on general questions; \
decline anything that asks about your owner, their systems, or actions on their behalf.";
            match llm_text(&state, sys, text, 800).await {
                Ok(t) => reply(rpc.id, t, "limited"),
                Err(e) => rpc_err(StatusCode::OK, rpc.id, -32603, &format!("reply failed: {}", e.message)),
            }
        }
        A2aTier::Full => {
            let runtime = state.manager.runtime();
            // Per-sender session so distinct peers never share one thread.
            let peer = format!("{}", &sender[..12.min(sender.len())]);
            let session_id = match runtime.store.ensure_session("a2a", &peer, "chat").await {
                Ok(id) => id,
                Err(err) => return rpc_err(StatusCode::OK, rpc.id, -32603, &format!("session: {err:#}")),
            };
            match runtime
                .run_turn(session_id, state.default_tier, vec![revenant_core::ContentBlock::text(text)])
                .await
            {
                Ok(stats) => reply(rpc.id, stats.final_text, "full"),
                Err(err) => rpc_err(StatusCode::OK, rpc.id, -32603, &format!("turn failed: {err:#}")),
            }
        }
    }
}

/// Serve an embedded UI asset, falling back to index.html for SPA routes.
async fn serve_ui(uri: axum::http::Uri) -> axum::response::Response {
    let path = uri.path().trim_start_matches('/');
    let path = if path.is_empty() { "index.html" } else { path };
    let asset = WebAssets::get(path).or_else(|| WebAssets::get("index.html"));
    match asset {
        Some(content) => {
            let mime = mime_guess::from_path(path).first_or_octet_stream();
            (
                [(axum::http::header::CONTENT_TYPE, mime.as_ref())],
                content.data,
            )
                .into_response()
        }
        None => (
            StatusCode::NOT_FOUND,
            "web UI not embedded (build web/ first)",
        )
            .into_response(),
    }
}

async fn auth(
    State(state): State<AppState>,
    request: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    let provided = request
        .headers()
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(str::to_string)
        // EventSource clients can't always set headers; ?token= fallback.
        .or_else(|| {
            request.uri().query().and_then(|q| {
                q.split('&')
                    .find_map(|kv| kv.strip_prefix("token=").map(str::to_string))
            })
        });
    match provided {
        Some(token) if constant_time_eq(&token, &state.token) => next.run(request).await,
        _ => (StatusCode::UNAUTHORIZED, "missing or invalid bearer token").into_response(),
    }
}

fn constant_time_eq(a: &str, b: &str) -> bool {
    a.len() == b.len()
        && a.bytes().zip(b.bytes()).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

async fn health(State(state): State<AppState>) -> Json<serde_json::Value> {
    let gateway = state.gateway_probe.models_ready().await;
    Json(json!({
        "ok": true,
        "version": env!("CARGO_PKG_VERSION"),
        "gateway_healthy": gateway,
    }))
}

async fn events(
    State(state): State<AppState>,
) -> Sse<impl Stream<Item = Result<SseEvent, std::convert::Infallible>>> {
    let rx = state.manager.runtime().events.subscribe();
    let seq = state.event_seq.clone();
    let stream = BroadcastStream::new(rx).filter_map(move |event| match event {
        Ok(event) => {
            let id = seq.fetch_add(1, Ordering::Relaxed);
            let name = match &event {
                revenant_core::Event::TurnDelta { .. } => "turn_delta",
                revenant_core::Event::TurnStarted { .. } => "turn_started",
                revenant_core::Event::TurnCompleted { .. } => "turn_completed",
                revenant_core::Event::TurnFailed { .. } => "turn_failed",
                revenant_core::Event::ToolStarted { .. } => "tool_started",
                revenant_core::Event::ToolFinished { .. } => "tool_finished",
                revenant_core::Event::ApprovalCreated { .. } => "approval_created",
                revenant_core::Event::ApprovalResolved { .. } => "approval_resolved",
                revenant_core::Event::SubagentSpawned { .. } => "subagent_spawned",
                revenant_core::Event::SubagentFinished { .. } => "subagent_finished",
                revenant_core::Event::LoopCompleted { .. } => "loop_completed",
                revenant_core::Event::SkillLearned { .. } => "skill_learned",
                revenant_core::Event::PrivacyRouted { .. } => "privacy_routed",
                revenant_core::Event::GatewayStatus { .. } => "gateway_status",
                revenant_core::Event::UpdateAvailable { .. } => "update_available",
                revenant_core::Event::UpdateInstalled { .. } => "update_installed",
                revenant_core::Event::ContextFolded { .. } => "context_folded",
                revenant_core::Event::TaskQueued { .. } => "task_queued",
                revenant_core::Event::ReminderFired { .. } => "reminder_fired",
                revenant_core::Event::ComplexityRouted { .. } => "complexity_routed",
                revenant_core::Event::TurnCancelled { .. } => "turn_cancelled",
                revenant_core::Event::BudgetAlert { .. } => "budget_alert",
                revenant_core::Event::SelfReviewCompleted { .. } => "self_review_completed",
            };
            Some(Ok(SseEvent::default()
                .id(id.to_string())
                .event(name)
                .data(serde_json::to_string(&event).unwrap_or_default())))
        }
        Err(_) => None, // lagged; client re-syncs via REST
    });
    Sse::new(stream).keep_alive(KeepAlive::default())
}

async fn sessions_list(State(state): State<AppState>) -> Result<Json<serde_json::Value>, ApiError> {
    let sessions = state.manager.runtime().store.sessions_list(100).await?;
    Ok(Json(json!({ "sessions": sessions })))
}

#[derive(Deserialize)]
struct SessionCreate {
    #[serde(default)]
    peer: Option<String>,
}

async fn session_create(
    State(state): State<AppState>,
    Json(body): Json<SessionCreate>,
) -> Result<Json<serde_json::Value>, ApiError> {
    // No explicit peer → mint a UNIQUE one so each call starts a NEW
    // conversation (POST = create). ensure_session upserts on
    // (channel, peer, kind), so a fixed default would forever return the same
    // session — which is exactly the "can't start a second chat" bug. An
    // explicit peer still addresses a stable session (channel integrations).
    let peer = body.peer.unwrap_or_else(|| {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        format!("web-{nanos:x}")
    });
    let id = state
        .manager
        .runtime()
        .store
        .ensure_session("api", &peer, "chat")
        .await?;
    Ok(Json(json!({ "id": id })))
}

#[derive(Deserialize)]
struct MessagesQuery {
    #[serde(default = "default_limit")]
    limit: usize,
}
fn default_limit() -> usize {
    50
}

async fn messages_list(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Query(q): Query<MessagesQuery>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let messages = state.manager.runtime().store.history(id, q.limit).await?;
    let out: Vec<_> = messages
        .into_iter()
        .map(|m| {
            json!({
                "id": m.id,
                "turn": m.turn,
                "role": m.role.as_str(),
                "content": m.content,
            })
        })
        .collect();
    Ok(Json(json!({ "messages": out })))
}

#[derive(Deserialize)]
struct SendBody {
    text: String,
    #[serde(default)]
    tier: Option<String>,
}

async fn message_send(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Json(body): Json<SendBody>,
) -> Result<impl IntoResponse, ApiError> {
    let tier = match body.tier.as_deref() {
        Some(t) => t.parse::<Tier>().map_err(ApiError::bad_request)?,
        None => state.default_tier,
    };
    // Reject unknown sessions up front — otherwise the turn 202-accepts and
    // then dies on a FK constraint in the background, with no error to the
    // client. Create a session with POST /v1/sessions first.
    if !state.manager.runtime().store.session_exists(id).await? {
        return Err(ApiError {
            status: StatusCode::NOT_FOUND,
            message: format!("no session {id} — create one with POST /v1/sessions"),
        });
    }
    state
        .manager
        .submit(id, SessionMsg::UserInput { content: body.text, tier })
        .await?;
    Ok((StatusCode::ACCEPTED, Json(json!({ "accepted": true, "session_id": id }))))
}

#[derive(Deserialize)]
struct CodeBody {
    /// Absolute path to the git worktree the coding agent may edit.
    root: String,
    task: String,
    #[serde(default)]
    tier: Option<String>,
}

/// Run one worktree-jailed coding turn (the Ascension actuator). The agent can
/// only read/write within `root`; the caller builds/tests the result.
async fn code(
    State(state): State<AppState>,
    Json(body): Json<CodeBody>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let tier = match body.tier.as_deref() {
        Some(t) => t.parse::<Tier>().map_err(ApiError::bad_request)?,
        None => state.default_tier,
    };
    let root = std::path::PathBuf::from(&body.root);
    // Guard: only ever edit an actual git worktree, never a stray path.
    if !root.join(".git").exists() {
        return Err(ApiError::bad_request("root is not a git worktree"));
    }
    let text = state.manager.runtime().code_once(&root, &body.task, tier).await?;
    Ok(Json(json!({ "text": text })))
}

async fn approvals_pending(
    State(state): State<AppState>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let pending = state.manager.runtime().store.approvals_pending().await?;
    Ok(Json(json!({ "approvals": pending })))
}

#[derive(Deserialize)]
struct DecisionBody {
    approve: bool,
    #[serde(default)]
    resolver: Option<String>,
    /// Approve every request of this kind for the session (task-level grant).
    #[serde(default)]
    grant: bool,
}

async fn approval_decide(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<DecisionBody>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let resolver = body.resolver.unwrap_or_else(|| "api".to_string());
    let applied = state
        .manager
        .runtime()
        .approvals
        .resolve_scoped(&id, body.approve, body.grant, &resolver)
        .await?;
    Ok(Json(json!({ "applied": applied })))
}

async fn skills_list(State(state): State<AppState>) -> Json<serde_json::Value> {
    let skills: Vec<_> = state
        .manager
        .runtime()
        .skills
        .list()
        .into_iter()
        .map(|s| json!({ "name": s.name, "description": s.description }))
        .collect();
    Json(json!({ "skills": skills }))
}

#[derive(Deserialize)]
struct SpendQuery {
    #[serde(default = "default_window")]
    window: String,
}
fn default_window() -> String {
    "today".to_string()
}

async fn spend(
    State(state): State<AppState>,
    Query(q): Query<SpendQuery>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let from = match q.window.as_str() {
        "today" => now - (now % 86_400),
        "24h" => now - 86_400,
        "7d" => now - 7 * 86_400,
        other => {
            return Err(ApiError::bad_request(format!(
                "unknown window '{other}' (today|24h|7d)"
            )))
        }
    };
    let rows = state.manager.runtime().store.spend_since(from).await?;
    Ok(Json(json!({ "window": q.window, "by_model": rows })))
}

/// Today's spend vs the configured soft daily budget — the same math the
/// background alert uses, for the web gauge. `configured:false` when no daily
/// budget is set (or a USD budget can't be priced).
async fn budget(State(state): State<AppState>) -> Result<Json<serde_json::Value>, ApiError> {
    let cfg = revenant_core::config::Config::from_toml(
        &std::fs::read_to_string(state.home.config_path()).unwrap_or_default(),
    )
    .map_err(|e| ApiError { status: StatusCode::INTERNAL_SERVER_ERROR, message: format!("parse config: {e}") })?;

    let priced = !cfg.pricing.is_empty();
    let (unit_usd, budget) = match (cfg.spending.daily_budget_usd, cfg.spending.daily_budget_tokens) {
        (Some(b), _) if priced && b > 0.0 => (true, b),
        (_, Some(b)) if b > 0 => (false, b as f64),
        _ => return Ok(Json(json!({ "configured": false }))),
    };

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let rows = state.manager.runtime().store.spend_since(now - (now % 86_400)).await?;
    let spent: f64 = if unit_usd {
        rows.iter()
            .filter_map(|r| {
                cfg.pricing.get(&r.model).map(|p| {
                    r.tokens_in as f64 / 1e6 * p.input_per_mtok
                        + r.tokens_out as f64 / 1e6 * p.output_per_mtok
                })
            })
            .sum()
    } else {
        rows.iter().map(|r| (r.tokens_in + r.tokens_out) as f64).sum()
    };
    let frac = if budget > 0.0 { spent / budget } else { 0.0 };
    let (spent_s, budget_s) = if unit_usd {
        (format!("${spent:.2}"), format!("${budget:.2}"))
    } else {
        (fmt_tokens(spent), fmt_tokens(budget))
    };
    Ok(Json(json!({
        "configured": true,
        "unit": if unit_usd { "usd" } else { "tokens" },
        "spent": spent_s,
        "budget": budget_s,
        "pct": (frac * 100.0).round() as i64,
        "frac": frac,
    })))
}

/// Run a behavioral self-review on demand and return what it found/changed.
async fn introspect(State(state): State<AppState>) -> Result<Json<serde_json::Value>, ApiError> {
    let cfg = revenant_core::config::Config::from_toml(
        &std::fs::read_to_string(state.home.config_path()).unwrap_or_default(),
    )
    .unwrap_or_else(|_| revenant_core::config::Config::default_config());
    let i = &cfg.introspection;
    let review = state
        .manager
        .runtime()
        .self_review(i.lookback_secs, i.max_notes, &i.tier)
        .await
        .map_err(|e| ApiError {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            message: format!("self-review failed: {e:#}"),
        })?;
    // On-demand: the owner asked, so always surface it to chat surfaces too.
    state.manager.runtime().events.emit(revenant_core::event::Event::SelfReviewCompleted {
        summary: review.summary.clone(),
        lessons: review.lessons.len() as u32,
        suggestions: review.suggestions.clone(),
    });
    Ok(Json(json!({
        "summary": review.summary,
        "lessons": review.lessons,
        "suggestions": review.suggestions,
    })))
}

fn fmt_tokens(n: f64) -> String {
    let n = n as u64;
    if n >= 1_000_000 {
        format!("{:.1}M tok", n as f64 / 1e6)
    } else if n >= 1_000 {
        format!("{:.1}K tok", n as f64 / 1e3)
    } else {
        format!("{n} tok")
    }
}

/// Gateway-authoritative spend: what agentgateway actually metered (tokens,
/// requests, cost) grouped by provider, over its default window (last 24h).
/// Fails soft — returns `available:false` with a reason when the gateway or its
/// request-log DB isn't reachable, so the UI can degrade to the store view.
async fn analytics(State(state): State<AppState>) -> Json<serde_json::Value> {
    match revenant_gateway::analytics_summary(state.admin_port, "provider").await {
        Ok(summary) => {
            let (requests, total_tokens, cost) = summary.totals();
            let by_provider: Vec<_> = summary
                .groups
                .iter()
                .map(|g| {
                    json!({
                        "label": g.label,
                        "requests": g.requests,
                        "total_tokens": g.total_tokens,
                        "cost": g.cost,
                    })
                })
                .collect();
            Json(json!({
                "available": true,
                "window": "last 24h",
                "from": summary.window_from,
                "to": summary.window_to,
                "by_provider": by_provider,
                "totals": { "requests": requests, "total_tokens": total_tokens, "cost": cost },
            }))
        }
        Err(e) => Json(json!({ "available": false, "error": e.to_string() })),
    }
}

async fn pairing_create(State(state): State<AppState>) -> Result<Json<serde_json::Value>, ApiError> {
    // 8 chars from an unambiguous alphabet, 10-minute TTL, single use.
    const ALPHABET: &[u8] = b"ABCDEFGHJKLMNPQRSTUVWXYZ23456789";
    let code: String = (0..8)
        .map(|_| {
            let mut byte = [0u8; 1];
            let _ = getrandom_fill(&mut byte);
            ALPHABET[byte[0] as usize % ALPHABET.len()] as char
        })
        .collect();
    state
        .manager
        .runtime()
        .store
        .pairing_code_create(&code, 600)
        .await?;
    Ok(Json(json!({ "code": code, "expires_in_s": 600 })))
}

fn getrandom_fill(buf: &mut [u8]) -> std::io::Result<()> {
    use std::io::Read;
    std::fs::File::open("/dev/urandom")?.read_exact(buf)
}

async fn personalities_list(State(state): State<AppState>) -> Json<serde_json::Value> {
    let items: Vec<_> = state
        .manager
        .runtime()
        .personalities
        .list()
        .into_iter()
        .map(|p| json!({ "name": p.name, "description": p.description, "emoji": p.emoji, "voice": p.voice }))
        .collect();
    Json(json!({ "personalities": items }))
}

#[derive(Deserialize)]
struct PersonaBody {
    /// null clears the persona (back to default voice).
    persona: Option<String>,
}

/// Stop the running turn for a session. `running: false` means nothing was in
/// flight — not an error (idempotent from the UI's perspective).
async fn session_cancel(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let running = state.manager.runtime().cancel(id);
    Ok(Json(json!({ "ok": true, "running": running })))
}

async fn session_set_persona(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Json(body): Json<PersonaBody>,
) -> Result<Json<serde_json::Value>, ApiError> {
    state
        .manager
        .runtime()
        .store
        .session_set_persona(id, body.persona.as_deref())
        .await?;
    Ok(Json(json!({ "ok": true, "persona": body.persona })))
}

async fn agents_list(State(state): State<AppState>) -> Json<serde_json::Value> {
    let agents: Vec<_> = state
        .manager
        .runtime()
        .agents
        .list()
        .into_iter()
        .map(|a| {
            json!({
                "name": a.name,
                "description": a.description,
                "tier": a.tier,
                "tools": a.tools,
                "skills": a.skills,
            })
        })
        .collect();
    Json(json!({ "agents": agents }))
}

async fn agent_get(
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let def = state
        .manager
        .runtime()
        .agents
        .get(&name)
        .ok_or_else(|| ApiError { status: StatusCode::NOT_FOUND, message: format!("no agent '{name}'") })?;
    Ok(Json(json!({
        "name": def.name,
        "description": def.description,
        "tier": def.tier,
        "tools": def.tools,
        "skills": def.skills,
        "directive": def.directive,
    })))
}

#[derive(Deserialize)]
struct AgentBody {
    #[serde(default)]
    description: String,
    #[serde(default)]
    tier: Option<String>,
    #[serde(default)]
    tools: Vec<String>,
    #[serde(default)]
    skills: Vec<String>,
    directive: String,
}

async fn agent_put(
    State(state): State<AppState>,
    Path(name): Path<String>,
    Json(body): Json<AgentBody>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let def = revenant_agent::AgentDef {
        name,
        description: body.description,
        tier: body.tier,
        tools: body.tools,
        skills: body.skills,
        directive: body.directive,
    };
    state.manager.runtime().agents.write(&def)?;
    Ok(Json(json!({ "ok": true })))
}

/// Redacted gateway/config view: tiers + models + failover, API-key
/// PRESENCE (never values), gateway info, embedder. Keys are file-only.
async fn config_get(State(state): State<AppState>) -> Result<Json<serde_json::Value>, ApiError> {
    let raw = std::fs::read_to_string(state.home.config_path())
        .map_err(|e| ApiError { status: StatusCode::INTERNAL_SERVER_ERROR, message: format!("read config: {e}") })?;
    let cfg = revenant_core::config::Config::from_toml(&raw)
        .map_err(|e| ApiError { status: StatusCode::INTERNAL_SERVER_ERROR, message: format!("parse config: {e}") })?;

    // Which key env vars are present in secrets.env (names only).
    let present: std::collections::HashSet<String> = std::fs::read_to_string(state.home.secrets_path())
        .unwrap_or_default()
        .lines()
        .filter_map(|l| l.split_once('=').map(|(k, _)| k.trim().to_string()))
        .collect();

    let tiers: serde_json::Map<String, serde_json::Value> = cfg
        .tiers
        .iter()
        .map(|(name, tier)| {
            let targets: Vec<_> = tier
                .targets
                .iter()
                .map(|t| {
                    json!({
                        "provider": format!("{:?}", t.provider).to_lowercase(),
                        "model": t.model,
                        "api_key_env": t.api_key_env,
                        "key_present": t.api_key_env.as_ref().map(|e| present.contains(e)).unwrap_or(true),
                        "base_url": t.base_url,
                    })
                })
                .collect();
            (name.clone(), json!({ "targets": targets, "failover": tier.targets.len() > 1 }))
        })
        .collect();

    Ok(Json(json!({
        "gateway": {
            "mode": format!("{:?}", cfg.gateway.mode).to_lowercase(),
            "version": cfg.gateway.version,
            "llm_port": cfg.gateway.llm_port,
            "endpoint": cfg.gateway.endpoint,
        },
        "tiers": tiers,
        "default_tier": cfg.agent.default_tier,
        "embedder": format!("{:?}", cfg.memory.embedder).to_lowercase(),
        "keys_present": present.into_iter().collect::<Vec<_>>(),
        "power_user": cfg.experience.power_user,
    })))
}

async fn loops_list(State(state): State<AppState>) -> Result<Json<serde_json::Value>, ApiError> {
    let loops = state.manager.runtime().store.loops_list().await?;
    Ok(Json(json!({ "loops": loops })))
}

async fn loop_runs(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let runs = state.manager.runtime().store.loop_runs(&id, 20).await?;
    Ok(Json(json!({ "runs": runs })))
}

async fn loop_delete(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let ok = state.manager.runtime().store.loop_delete(&id).await?;
    Ok(Json(json!({ "deleted": ok })))
}

#[derive(Deserialize)]
struct LoopPatch {
    enabled: bool,
}

async fn loop_patch(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<LoopPatch>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let ok = state.manager.runtime().store.loop_set_enabled(&id, body.enabled).await?;
    Ok(Json(json!({ "updated": ok })))
}

async fn tools_list(State(state): State<AppState>) -> Json<serde_json::Value> {
    let tools: Vec<_> = state
        .manager
        .runtime()
        .tools
        .describe()
        .into_iter()
        .map(|(name, description, tier)| {
            json!({
                "name": name,
                "description": description,
                "permission": format!("{tier:?}"),
            })
        })
        .collect();
    // subagent_run is virtual (not in the registry) — surface it too.
    let mut all = tools;
    all.push(json!({
        "name": "subagent_run",
        "description": "Delegate a self-contained subtask to a focused child agent.",
        "permission": "ReadOnly",
    }));
    all.push(json!({
        "name": "agent_create",
        "description": "Define a reusable subagent (directive, tools, tier) to delegate to later.",
        "permission": "WriteWorkspace",
    }));
    Json(json!({ "tools": all }))
}

async fn subagents_list(State(state): State<AppState>) -> Result<Json<serde_json::Value>, ApiError> {
    let rows = state.manager.runtime().store.subagents_list(50).await?;
    let out: Vec<_> = rows
        .into_iter()
        .map(|r| {
            // Pull the task text out of the stored first-user-message JSON.
            let task = r
                .first_user
                .as_deref()
                .and_then(|json| serde_json::from_str::<Vec<serde_json::Value>>(json).ok())
                .and_then(|blocks| {
                    blocks
                        .into_iter()
                        .find_map(|b| b.get("text").and_then(|t| t.as_str()).map(String::from))
                })
                .unwrap_or_default();
            json!({
                "id": r.id,
                "parent_session": r.parent_session,
                "created_at": r.created_at,
                "messages": r.message_count,
                "task": task,
            })
        })
        .collect();
    Ok(Json(json!({ "subagents": out })))
}

async fn memory_status(State(state): State<AppState>) -> Result<Json<serde_json::Value>, ApiError> {
    match &state.manager.runtime().memory {
        Some(memory) => {
            let status = memory.status().await?;
            Ok(Json(serde_json::to_value(status).unwrap_or_default()))
        }
        None => Err(ApiError {
            status: StatusCode::NOT_FOUND,
            message: "memory engine disabled".into(),
        }),
    }
}

async fn gateway_status(State(state): State<AppState>) -> Json<serde_json::Value> {
    let healthy = state.gateway_probe.models_ready().await;
    Json(json!({ "healthy": healthy }))
}

// ---- Necropolis (the horde network): read proxies + signed clickops --------
//
// Reads proxy through the daemon so the browser needs no CORS + no keys. Writes
// are signed here with THIS node's identity (never exposed to the browser); the
// server still enforces every rule (accounts, no-self-dealing, proof-of-work).

/// Resolve the network directory URL: env override, else configured
/// `network.necropolis_url` (only when the network is enabled).
fn net_url(home: &revenant_core::home::Home) -> Option<String> {
    if let Ok(u) = std::env::var("REVENANT_NECROPOLIS") {
        if !u.trim().is_empty() {
            return Some(u);
        }
    }
    let cfg =
        revenant_core::config::Config::from_toml(&std::fs::read_to_string(home.config_path()).ok()?).ok()?;
    if !cfg.network.enabled {
        return None;
    }
    cfg.network.necropolis_url
}

/// A Necropolis client + this node's signing identity, or a 400 if the network
/// isn't configured / the identity can't load.
fn net_ctx(
    state: &AppState,
) -> Result<(revenant_net::NecropolisClient, revenant_net::Identity), ApiError> {
    let url = net_url(&state.home).ok_or_else(|| {
        ApiError::bad_request(
            "the network isn't configured — set [network] enabled = true and necropolis_url",
        )
    })?;
    let id = revenant_net::Identity::load_or_create(&state.home.identity_dir())?;
    Ok((revenant_net::NecropolisClient::new(&url), id))
}

fn net_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

async fn net_quests(State(state): State<AppState>) -> Result<Json<serde_json::Value>, ApiError> {
    let (c, _) = net_ctx(&state)?;
    Ok(Json(json!({ "quests": c.quests(None).await? })))
}

async fn net_quest(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let (c, _) = net_ctx(&state)?;
    Ok(Json(c.quest(&id).await?))
}

async fn net_leaderboard(
    State(state): State<AppState>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let (c, _) = net_ctx(&state)?;
    Ok(Json(json!({ "leaderboard": c.leaderboard().await? })))
}

#[derive(Deserialize)]
struct BazaarQuery {
    #[serde(default)]
    q: String,
}

async fn net_bazaar(
    State(state): State<AppState>,
    Query(q): Query<BazaarQuery>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let (c, _) = net_ctx(&state)?;
    // A query searches the codex; otherwise list the whole artifact catalog.
    let items = if q.q.trim().is_empty() {
        c.list(None).await?
    } else {
        c.search(&q.q).await?.artifacts
    };
    Ok(Json(json!({ "items": items })))
}

/// This node's own standing: pubkey, credit balance, reputation.
async fn net_me(State(state): State<AppState>) -> Result<Json<serde_json::Value>, ApiError> {
    let (c, id) = net_ctx(&state)?;
    let me = id.id();
    let credits = c.credits().await.unwrap_or_default();
    let rep = c.reputation().await.unwrap_or_default();
    Ok(Json(json!({
        "pubkey": me,
        "credits": credits.get(&me).copied().unwrap_or(100),
        "reputation": rep.get(&me).copied().unwrap_or(0.0),
    })))
}

/// Every agent bound to this node's account (full cards — heartbeating or not),
/// via the account key on this node. Without an account key, falls back to just
/// this node's own agent from the public roster.
async fn net_horde(State(state): State<AppState>) -> Result<Json<serde_json::Value>, ApiError> {
    let (c, id) = net_ctx(&state)?;
    let me = id.id();
    let key_path = state.home.root().join("account.key");
    let agents: Vec<serde_json::Value> = match std::fs::read_to_string(&key_path) {
        Ok(k) if !k.trim().is_empty() => c.account_agents(k.trim()).await.unwrap_or_default(),
        // No account key bound on this node — show just this node from the roster.
        _ => c
            .agents()
            .await
            .unwrap_or_default()
            .into_iter()
            .filter(|a| a.get("agent").and_then(|x| x.as_str()) == Some(me.as_str()))
            .collect(),
    };
    let bound: Vec<String> = agents
        .iter()
        .filter_map(|a| a.get("agent").and_then(|x| x.as_str()).map(String::from))
        .collect();
    Ok(Json(json!({ "me": me, "bound": bound, "agents": agents })))
}

#[derive(Deserialize)]
struct ClaimBody {
    task: String,
}
async fn net_claim(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(b): Json<ClaimBody>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let (c, k) = net_ctx(&state)?;
    let claim = revenant_net::quest::TaskClaim::create(&k, id, b.task, net_now());
    c.claim_task(&claim).await?;
    Ok(Json(json!({ "ok": true })))
}

#[derive(Deserialize)]
struct SolveBody {
    task: String,
    output: String,
}
async fn net_solve(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(b): Json<SolveBody>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let (c, k) = net_ctx(&state)?;
    let r = revenant_net::quest::TaskResult::create(&k, id, b.task, b.output, net_now());
    c.post_result(&r).await?;
    Ok(Json(json!({ "ok": true, "result_id": r.id })))
}

#[derive(Deserialize)]
struct AcceptBody {
    task: String,
    result_id: String,
}
async fn net_accept(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(b): Json<AcceptBody>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let (c, k) = net_ctx(&state)?;
    let a = revenant_net::quest::TaskAccept::create(&k, id, b.task, b.result_id, net_now());
    Ok(Json(c.accept_result(&a).await?))
}

#[derive(Deserialize)]
struct CloseBody {
    #[serde(default)]
    withdraw: bool,
}
async fn net_close(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(b): Json<CloseBody>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let (c, k) = net_ctx(&state)?;
    // Proof gate: a quest with unsettled tasks can only be *withdrawn*, never
    // passed off as completed. Mirror the tool's guard so the UI can't fake it.
    let state_json = c.quest(&id).await?;
    let unsettled: Vec<String> = state_json
        .get("tasks")
        .and_then(|t| t.as_array())
        .map(|ts| {
            ts.iter()
                .filter(|t| t.get("status").and_then(|s| s.as_str()) != Some("solved"))
                .filter_map(|t| t.get("id").and_then(|i| i.as_str()).map(String::from))
                .collect()
        })
        .unwrap_or_default();
    if !unsettled.is_empty() && !b.withdraw {
        return Err(ApiError::bad_request(format!(
            "quest has {} unsettled task(s) ({}) — nothing proven solved. Closing now is a withdrawal, not a completion; pass withdraw=true to abandon the unsolved work.",
            unsettled.len(),
            unsettled.join(", ")
        )));
    }
    let close = revenant_net::quest::QuestClose::create(&k, id, net_now());
    c.close_quest(&close).await?;
    Ok(Json(json!({ "ok": true, "withdrawn": !unsettled.is_empty() })))
}

#[derive(Deserialize)]
struct BoostBody {
    target: String,
    amount: u64,
}
async fn net_boost(
    State(state): State<AppState>,
    Json(b): Json<BoostBody>,
) -> Result<Json<serde_json::Value>, ApiError> {
    if b.amount == 0 {
        return Err(ApiError::bad_request("a boost must spend at least 1 credit"));
    }
    let (c, k) = net_ctx(&state)?;
    let boost = revenant_net::boost::Boost::create(&k, b.target, b.amount, net_now());
    c.boost(&boost).await?;
    Ok(Json(json!({ "ok": true })))
}

/// One-click install of a skill from the Bazaar: pull (verifies signature +
/// content hash), confirm it's a skill, write it, and reindex — the same path
/// the `skill_adopt` tool takes.
async fn net_install(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let (c, k) = net_ctx(&state)?;
    // Resolve short id / title → full artifact id.
    let full = id.len() == 64 && id.chars().all(|ch| ch.is_ascii_hexdigit());
    let full_id = if full {
        id.clone()
    } else {
        let items = c.list(Some("skill")).await?;
        items
            .iter()
            .find(|a| {
                a["title"].as_str().is_some_and(|t| t.eq_ignore_ascii_case(&id))
                    || a["id"].as_str().is_some_and(|aid| aid.starts_with(&id))
            })
            .and_then(|a| a["id"].as_str().map(String::from))
            .ok_or_else(|| ApiError::bad_request(format!("no skill matching \"{id}\" on the network")))?
    };
    let artifact = c.pull(&full_id).await.map_err(|e| ApiError::bad_request(format!("install refused: {e:#}")))?;
    if artifact.kind != revenant_net::ArtifactKind::Skill {
        return Err(ApiError::bad_request(format!("artifact is a {:?}, not a skill", artifact.kind)));
    }
    let payload = artifact.payload()?;
    let slug: String = artifact
        .title
        .to_lowercase()
        .chars()
        .map(|ch| if ch.is_ascii_alphanumeric() { ch } else { '-' })
        .collect::<String>()
        .trim_matches('-')
        .to_string();
    let slug = if slug.is_empty() { full_id[..12].to_string() } else { slug };
    let dir = state.home.skills_dir().join(&slug);
    std::fs::create_dir_all(&dir).map_err(|e| ApiError::from(anyhow::anyhow!(e)))?;
    std::fs::write(dir.join("SKILL.md"), &payload).map_err(|e| ApiError::from(anyhow::anyhow!(e)))?;
    let _ = state.manager.runtime().skills.scan(); // so use_skill sees it now
    // Best-effort adoption attestation (feeds the author's reputation).
    let _ = c.attest(&full_id, &k.id(), true).await;
    Ok(Json(json!({ "ok": true, "slug": slug, "title": artifact.title })))
}

// ---- distributed thinking: orchestrate a run across the private horde board --
//
// One goal → decomposed (one LLM call) into subtasks posted to the account's
// private board → the horde's workers claim + solve them → gathered + synthesized
// (one LLM call) into a final answer. The board is the durable source of truth;
// the UI polls /v1/net/horde/run/:run to watch the fan-out.

/// One tier'd, non-streaming LLM call returning the joined text. Transient
/// provider pressure (529 Overloaded) is retried with backoff — the agent turn
/// pipeline already rides these out; a single raw call must too.
async fn llm_text(state: &AppState, system: &str, user: String, max_tokens: u32) -> Result<String, ApiError> {
    let req = revenant_llm::MessagesRequest {
        model: state.default_tier.to_string(),
        max_tokens,
        system: Some(serde_json::Value::String(system.to_string())),
        messages: vec![revenant_llm::WireMessage::new(
            revenant_core::Role::User,
            vec![revenant_core::ContentBlock::text(user)],
        )],
        tools: vec![],
        tool_choice: None,
        stream: true,
        identity: Some("horde-orchestrator".to_string()),
    };
    let llm = state.manager.runtime().llm.clone();
    let mut outcome = Err(anyhow::anyhow!("unreached"));
    for (attempt, delay_ms) in [(1u32, 0u64), (2, 2000), (3, 6000)] {
        if delay_ms > 0 {
            tokio::time::sleep(std::time::Duration::from_millis(delay_ms)).await;
        }
        outcome = llm.stream_message(&req, |_| {}).await;
        match &outcome {
            Ok(_) => break,
            Err(e) if format!("{e:#}").to_lowercase().contains("overloaded") => {
                tracing::debug!("llm_text: provider overloaded (attempt {attempt}) — backing off");
            }
            Err(_) => break, // non-transient: surface it
        }
    }
    let outcome = outcome.map_err(ApiError::from)?;
    Ok(outcome
        .content
        .iter()
        .filter_map(|b| match b {
            revenant_core::ContentBlock::Text { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join(""))
}

#[derive(Deserialize)]
struct RunStartBody {
    goal: String,
    #[serde(default)]
    subtasks: Option<usize>,
}

/// Start a distributed run: decompose the goal, post subtasks to the board.
async fn net_horde_run_start(
    State(state): State<AppState>,
    Json(b): Json<RunStartBody>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let goal = b.goal.trim().to_string();
    if goal.is_empty() {
        return Err(ApiError::bad_request("a goal is required"));
    }
    let n = b.subtasks.unwrap_or(4).clamp(1, 8);
    let (c, k) = net_ctx(&state)?;

    // Decompose into independent subtasks (JSON array of {title, spec}).
    let sys = "You break a goal into INDEPENDENT subtasks that can be solved in parallel by separate \
agents, then combined. Return ONLY a JSON array (no prose) of objects with \"title\" (short) and \
\"spec\" (a self-contained instruction). Each subtask must stand alone — no subtask may depend on \
another's output. Prefer fewer, meatier subtasks over many trivial ones.";
    let raw = llm_text(
        &state,
        sys,
        format!("GOAL: {goal}\n\nDecompose into at most {n} independent subtasks. JSON array only."),
        1200,
    )
    .await?;
    let subtasks = parse_subtasks(&raw, n);
    if subtasks.is_empty() {
        return Err(ApiError::bad_request("could not decompose the goal into subtasks"));
    }

    // A run id anchored to the goal + time so concurrent runs never collide.
    let now = net_now();
    let mut hsh = sha2::Sha256::new();
    use sha2::Digest;
    hsh.update(goal.as_bytes());
    hsh.update(now.to_le_bytes());
    let run = format!("run-{}", &hex::encode(hsh.finalize())[..16]);

    let mut posted = Vec::new();
    for (title, spec) in &subtasks {
        let t = revenant_net::horde::HordeTask::create(&k, &run, title.clone(), spec.clone(), vec![], now);
        c.post_horde_task(&t).await?;
        posted.push(json!({ "id": t.id, "title": title, "spec": spec }));
    }
    Ok(Json(json!({ "run": run, "goal": goal, "tasks": posted })))
}

/// Best-effort parse of an LLM JSON array of {title, spec}.
fn parse_subtasks(raw: &str, max: usize) -> Vec<(String, String)> {
    let start = raw.find('[');
    let end = raw.rfind(']');
    let slice = match (start, end) {
        (Some(s), Some(e)) if e > s => &raw[s..=e],
        _ => return Vec::new(),
    };
    let arr: Vec<serde_json::Value> = serde_json::from_str(slice).unwrap_or_default();
    arr.into_iter()
        .filter_map(|v| {
            let title = v.get("title").and_then(|x| x.as_str()).unwrap_or("").trim().to_string();
            let spec = v.get("spec").and_then(|x| x.as_str()).unwrap_or("").trim().to_string();
            if spec.is_empty() {
                None
            } else {
                Some((if title.is_empty() { spec.chars().take(48).collect() } else { title }, spec))
            }
        })
        .take(max)
        .collect()
}

/// Poll a run's live state (proxies the board): each subtask's status + output.
async fn net_horde_run_get(
    State(state): State<AppState>,
    Path(run): Path<String>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let (c, k) = net_ctx(&state)?;
    Ok(Json(c.horde_run(&run, &k).await?))
}

#[derive(Deserialize)]
struct SynthBody {
    goal: String,
    run: String,
}

/// Gather a run's solved subtasks and synthesize the final answer.
async fn net_horde_synthesize(
    State(state): State<AppState>,
    Json(b): Json<SynthBody>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let (c, k) = net_ctx(&state)?;
    let run = c.horde_run(&b.run, &k).await?;
    let empty = vec![];
    let tasks = run.get("tasks").and_then(|t| t.as_array()).unwrap_or(&empty);
    let mut parts = String::new();
    let mut used = 0usize;
    for t in tasks {
        if t.get("status").and_then(|s| s.as_str()) == Some("solved") {
            let title = t.get("title").and_then(|x| x.as_str()).unwrap_or("subtask");
            let out = t.get("output").and_then(|x| x.as_str()).unwrap_or("");
            parts.push_str(&format!("### {title}\n{out}\n\n"));
            used += 1;
        }
    }
    if used == 0 {
        return Err(ApiError::bad_request("no solved subtasks to synthesize yet"));
    }
    let sys = "You are synthesizing the results of subtasks your horde solved in parallel into one \
coherent answer to the original goal. Integrate the pieces, resolve overlaps, and produce the final \
deliverable — not a summary of who did what.";
    let answer = llm_text(
        &state,
        sys,
        format!("ORIGINAL GOAL: {}\n\nSUBTASK RESULTS:\n\n{parts}\nProduce the final answer.", b.goal),
        2000,
    )
    .await?;
    Ok(Json(json!({ "answer": answer.trim(), "synthesized_from": used })))
}

// ---- error plumbing ----

pub struct ApiError {
    status: StatusCode,
    message: String,
}

impl ApiError {
    fn bad_request(msg: impl std::fmt::Display) -> Self {
        ApiError { status: StatusCode::BAD_REQUEST, message: msg.to_string() }
    }
}

impl From<anyhow::Error> for ApiError {
    fn from(err: anyhow::Error) -> Self {
        ApiError { status: StatusCode::INTERNAL_SERVER_ERROR, message: format!("{err:#}") }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> axum::response::Response {
        (self.status, Json(json!({ "error": self.message }))).into_response()
    }
}
