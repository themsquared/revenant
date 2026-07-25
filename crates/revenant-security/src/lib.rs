//! revenant-security: the approval broker — the single choke point every
//! capability escalation crosses. Requests are persisted, broadcast to all
//! surfaces, resolved first-writer-wins, and default-DENY on TTL expiry.

use anyhow::Result;
use revenant_core::{Event, EventBus};
use revenant_store::Store;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::oneshot;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    Approved,
    Denied,
    TimedOut,
}

impl Verdict {
    pub fn as_str(&self) -> &'static str {
        match self {
            Verdict::Approved => "approved",
            Verdict::Denied => "denied",
            Verdict::TimedOut => "timed_out",
        }
    }
}

/// The result of asking the owner FOR A VALUE rather than for permission.
///
/// Deliberately a separate type from [`Verdict`], not a fourth variant of it.
/// `Verdict` gates capability escalation, and every caller treats
/// "not `Approved`" as refusal; folding a text answer into it would create a
/// path where a typed reply could be read as consent. Keeping them apart means
/// an elicitation can never authorize anything, and an approval can never
/// smuggle data back.
///
/// Mirrors the three actions MCP elicitation defines (accept / decline /
/// cancel) so the wire mapping is one-to-one and nothing is invented here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ElicitOutcome {
    /// The owner supplied the requested content.
    Accepted(String),
    /// The owner explicitly refused to provide it.
    Declined,
    /// Dismissed, or no answer before the TTL expired. The default.
    Cancelled,
}

impl ElicitOutcome {
    pub fn as_str(&self) -> &'static str {
        match self {
            ElicitOutcome::Accepted(_) => "accepted",
            ElicitOutcome::Declined => "declined",
            ElicitOutcome::Cancelled => "cancelled",
        }
    }
}

/// How long a "approve for this task" grant lasts (per session + tool kind).
/// A task is a burst of related work; an hour is long enough to not re-nag,
/// short enough that a stale grant expires on its own.
const GRANT_TTL_SECS: i64 = 3600;

struct Pending {
    tx: oneshot::Sender<Verdict>,
    session_id: i64,
    kind: String,
}

#[derive(Clone)]
pub struct ApprovalBroker {
    store: Store,
    events: EventBus,
    pending: Arc<Mutex<HashMap<String, Pending>>>,
    /// In-flight elicitations, kept in their OWN map so a text answer can never
    /// be delivered to a waiting capability approval (or the reverse).
    pending_elicit: Arc<Mutex<HashMap<String, oneshot::Sender<ElicitOutcome>>>>,
    /// Standing "approve all for this task" grants: (session_id, kind) -> expiry.
    /// A granted kind auto-approves without prompting until it expires.
    grants: Arc<Mutex<HashMap<(i64, String), i64>>>,
    default_ttl: Duration,
}

impl ApprovalBroker {
    pub fn new(store: Store, events: EventBus, default_ttl: Duration) -> Self {
        ApprovalBroker {
            store,
            events,
            pending: Arc::default(),
            pending_elicit: Arc::default(),
            grants: Arc::default(),
            default_ttl,
        }
    }

    /// True if the owner already granted this (session, kind) for the task.
    fn has_grant(&self, session_id: i64, kind: &str) -> bool {
        let mut g = self.grants.lock().unwrap();
        let key = (session_id, kind.to_string());
        match g.get(&key) {
            Some(&exp) if exp > now() => true,
            Some(_) => {
                g.remove(&key);
                false
            }
            None => false,
        }
    }

    /// Drop every standing grant for a session (e.g. on an explicit "stop").
    pub fn revoke_grants(&self, session_id: i64) {
        self.grants.lock().unwrap().retain(|(s, _), _| *s != session_id);
    }

