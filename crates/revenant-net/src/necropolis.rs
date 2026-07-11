//! The Necropolis: the central directory where the horde musters. Revenants
//! register their identity + endpoint + capabilities and discover peers; they
//! publish artifact metadata here and (M1) the artifact bytes too, so the
//! catalog is queryable. It is deliberately thin — it holds no keys, signs
//! nothing, and is NOT trusted for authenticity: every artifact is verified
//! against its author's signature on publish and again by the receiver. The
//! directory is a phonebook, not an authority.

use crate::artifact::{Artifact, ArtifactKind};
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

#[derive(Debug, Clone, Default, Serialize)]
pub struct Reputation {
    /// Artifacts this identity has published.
    pub published: u32,
    /// Times a peer reported re-running this identity's eval proof and passing.
    pub adopted: u32,
}

#[derive(Debug, Clone, Serialize)]
pub struct Peer {
    pub id: String,
    pub endpoint: String,
    pub capabilities: Vec<String>,
    pub reputation: Reputation,
}

#[derive(Default)]
pub struct Directory {
    peers: BTreeMap<String, Peer>,
    artifacts: BTreeMap<String, Artifact>,
}

pub type SharedDir = Arc<Mutex<Directory>>;

pub fn router(dir: SharedDir) -> Router {
    Router::new()
        .route("/health", get(|| async { "necropolis ok" }))
        .route("/register", post(register))
        .route("/peers", get(peers))
        .route("/artifacts", post(publish).get(list))
        .route("/artifacts/:id", get(fetch))
        .route("/artifacts/:id/attest", post(attest))
        .with_state(dir)
}

#[derive(Deserialize)]
struct RegisterReq {
    id: String,
    endpoint: String,
    #[serde(default)]
    capabilities: Vec<String>,
}

async fn register(
    State(dir): State<SharedDir>,
    Json(req): Json<RegisterReq>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    if req.id.len() != 64 || hex::decode(&req.id).is_err() {
        return Err((StatusCode::BAD_REQUEST, "id must be a 64-hex public key".into()));
    }
    let mut d = dir.lock().unwrap();
    let existing_rep = d.peers.get(&req.id).map(|p| p.reputation.clone()).unwrap_or_default();
    d.peers.insert(
        req.id.clone(),
        Peer {
            id: req.id.clone(),
            endpoint: req.endpoint,
            capabilities: req.capabilities,
            reputation: existing_rep,
        },
    );
    Ok(Json(serde_json::json!({ "ok": true, "peers": d.peers.len() })))
}

async fn peers(State(dir): State<SharedDir>) -> Json<Vec<Peer>> {
    Json(dir.lock().unwrap().peers.values().cloned().collect())
}

async fn publish(
    State(dir): State<SharedDir>,
    Json(artifact): Json<Artifact>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    // The directory refuses to catalog anything that doesn't verify — a bad
    // signature or a mismatched content hash never enters the catalog.
    if !artifact.verify() {
        return Err((StatusCode::BAD_REQUEST, "artifact failed signature/hash verification".into()));
    }
    let mut d = dir.lock().unwrap();
    let author = artifact.author.clone();
    let id = artifact.id.clone();
    d.artifacts.insert(id.clone(), artifact);
    // Ensure the author has a peer record, then credit the publish. An author
    // may publish before registering an endpoint (reputation still accrues).
    let peer = d.peers.entry(author.clone()).or_insert_with(|| Peer {
        id: author,
        endpoint: String::new(),
        capabilities: vec![],
        reputation: Reputation::default(),
    });
    peer.reputation.published += 1;
    Ok(Json(serde_json::json!({ "ok": true, "id": id })))
}

#[derive(Deserialize)]
struct ListQuery {
    kind: Option<String>,
}

async fn list(State(dir): State<SharedDir>, Query(q): Query<ListQuery>) -> Json<Vec<serde_json::Value>> {
    let want: Option<ArtifactKind> =
        q.kind.and_then(|k| serde_json::from_value(serde_json::Value::String(k)).ok());
    let d = dir.lock().unwrap();
    Json(
        d.artifacts
            .values()
            .filter(|a| want.is_none_or(|w| a.kind == w))
            .map(|a| a.summary())
            .collect(),
    )
}

