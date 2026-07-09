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

#[derive(Clone)]
pub struct ApprovalBroker {
    store: Store,
    events: EventBus,
    pending: Arc<Mutex<HashMap<String, oneshot::Sender<Verdict>>>>,
    default_ttl: Duration,
}

impl ApprovalBroker {
    pub fn new(store: Store, events: EventBus, default_ttl: Duration) -> Self {
        ApprovalBroker { store, events, pending: Arc::default(), default_ttl }
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
        self.pending.lock().unwrap().insert(id.clone(), tx);
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
        let verdict = if approve { Verdict::Approved } else { Verdict::Denied };
        if !self.store.approval_resolve(id, verdict.as_str(), resolver).await? {
            return Ok(false);
        }
        if let Some(tx) = self.pending.lock().unwrap().remove(id) {
            let _ = tx.send(verdict);
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
        let broker = ApprovalBroker::new(store.clone(), events, Duration::from_millis(200));

        // Approved path
        let b2 = broker.clone();
        let req = tokio::spawn(async move { b2.request(1, "exec", "run ls", serde_json::json!({})).await });
        tokio::time::sleep(Duration::from_millis(50)).await;
        let pending = store.approvals_pending().await.unwrap();
        assert_eq!(pending.len(), 1);
        assert!(broker.resolve(&pending[0].id, true, "test").await.unwrap());
        assert_eq!(req.await.unwrap().unwrap(), Verdict::Approved);

        // Timeout path → default deny
        let verdict = broker
            .request(1, "exec", "run rm -rf", serde_json::json!({}))
            .await
            .unwrap();
        assert_eq!(verdict, Verdict::TimedOut);

        let _ = std::fs::remove_dir_all(&dir);
    }
}
