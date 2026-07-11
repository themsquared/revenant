//! The Necropolis: the directory where the horde musters. It is now backed by
//! a durable hash-linked [`Ledger`] — every publish and attestation is an
//! append-only, tamper-evident entry, and the queryable catalog + reputation
//! are *derived* by replaying the log on open. It holds no keys and signs
//! nothing: authenticity is each artifact's own signature, verified on the way
//! in and again by every receiver. Replicas sync by pulling `/ledger/since`
//! and re-verifying the chain — federation without consensus.

use crate::artifact::{Artifact, ArtifactKind};
use crate::ledger::{Entry, Ledger};
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

#[derive(Debug, Clone, Default, Serialize)]
pub struct Reputation {
    pub published: u32,
    pub adopted: u32,
}

#[derive(Debug, Clone, Serialize)]
pub struct Peer {
    pub id: String,
    pub endpoint: String,
    pub capabilities: Vec<String>,
    pub reputation: Reputation,
}

pub struct Directory {
    ledger: Ledger,
    peers: BTreeMap<String, Peer>,
    artifacts: BTreeMap<String, Artifact>,
}

pub type SharedDir = Arc<Mutex<Directory>>;

impl Directory {
    /// Open a directory backed by a ledger file (`":memory:"` for ephemeral),
    /// verifying the chain and replaying it to rebuild the catalog + reputation.
    pub fn open(ledger_path: &str) -> anyhow::Result<Self> {
        let ledger = Ledger::open(ledger_path)?;
        ledger.verify_chain()?; // refuse to serve a tampered history
        let mut dir = Directory { ledger, peers: BTreeMap::new(), artifacts: BTreeMap::new() };
        for e in dir.ledger.since(0)? {
            dir.apply(&e);
        }
        Ok(dir)
    }

    pub fn in_memory() -> Self {
        Self::open(":memory:").expect("in-memory ledger opens")
    }

    /// Number of entries in the (verified) ledger — for startup logging.
    pub fn ledger_len(&self) -> anyhow::Result<usize> {
        self.ledger.since(0).map(|v| v.len())
    }

    /// This directory's current ledger head sequence — the cursor a replica
    /// hands a peer to pull only what it is missing.
    pub fn head_seq(&self) -> anyhow::Result<i64> {
        self.ledger.head_seq()
    }

    /// Federate: fold a batch of entries pulled from a peer into this
    /// directory, trusting none of it. The batch is first checked to chain
    /// cleanly onto our own head — every `prev_hash` link and every recomputed
    /// content hash — so a forked or tampered stream is rejected whole, before
    /// a single row is written. Returns how many new entries were applied
    /// (0 if we were already current). Fails closed.
    pub fn apply_remote(&mut self, entries: &[Entry]) -> anyhow::Result<usize> {
        use crate::ledger::Ledger;
        // Pre-validate the whole batch against our head — atomic in spirit:
        // nothing is written unless the entire chain checks out.
        let mut prev = self.ledger.head_hash()?;
        for e in entries {
            if e.prev_hash != prev {
                anyhow::bail!("sync rejected: entry {} does not chain onto our history (fork?)", e.seq);
            }
            if Ledger::entry_hash(&e.prev_hash, &e.kind, &e.body) != e.hash {
                anyhow::bail!("sync rejected: entry {} hash mismatch (tampered payload)", e.seq);
            }
            prev = e.hash.clone();
        }
        // The batch is sound; commit and derive.
        let mut applied = 0;
        for e in entries {
            self.ledger.append_verified(e)?;
            self.apply(e);
            applied += 1;
        }
        Ok(applied)
    }

    /// Fold one ledger entry into the derived indices (used by both startup
    /// replay and live appends).
    fn apply(&mut self, e: &Entry) {
        match e.kind.as_str() {
            "artifact" => {
                if let Ok(a) = serde_json::from_str::<Artifact>(&e.body) {
                    bump(&mut self.peers, &a.author, |r| r.published += 1);
                    self.artifacts.insert(a.id.clone(), a);
                }
            }
            "attest" => {
                if let Ok(v) = serde_json::from_str::<serde_json::Value>(&e.body) {
                    let passed = v["passed"].as_bool().unwrap_or(false);
                    let author = v["author"].as_str().unwrap_or("").to_string();
                    if passed && !author.is_empty() {
                        bump(&mut self.peers, &author, |r| r.adopted += 1);
                    }
                }
            }
            _ => {}
        }
    }
}

fn bump(peers: &mut BTreeMap<String, Peer>, id: &str, f: impl FnOnce(&mut Reputation)) {
    let p = peers.entry(id.to_string()).or_insert_with(|| Peer {
        id: id.to_string(),
        endpoint: String::new(),
        capabilities: vec![],
        reputation: Reputation::default(),
    });
    f(&mut p.reputation);
}

