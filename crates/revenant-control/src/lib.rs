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
        .route("/v1/analytics", get(analytics))
        .route("/v1/memory/status", get(memory_status))
        .route("/v1/gateway/status", get(gateway_status))
        .route("/v1/channels/pairings", post(pairing_create))
        // A2A: revenant as a callable node in the agent mesh (authed).
        .route("/a2a", post(a2a_message))
        .layer(axum::middleware::from_fn_with_state(state.clone(), auth))
        .with_state(state.clone());

    // The A2A agent card is discovery — served unauthenticated (loopback).
    Router::new()
        .route("/.well-known/agent-card.json", get(agent_card))
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

/// A2A JSON-RPC endpoint. Implements `message/send`: extract the text parts,
/// run a full revenant turn, return the reply as an A2A agent message.
async fn a2a_message(
    State(state): State<AppState>,
    Json(rpc): Json<A2aRpc>,
) -> Json<serde_json::Value> {
    let rpc_err = |id: &serde_json::Value, code: i64, msg: &str| {
        Json(json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": msg } }))
    };
    if rpc.method != "message/send" {
        return rpc_err(&rpc.id, -32601, &format!("unsupported A2A method '{}'", rpc.method));
    }
    // Concatenate the text parts of the incoming message.
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
        return rpc_err(&rpc.id, -32602, "message has no text parts");
    }

    let runtime = state.manager.runtime();
    let session_id = match runtime.store.ensure_session("a2a", "peer", "chat").await {
        Ok(id) => id,
        Err(err) => return rpc_err(&rpc.id, -32603, &format!("session: {err:#}")),
    };
    match runtime
        .run_turn(session_id, state.default_tier, vec![revenant_core::ContentBlock::text(text)])
        .await
    {
        Ok(stats) => Json(json!({
            "jsonrpc": "2.0",
            "id": rpc.id,
            "result": {
                "role": "agent",
                "parts": [{ "kind": "text", "text": stats.final_text }],
                "kind": "message"
            }
        })),
        Err(err) => rpc_err(&rpc.id, -32603, &format!("turn failed: {err:#}")),
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