    /// Ask the owner. Blocks the calling turn until resolved or TTL expiry
    /// (expiry = denied). The request is visible on every surface at once.
    pub async fn request(
        &self,
        session_id: i64,
        kind: &str,
        summary: &str,
        payload: serde_json::Value,
    ) -> Result<Verdict> {
        // Already granted "for this task"? Approve silently — no prompt, no
        // event. This is what stops exec from nagging on every command.
        if self.has_grant(session_id, kind) {
            return Ok(Verdict::Approved);
        }
        let id = uuid::Uuid::new_v4().simple().to_string();
        let ttl = self.default_ttl;
        let payload_str = serde_json::to_string(&serde_json::json!({
            "summary": summary,
            "session_id": session_id,
            "detail": payload,
        }))?;
        self.store
            .approval_insert(&id, kind, &payload_str, (ttl.as_secs().max(1)) as i64)
            .await?;

        let (tx, rx) = oneshot::channel();
        self.pending
            .lock()
            .unwrap()
            .insert(id.clone(), Pending { tx, session_id, kind: kind.to_string() });
        self.events.emit(Event::ApprovalCreated {
            id: id.clone(),
            session_id,
            kind: kind.to_string(),
            summary: summary.to_string(),
            expires_at: now() + ttl.as_secs() as i64,
        });

        let verdict = match tokio::time::timeout(ttl, rx).await {
            Ok(Ok(verdict)) => verdict,
            _ => {
                // TTL expiry or broker drop: deny and record it.
                self.pending.lock().unwrap().remove(&id);
                let _ = self.store.approval_resolve(&id, "timed_out", "system").await;
                self.events.emit(Event::ApprovalResolved {
                    id: id.clone(),
                    verdict: "timed_out".into(),
                    resolver: "system".into(),
                });
                Verdict::TimedOut
            }
        };
        Ok(verdict)
    }

    /// Ask the owner FOR A VALUE — an MCP server requested input mid-tool-call
    /// (spec 2025-06-18 elicitation). Blocks the calling turn until the owner
    /// answers or the TTL expires; expiry is [`ElicitOutcome::Cancelled`].
    ///
    /// `source` names WHO is asking (the MCP server) and is carried to every
    /// surface, because "some tool wants your API key" is unanswerable — the
    /// owner has to know which server is asking to judge it. `schema` is the
    /// server's requested shape, passed through for the surface to render.
    ///
    /// Three properties this deliberately does NOT share with [`request`]:
    ///
    /// 1. It never consults standing grants. A grant means "you may keep doing
    ///    this kind of ACTION for this task" — letting it auto-satisfy a request
    ///    for DATA would hand a server a value the owner never saw, which is the
    ///    exfiltration shape this whole path has to prevent.
    /// 2. It never creates a grant. Every value is asked for individually.
    /// 3. Only a human resolve can produce `Accepted`. There is no code path
    ///    where a timeout, a default, or anything read from memory/context
    ///    becomes an answer — silence is `Cancelled`, never a guess.
    pub async fn elicit(
        &self,
        session_id: i64,
        source: &str,
        prompt: &str,
        schema: serde_json::Value,
    ) -> Result<ElicitOutcome> {
        let id = uuid::Uuid::new_v4().simple().to_string();
        let ttl = self.default_ttl;
        let payload_str = serde_json::to_string(&serde_json::json!({
            "summary": prompt,
            "session_id": session_id,
            "source": source,
            "schema": schema,
        }))?;
        // Stored under a reserved kind so it can never collide with a tool kind
        // and be resolved by the boolean approve/deny path by mistake.
        self.store
            .approval_insert(&id, "elicitation", &payload_str, (ttl.as_secs().max(1)) as i64)
            .await?;

        let (tx, rx) = oneshot::channel();
        self.pending_elicit.lock().unwrap().insert(id.clone(), tx);
        self.events.emit(Event::ElicitationRequested {
            id: id.clone(),
            session_id,
            source: source.to_string(),
            prompt: prompt.to_string(),
            expires_at: now() + ttl.as_secs() as i64,
        });

        let outcome = match tokio::time::timeout(ttl, rx).await {
            Ok(Ok(outcome)) => outcome,
            _ => {
                self.pending_elicit.lock().unwrap().remove(&id);
                let _ = self.store.approval_resolve(&id, "cancelled", "system").await;
                self.events.emit(Event::ApprovalResolved {
                    id: id.clone(),
                    verdict: "cancelled".into(),
                    resolver: "system".into(),
                });
                ElicitOutcome::Cancelled
            }
        };
        Ok(outcome)
    }