impl Default for Directory {
    fn default() -> Self {
        Self::in_memory()
    }
}

pub fn router(dir: SharedDir) -> Router {
    Router::new()
        .route("/health", get(|| async { "necropolis ok" }))
        .route("/register", post(register))
        .route("/peers", get(peers))
        .route("/artifacts", post(publish).get(list))
        .route("/artifacts/:id", get(fetch))
        .route("/artifacts/:id/attest", post(attest))
        .route("/ledger/head", get(ledger_head))
        .route("/ledger/since/:seq", get(ledger_since))
        // The catalog is public read — allow any origin so the static skills
        // marketplace (Netlify) can fetch it cross-origin. Authenticity is the
        // per-artifact signature, never the origin, so `*` is safe here.
        .layer(axum::middleware::from_fn(cors))
        .with_state(dir)
}

/// Add a permissive CORS header so browser clients (the marketplace) can read
/// the directory cross-origin. GET/POST here carry no custom headers, so these
/// are "simple" requests needing no preflight.
async fn cors(req: axum::extract::Request, next: axum::middleware::Next) -> axum::response::Response {
    let mut resp = next.run(req).await;
    resp.headers_mut().insert(
        axum::http::header::ACCESS_CONTROL_ALLOW_ORIGIN,
        axum::http::HeaderValue::from_static("*"),
    );
    resp
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
    // Presence (endpoint/capabilities) is ephemeral, not ledgered; reputation
    // is preserved from the replayed history.
    let rep = d.peers.get(&req.id).map(|p| p.reputation.clone()).unwrap_or_default();
    d.peers.insert(
        req.id.clone(),
        Peer { id: req.id, endpoint: req.endpoint, capabilities: req.capabilities, reputation: rep },
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
    if !artifact.verify() {
        return Err((StatusCode::BAD_REQUEST, "artifact failed signature/hash verification".into()));
    }
    let body = serde_json::to_string(&artifact).map_err(ise)?;
    let id = artifact.id.clone();
    let mut d = dir.lock().unwrap();
    let entry = d.ledger.append("artifact", &body, artifact.created_ts).map_err(ise)?;
    d.apply(&entry);
    Ok(Json(serde_json::json!({ "ok": true, "id": id, "seq": entry.seq })))
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
    verifier: String,
    passed: bool,
}

async fn attest(
    State(dir): State<SharedDir>,
    Path(id): Path<String>,
    Json(req): Json<AttestReq>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let mut d = dir.lock().unwrap();
    let Some(author) = d.artifacts.get(&id).map(|a| a.author.clone()) else {
        return Err((StatusCode::NOT_FOUND, "no such artifact".into()));
    };
    // Record who the credit accrues to inside the entry so replay is
    // self-contained (a replica needn't hold the artifact to apply the attest).
    let body = serde_json::json!({
        "artifact_id": id, "author": author, "verifier": req.verifier, "passed": req.passed
    })
    .to_string();
    let ts = d.artifacts.get(&id).map(|a| a.created_ts).unwrap_or(0);
    let entry = d.ledger.append("attest", &body, ts).map_err(ise)?;
    d.apply(&entry);
    Ok(Json(serde_json::json!({ "ok": true, "seq": entry.seq })))
}

async fn ledger_head(State(dir): State<SharedDir>) -> Json<serde_json::Value> {
    let d = dir.lock().unwrap();
    Json(serde_json::json!({
        "seq": d.ledger.head_seq().unwrap_or(0),
        "hash": d.ledger.head_hash().unwrap_or_default(),
    }))
}

async fn ledger_since(
    State(dir): State<SharedDir>,
    Path(seq): Path<i64>,
) -> Result<Json<Vec<Entry>>, (StatusCode, String)> {
    dir.lock().unwrap().ledger.since(seq).map(Json).map_err(ise)
}

fn ise<E: std::fmt::Display>(e: E) -> (StatusCode, String) {
    (StatusCode::INTERNAL_SERVER_ERROR, e.to_string())
}

/// Pull a peer's ledger into `dir` once, re-verifying the chain locally.
/// Returns the number of new entries applied. The lock is never held across
/// the network await: we read our cursor, release, fetch, then re-acquire to
/// apply — so serving traffic is never blocked on a slow peer.
pub async fn sync_once(dir: &SharedDir, peer: &crate::NecropolisClient) -> anyhow::Result<usize> {
    let since = { dir.lock().unwrap().head_seq()? };
    let incoming = peer.ledger_since(since).await?;
    if incoming.is_empty() {
        return Ok(0);
    }
    let mut d = dir.lock().unwrap();
    d.apply_remote(&incoming)
}

/// Federate forever: every `interval`, sync `dir` from each peer. Failures
/// (an unreachable peer, a forked chain) are logged and skipped — one bad peer
/// never takes the directory down. Spawn this alongside [`serve`].
pub async fn federate(dir: SharedDir, peers: Vec<String>, interval: std::time::Duration) {
    if peers.is_empty() {
        return;
    }
    let clients: Vec<_> = peers.iter().map(crate::NecropolisClient::new).collect();
    let mut tick = tokio::time::interval(interval);
    loop {
        tick.tick().await;
        for (url, client) in peers.iter().zip(&clients) {
            match sync_once(&dir, client).await {
                Ok(0) => {}
                Ok(n) => tracing::info!("federate: applied {n} new entries from {url}"),
                Err(e) => tracing::warn!("federate: sync from {url} skipped: {e}"),
            }
        }
    }
}

/// Bind and serve until the process ends.
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
    use tower::ServiceExt;

    fn shared() -> SharedDir {
        Arc::new(Mutex::new(Directory::in_memory()))
    }

    #[tokio::test]
    async fn publish_rejects_tampered_artifact() {
        let k = Identity::load_or_create(tempfile::tempdir().unwrap().path()).unwrap();
        let mut a = Artifact::create(&k, ArtifactKind::Skill, "t", "d", b"x", None, 1);
        a.title = "tampered".into();
        let resp = router(shared())
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
    async fn publish_is_ledgered_and_derives_catalog() {
        let dir = shared();
        let k = Identity::load_or_create(tempfile::tempdir().unwrap().path()).unwrap();
        let a = Artifact::create(&k, ArtifactKind::Skill, "weather-arb", "d", b"payload", None, 1);

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

        // The ledger recorded it and the chain verifies.
        assert_eq!(dir.lock().unwrap().ledger.verify_chain().unwrap(), 1);
        // Catalog + reputation were derived from the entry.
        assert_eq!(dir.lock().unwrap().artifacts.len(), 1);
        assert_eq!(dir.lock().unwrap().peers[&k.id()].reputation.published, 1);
    }

    #[test]
    fn catalog_survives_restart_via_replay() {
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("n.db").to_string_lossy().to_string();
        let k = Identity::load_or_create(tempfile::tempdir().unwrap().path()).unwrap();
        let a = Artifact::create(&k, ArtifactKind::Plugin, "tool", "d", b"wasm", None, 7);
        {
            let d = Directory::open(&p).unwrap();
            let body = serde_json::to_string(&a).unwrap();
            let e = d.ledger.append("artifact", &body, 7).unwrap();
            // (In the server this happens inside publish; here we drive the ledger
            // directly to prove replay rebuilds state on a fresh open.)
            let _ = e;
        }
        // Reopen: the catalog is reconstructed purely from the ledger.
        let d2 = Directory::open(&p).unwrap();
        assert_eq!(d2.artifacts.len(), 1);
        assert!(d2.artifacts.contains_key(&a.id));
        assert_eq!(d2.peers[&k.id()].reputation.published, 1);
    }

    // --- federation: replica sync (apply_remote) ------------------------

    /// Publish an artifact into a directory the way the server does — append to
    /// the ledger and fold it into the derived indices — returning it.
    fn seed(dir: &mut Directory, k: &Identity, kind: ArtifactKind, title: &str, ts: i64) -> Artifact {
        let a = Artifact::create(k, kind, title, "d", title.as_bytes(), None, ts);
        let e = dir.ledger.append("artifact", &serde_json::to_string(&a).unwrap(), ts).unwrap();
        dir.apply(&e);
        a
    }

    #[test]
    fn federation_replicates_the_whole_catalog() {
        let k = Identity::load_or_create(tempfile::tempdir().unwrap().path()).unwrap();
        let mut origin = Directory::in_memory();
        let a1 = seed(&mut origin, &k, ArtifactKind::Skill, "weather-arb", 1);
        let a2 = seed(&mut origin, &k, ArtifactKind::Plugin, "port-scan", 2);

        let mut replica = Directory::in_memory();
        let applied = replica.apply_remote(&origin.ledger.since(0).unwrap()).unwrap();

        assert_eq!(applied, 2);
        assert_eq!(replica.artifacts.len(), 2);
        assert!(replica.artifacts.contains_key(&a1.id));
        assert!(replica.artifacts.contains_key(&a2.id));
        // Reputation was re-derived on the replica, not trusted from the wire.
        assert_eq!(replica.peers[&k.id()].reputation.published, 2);
        // And the replica's own chain audit passes, head-for-head with origin.
        assert_eq!(replica.ledger.verify_chain().unwrap(), 2);
        assert_eq!(replica.ledger.head_hash().unwrap(), origin.ledger.head_hash().unwrap());
    }

    #[test]
    fn federation_is_idempotent_and_incremental() {
        let k = Identity::load_or_create(tempfile::tempdir().unwrap().path()).unwrap();
        let mut origin = Directory::in_memory();
        seed(&mut origin, &k, ArtifactKind::Skill, "one", 1);

        let mut replica = Directory::in_memory();
        assert_eq!(replica.apply_remote(&origin.ledger.since(0).unwrap()).unwrap(), 1);

        // Re-syncing from the replica's head pulls nothing and applies nothing.
        let since = replica.head_seq().unwrap();
        assert_eq!(replica.apply_remote(&origin.ledger.since(since).unwrap()).unwrap(), 0);

        // Origin advances; an incremental sync applies only the new entry.
        seed(&mut origin, &k, ArtifactKind::Signal, "two", 2);
        let since = replica.head_seq().unwrap();
        assert_eq!(replica.apply_remote(&origin.ledger.since(since).unwrap()).unwrap(), 1);
        assert_eq!(replica.artifacts.len(), 2);
    }

    #[test]
    fn federation_rejects_a_forked_stream() {
        let k = Identity::load_or_create(tempfile::tempdir().unwrap().path()).unwrap();
        // Replica already has its own history.
        let mut replica = Directory::in_memory();
        seed(&mut replica, &k, ArtifactKind::Skill, "local", 1);

        // A peer whose chain forked from genesis — its entries don't chain onto
        // the replica's head. The whole batch must be refused, nothing written.
        let mut fork = Directory::in_memory();
        seed(&mut fork, &k, ArtifactKind::Skill, "theirs", 9);

        let before = replica.artifacts.len();
        let err = replica.apply_remote(&fork.ledger.since(0).unwrap()).unwrap_err();
        assert!(err.to_string().contains("does not chain"), "got: {err}");
        assert_eq!(replica.artifacts.len(), before, "a rejected sync must not mutate state");
    }

    #[test]
    fn federation_rejects_a_tampered_body_atomically() {
        let k = Identity::load_or_create(tempfile::tempdir().unwrap().path()).unwrap();
        let mut origin = Directory::in_memory();
        seed(&mut origin, &k, ArtifactKind::Skill, "good", 1);
        seed(&mut origin, &k, ArtifactKind::Skill, "also-good", 2);

        let mut entries = origin.ledger.since(0).unwrap();
        // Corrupt the second entry's payload without fixing its hash.
        entries[1].body = r#"{"id":"evil"}"#.into();

        let mut replica = Directory::in_memory();
        assert!(replica.apply_remote(&entries).is_err());
        // Pre-validation means the *first*, valid entry was not applied either.
        assert_eq!(replica.artifacts.len(), 0, "a tampered batch is rejected whole");
        assert_eq!(replica.ledger.head_seq().unwrap(), 0);
    }

    #[tokio::test]
    async fn federation_end_to_end_over_http() {
        // A real origin server on an ephemeral port.
        let origin = shared();
        let k = Identity::load_or_create(tempfile::tempdir().unwrap().path()).unwrap();
        let a = Artifact::create(&k, ArtifactKind::Skill, "e2e", "d", b"payload", None, 1);
        {
            let mut d = origin.lock().unwrap();
            let e = d.ledger.append("artifact", &serde_json::to_string(&a).unwrap(), 1).unwrap();
            d.apply(&e);
        }
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, router(origin)).await.unwrap();
        });

        // A fresh replica pulls the origin's head + entries over HTTP and folds
        // them in, re-verifying every hash on its own box.
        let client = crate::NecropolisClient::new(format!("http://{addr}"));
        let head = client.ledger_head().await.unwrap();
        assert_eq!(head.seq, 1);

        let mut replica = Directory::in_memory();
        let incoming = client.ledger_since(replica.head_seq().unwrap()).await.unwrap();
        let applied = replica.apply_remote(&incoming).unwrap();

        assert_eq!(applied, 1);
        assert!(replica.artifacts.contains_key(&a.id));
        assert_eq!(replica.ledger.head_hash().unwrap(), head.hash);
    }

    #[tokio::test]
    async fn sync_once_federates_a_shared_directory() {
        let origin = shared();
        let k = Identity::load_or_create(tempfile::tempdir().unwrap().path()).unwrap();
        let a = Artifact::create(&k, ArtifactKind::Signal, "provider-throttling", "d", b"body", None, 1);
        {
            let mut d = origin.lock().unwrap();
            let e = d.ledger.append("artifact", &serde_json::to_string(&a).unwrap(), 1).unwrap();
            d.apply(&e);
        }
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, router(origin)).await.unwrap() });

        let replica = shared();
        let client = crate::NecropolisClient::new(format!("http://{addr}"));
        // First pass mirrors the one entry; second pass is a clean no-op.
        assert_eq!(sync_once(&replica, &client).await.unwrap(), 1);
        assert_eq!(sync_once(&replica, &client).await.unwrap(), 0);
        assert!(replica.lock().unwrap().artifacts.contains_key(&a.id));
    }
}