async fn fetch(
    State(dir): State<SharedDir>,
    Path(id): Path<String>,
) -> Result<Json<Artifact>, (StatusCode, String)> {
    dir.lock()
        .unwrap()
        .artifacts
        .get(&id)
        .cloned()
        .map(Json)
        .ok_or((StatusCode::NOT_FOUND, "no such artifact".into()))
}

#[derive(Deserialize)]
struct AttestReq {
    /// The peer reporting a successful local re-verification.
    verifier: String,
    passed: bool,
}

/// A peer reports it re-ran the artifact's eval proof locally and it passed —
/// the trust loop. Bumps the *author's* adopted count.
async fn attest(
    State(dir): State<SharedDir>,
    Path(id): Path<String>,
    Json(req): Json<AttestReq>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let mut d = dir.lock().unwrap();
    let Some(author) = d.artifacts.get(&id).map(|a| a.author.clone()) else {
        return Err((StatusCode::NOT_FOUND, "no such artifact".into()));
    };
    if req.passed {
        if let Some(p) = d.peers.get_mut(&author) {
            p.reputation.adopted += 1;
        }
    }
    let _ = req.verifier;
    Ok(Json(serde_json::json!({ "ok": true })))
}

/// Bind and serve the directory on `addr` until the process ends.
pub async fn serve(addr: std::net::SocketAddr, dir: SharedDir) -> anyhow::Result<()> {
    let listener = tokio::net::TcpListener::bind(addr).await?;
    tracing::info!("necropolis listening on {addr}");
    axum::serve(listener, router(dir)).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::identity::Identity;
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt; // oneshot

    fn body_json(bytes: &[u8]) -> serde_json::Value {
        serde_json::from_slice(bytes).unwrap()
    }

    #[tokio::test]
    async fn publish_rejects_tampered_artifact() {
        let dir: SharedDir = Arc::new(Mutex::new(Directory::default()));
        let app = router(dir);
        let k = Identity::load_or_create(tempfile::tempdir().unwrap().path()).unwrap();
        let mut a = Artifact::create(&k, ArtifactKind::Skill, "t", "d", b"x", None, 1);
        a.title = "tampered".into(); // invalidate signature
        let resp = app
            .oneshot(
                Request::post("/artifacts")
                    .header("content-type", "application/json")
                    .body(Body::from(serde_json::to_vec(&a).unwrap()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn publish_then_list_and_fetch() {
        let dir: SharedDir = Arc::new(Mutex::new(Directory::default()));
        let k = Identity::load_or_create(tempfile::tempdir().unwrap().path()).unwrap();
        let a = Artifact::create(&k, ArtifactKind::Skill, "weather-arb", "d", b"payload", None, 1);
        let id = a.id.clone();

        let r = router(dir.clone())
            .oneshot(
                Request::post("/artifacts")
                    .header("content-type", "application/json")
                    .body(Body::from(serde_json::to_vec(&a).unwrap()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(r.status(), StatusCode::OK);

        let r = router(dir.clone())
            .oneshot(Request::get("/artifacts?kind=skill").body(Body::empty()).unwrap())
            .await
            .unwrap();
        let bytes = axum::body::to_bytes(r.into_body(), usize::MAX).await.unwrap();
        let listing = body_json(&bytes);
        assert_eq!(listing.as_array().unwrap().len(), 1);
        assert_eq!(listing[0]["title"], "weather-arb");

        let r = router(dir)
            .oneshot(Request::get(format!("/artifacts/{id}")).body(Body::empty()).unwrap())
            .await
            .unwrap();
        let bytes = axum::body::to_bytes(r.into_body(), usize::MAX).await.unwrap();
        let fetched: Artifact = serde_json::from_slice(&bytes).unwrap();
        assert!(fetched.verify());
        assert_eq!(fetched.payload().unwrap(), b"payload");
    }
}