    /// Answer a pending elicitation from any surface. First writer wins;
    /// returns false if it was already resolved or is not a known elicitation.
    ///
    /// `answer: None` means the owner declined. An empty string is treated as a
    /// decline too — "I pressed send with nothing" must not be delivered as a
    /// value the server then acts on.
    pub async fn resolve_elicitation(
        &self,
        id: &str,
        answer: Option<&str>,
        resolver: &str,
    ) -> Result<bool> {
        let outcome = match answer.map(str::trim) {
            Some(text) if !text.is_empty() => ElicitOutcome::Accepted(text.to_string()),
            _ => ElicitOutcome::Declined,
        };
        let stored = match &outcome {
            ElicitOutcome::Accepted(text) => Some(text.as_str()),
            _ => None,
        };
        if !self
            .store
            .approval_resolve_with(id, outcome.as_str(), resolver, stored)
            .await?
        {
            return Ok(false);
        }
        // Only unblock a waiter that is actually an elicitation. A missing entry
        // still counts as resolved (the row was CAS'd) — e.g. the daemon
        // restarted and nothing is waiting any more.
        if let Some(tx) = self.pending_elicit.lock().unwrap().remove(id) {
            let _ = tx.send(outcome.clone());
        }
        self.events.emit(Event::ApprovalResolved {
            id: id.to_string(),
            verdict: outcome.as_str().into(),
            resolver: resolver.to_string(),
        });
        Ok(true)
    }

    /// Resolve from any surface. First writer wins; returns false if the
    /// approval was already resolved (or unknown).
    pub async fn resolve(&self, id: &str, approve: bool, resolver: &str) -> Result<bool> {
        self.resolve_scoped(id, approve, false, resolver).await
    }

    /// Resolve, optionally granting "all of this kind for this task": when
    /// `grant` is true and the verdict is approve, every later request of the
    /// same (session, kind) auto-approves for GRANT_TTL_SECS — no more prompts.
    pub async fn resolve_scoped(
        &self,
        id: &str,
        approve: bool,
        grant: bool,
        resolver: &str,
    ) -> Result<bool> {
        let verdict = if approve { Verdict::Approved } else { Verdict::Denied };
        if !self.store.approval_resolve(id, verdict.as_str(), resolver).await? {
            return Ok(false);
        }
        if let Some(p) = self.pending.lock().unwrap().remove(id) {
            if approve && grant {
                self.grants
                    .lock()
                    .unwrap()
                    .insert((p.session_id, p.kind.clone()), now() + GRANT_TTL_SECS);
            }
            let _ = p.tx.send(verdict);
        }
        self.events.emit(Event::ApprovalResolved {
            id: id.to_string(),
            verdict: verdict.as_str().into(),
            resolver: resolver.to_string(),
        });
        Ok(true)
    }
}

fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod elicit_tests {
    use super::*;

    /// Wait until a request has actually been persisted, instead of sleeping a
    /// fixed interval and hoping. The original version slept 50ms, which passed
    /// in isolation and failed under a loaded full-workspace run — the spawned
    /// task had not inserted yet, so `approvals_pending()[0]` panicked on an
    /// empty vec. Polling makes the test wait for the CONDITION it depends on.
    async fn await_pending(store: &Store) -> String {
        for _ in 0..200 {
            if let Some(row) = store.approvals_pending().await.unwrap_or_default().into_iter().next()
            {
                return row.id;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("no approval was persisted within 2s");
    }

    fn new_broker(name: &str, ttl: Duration) -> (ApprovalBroker, Store) {
        let dir = std::env::temp_dir().join(format!("rev-elicit-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let store = Store::open(&dir.join("t.db")).unwrap();
        let broker = ApprovalBroker::new(store.clone(), EventBus::new(64), ttl);
        (broker, store)
    }

    #[tokio::test]
    async fn answer_is_delivered_and_persisted() {
        let (broker, store) = new_broker("answer", Duration::from_secs(10));
        let b = broker.clone();
        let task = tokio::spawn(async move {
            b.elicit(1, "acme-mcp", "Which region?", serde_json::json!({"type":"string"})).await
        });
        let id = await_pending(&store).await;
        // Reserved kind: can never collide with a tool kind.
        let pending = store.approvals_pending().await.unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].kind, "elicitation");

        assert!(broker.resolve_elicitation(&id, Some("us-east-1"), "owner").await.unwrap());
        assert_eq!(
            task.await.unwrap().unwrap(),
            ElicitOutcome::Accepted("us-east-1".into())
        );
        // Answering twice is refused — first writer wins, like approvals.
        assert!(!broker.resolve_elicitation(&id, Some("eu-west-1"), "owner").await.unwrap());
    }

    #[tokio::test]
    async fn silence_and_emptiness_are_never_an_answer() {
        // No reply before the TTL → Cancelled, never a guessed value.
        let (broker, _s) = new_broker("timeout", Duration::from_millis(150));
        let out = broker
            .elicit(1, "acme-mcp", "API key?", serde_json::json!({"type":"string"}))
            .await
            .unwrap();
        assert_eq!(out, ElicitOutcome::Cancelled);

        // An empty / whitespace-only reply is a decline, not an empty value
        // handed to the server.
        let (broker, store) = new_broker("empty", Duration::from_secs(10));
        let b = broker.clone();
        let task = tokio::spawn(async move {
            b.elicit(1, "acme-mcp", "Token?", serde_json::json!({"type":"string"})).await
        });
        let id = await_pending(&store).await;
        assert!(broker.resolve_elicitation(&id, Some("   "), "owner").await.unwrap());
        assert_eq!(task.await.unwrap().unwrap(), ElicitOutcome::Declined);
    }

    /// The property that keeps elicitation from becoming an exfiltration path: a
    /// standing "approve all exec for this task" grant must NOT satisfy a request
    /// for DATA. If it did, a server could ask for a secret and be answered
    /// without the owner ever seeing the prompt.
    #[tokio::test]
    async fn a_standing_grant_cannot_auto_answer_an_elicitation() {
        let (broker, store) = new_broker("grant", Duration::from_millis(200));

        // Establish a standing grant for (session 1, "elicitation") — the most
        // adversarial case: same session, and the grant key is literally the
        // elicitation kind.
        let b = broker.clone();
        let approval =
            tokio::spawn(async move { b.request(1, "elicitation", "x", serde_json::json!({})).await });
        let id = await_pending(&store).await;
        broker.resolve_scoped(&id, true, true, "owner").await.unwrap();
        assert_eq!(approval.await.unwrap().unwrap(), Verdict::Approved);
        // The grant is live: a second capability request auto-approves.
        assert_eq!(
            broker.request(1, "elicitation", "x", serde_json::json!({})).await.unwrap(),
            Verdict::Approved
        );

        // ...but an elicitation still goes to the owner, and with nobody
        // answering it times out rather than inheriting the grant.
        let out = broker
            .elicit(1, "acme-mcp", "Paste your API key", serde_json::json!({"type":"string"}))
            .await
            .unwrap();
        assert_eq!(out, ElicitOutcome::Cancelled, "a grant must never answer for the owner");
    }

    /// The two paths must not be able to resolve each other: a boolean approve
    /// cannot satisfy a waiting elicitation, and an answer cannot approve a
    /// waiting capability request.
    /// TTLs here are seconds, not milliseconds, and that is load-bearing. The
    /// first version used 400ms — SHORTER than `await_pending`'s poll window — so
    /// on a loaded box the request timed out and left the pending set before the
    /// test could read its id, and `await_pending` panicked. My first fix
    /// addressed the wrong race (insert visibility) and CI caught the real one
    /// (expiry beating the poll). Both halves end by TTL on purpose, so this test
    /// takes a few seconds; determinism is worth more than the seconds.
    #[tokio::test]
    async fn the_two_paths_cannot_cross_resolve() {
        let (broker, store) = new_broker("cross", Duration::from_secs(3));

        // A waiting elicitation, resolved via the boolean approval path.
        let b = broker.clone();
        let elicit = tokio::spawn(async move {
            b.elicit(2, "acme-mcp", "Region?", serde_json::json!({"type":"string"})).await
        });
        let id = await_pending(&store).await;
        // The row CAS succeeds (it is the same table), but no value is produced:
        // the waiter is in the elicitation map and never receives an Accepted.
        broker.resolve_scoped(&id, true, false, "owner").await.unwrap();
        assert_eq!(
            elicit.await.unwrap().unwrap(),
            ElicitOutcome::Cancelled,
            "approve must not manufacture an elicitation answer"
        );

        // A waiting capability approval, resolved via the answer path.
        let (broker, store) = new_broker("cross2", Duration::from_secs(3));
        let b = broker.clone();
        let req = tokio::spawn(async move { b.request(3, "exec", "rm -rf /", serde_json::json!({})).await });
        let id = await_pending(&store).await;
        broker.resolve_elicitation(&id, Some("yes do it"), "owner").await.unwrap();
        assert_eq!(
            req.await.unwrap().unwrap(),
            Verdict::TimedOut,
            "a typed answer must never read as consent"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn approve_and_deny_and_timeout() {
        let dir = std::env::temp_dir().join(format!("rev-sec-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let store = Store::open(&dir.join("t.db")).unwrap();
        let events = EventBus::new(64);
        // Generous TTL so the resolve never races the timeout under load — the
        // old 200ms was flaky when the box was busy (e.g. a release build).
        let broker = ApprovalBroker::new(store.clone(), events.clone(), Duration::from_secs(10));

        // Approved path
        let b2 = broker.clone();
        let req = tokio::spawn(async move { b2.request(1, "exec", "run ls", serde_json::json!({})).await });
        tokio::time::sleep(Duration::from_millis(50)).await;
        let pending = store.approvals_pending().await.unwrap();
        assert_eq!(pending.len(), 1);
        assert!(broker.resolve(&pending[0].id, true, "test").await.unwrap());
        assert_eq!(req.await.unwrap().unwrap(), Verdict::Approved);

        // Denied path → explicit deny (deterministic, no wall-clock race)
        let b3 = broker.clone();
        let req = tokio::spawn(async move { b3.request(1, "exec", "run rm -rf", serde_json::json!({})).await });
        tokio::time::sleep(Duration::from_millis(50)).await;
        let pending = store.approvals_pending().await.unwrap();
        assert_eq!(pending.len(), 1);
        assert!(broker.resolve(&pending[0].id, false, "test").await.unwrap());
        assert_eq!(req.await.unwrap().unwrap(), Verdict::Denied);

        // Timeout path → default deny. A short-TTL broker with NO resolver: this
        // is robust at any load (nothing can make it wrongly approve).
        let broker_to = ApprovalBroker::new(store.clone(), events, Duration::from_millis(150));
        let verdict = broker_to
            .request(1, "exec", "run sleep", serde_json::json!({}))
            .await
            .unwrap();
        assert_eq!(verdict, Verdict::TimedOut);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn task_grant_auto_approves_same_session_kind() {
        let dir = std::env::temp_dir().join(format!("rev-sec-grant-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let store = Store::open(&dir.join("t.db")).unwrap();
        // Generous TTL: resolve paths never race the timeout, and we prove
        // scoping by "did it prompt?" (pending count) rather than waiting out a
        // timeout — fast and deterministic under any load.
        let broker = ApprovalBroker::new(store.clone(), EventBus::new(64), Duration::from_secs(10));

        // First exec: prompt, then approve WITH a task grant.
        let b2 = broker.clone();
        let req = tokio::spawn(async move { b2.request(7, "exec", "ls", serde_json::json!({})).await });
        tokio::time::sleep(Duration::from_millis(50)).await;
        let pending = store.approvals_pending().await.unwrap();
        assert!(broker.resolve_scoped(&pending[0].id, true, true, "test").await.unwrap());
        assert_eq!(req.await.unwrap().unwrap(), Verdict::Approved);

        // Second exec, SAME session+kind: auto-approved instantly, no prompt.
        let v = broker.request(7, "exec", "cat x", serde_json::json!({})).await.unwrap();
        assert_eq!(v, Verdict::Approved);
        assert_eq!(store.approvals_pending().await.unwrap().len(), 0, "should not have prompted");

        // A different session is NOT covered by the grant → it PROMPTS (a
        // pending approval appears). Session 7's grant means it can't be 7's.
        let b8 = broker.clone();
        let r8 = tokio::spawn(async move { b8.request(8, "exec", "ls", serde_json::json!({})).await });
        tokio::time::sleep(Duration::from_millis(50)).await;
        let pending = store.approvals_pending().await.unwrap();
        assert_eq!(pending.len(), 1, "uncovered session must prompt");
        broker.resolve(&pending[0].id, false, "test").await.unwrap();
        assert_eq!(r8.await.unwrap().unwrap(), Verdict::Denied);

        // Revoke clears the grant: session 7 prompts again.
        broker.revoke_grants(7);
        let b7 = broker.clone();
        let r7 = tokio::spawn(async move { b7.request(7, "exec", "ls", serde_json::json!({})).await });
        tokio::time::sleep(Duration::from_millis(50)).await;
        let pending = store.approvals_pending().await.unwrap();
        assert_eq!(pending.len(), 1, "revoked grant must prompt again");
        broker.resolve(&pending[0].id, false, "test").await.unwrap();
        assert_eq!(r7.await.unwrap().unwrap(), Verdict::Denied);

        let _ = std::fs::remove_dir_all(&dir);
    }
}
