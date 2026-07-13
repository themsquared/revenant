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
