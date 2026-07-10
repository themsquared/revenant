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
    event_seq: Arc<AtomicU64>,
}

impl AppState {
    pub fn new(
        manager: SessionManager,
        token: String,
        default_tier: Tier,
        gateway_probe: revenant_llm::LlmClient,
    ) -> Self {
        AppState {
            manager,
            token,
            default_tier,
            gateway_probe,
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
        .route("/v1/approvals", get(approvals_pending))
        .route("/v1/approvals/:id/decision", post(approval_decide))
        .route("/v1/skills", get(skills_list))
        .route("/v1/tools", get(tools_list))
        .route("/v1/subagents", get(subagents_list))
        .route("/v1/spend", get(spend))
        .route("/v1/memory/status", get(memory_status))
        .route("/v1/gateway/status", get(gateway_status))
        .route("/v1/channels/pairings", post(pairing_create))
        .layer(axum::middleware::from_fn_with_state(state.clone(), auth))
        .with_state(state);

    api.fallback(serve_ui)
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
                revenant_core::Event::GatewayStatus { .. } => "gateway_status",
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
    let peer = body.peer.unwrap_or_else(|| "local".to_string());
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
    state
        .manager
        .submit(id, SessionMsg::UserInput { content: body.text, tier })
        .await?;
    Ok((StatusCode::ACCEPTED, Json(json!({ "accepted": true, "session_id": id }))))
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
        .resolve(&id, body.approve, &resolver)
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
