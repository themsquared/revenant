//! Behavioral self-review wiring.
//!
//! Two background pieces:
//!   1. a **journal subscriber** that persists the friction signal (turn
//!      failures, cancellations, mid-turn steers/queues, denied approvals) which
//!      otherwise lives only on the event bus, and
//!   2. a **review timer** that periodically runs `AgentRuntime::self_review`,
//!      so the agent reflects on its own recent performance and rewrites the
//!      operating notes it re-reads every turn.
//!
//! Both are fail-soft: a journal write or a review that errors just logs and
//! the daemon carries on. The review is off if `[introspection].enabled=false`.

use revenant_agent::AgentRuntime;
use revenant_core::config::Config;
use revenant_core::event::Event;
use std::sync::Arc;
use std::time::Duration;

/// Spawn the journal subscriber (always) and the review timer (unless disabled).
pub fn spawn(cfg: Config, runtime: Arc<AgentRuntime>) {
    spawn_journal(runtime.clone());
    if cfg.introspection.enabled {
        spawn_review_timer(cfg, runtime);
    }
}

/// Persist friction events to the agent journal as they happen.
fn spawn_journal(runtime: Arc<AgentRuntime>) {
    let store = runtime.store.clone();
    let mut rx = runtime.events.subscribe();
    tokio::spawn(async move {
        loop {
            match rx.recv().await {
                Ok(event) => {
                    let (kind, sid, detail) = match event {
                        Event::TurnFailed { session_id, error } => {
                            ("turn_failed", Some(session_id), error)
                        }
                        Event::TurnCancelled { session_id } => {
                            ("turn_cancelled", Some(session_id), String::new())
                        }
                        Event::ContextFolded { session_id, note } => {
                            ("context_folded", Some(session_id), note)
                        }
                        Event::TaskQueued { session_id, task } => {
                            ("task_queued", Some(session_id), task)
                        }
                        Event::ApprovalResolved { verdict, id, .. } if verdict == "denied" => {
                            ("approval_denied", None, id)
                        }
                        _ => continue,
                    };
                    if let Err(err) = store.journal_add(kind, sid, &detail).await {
                        tracing::debug!("journal write failed: {err:#}");
                    }
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(_) => break, // bus closed → daemon shutting down
            }
        }
    });
}

/// Run a self-review on an interval.
fn spawn_review_timer(cfg: Config, runtime: Arc<AgentRuntime>) {
    let interval = Duration::from_secs(cfg.introspection.interval_secs.max(3600));
    let lookback = cfg.introspection.lookback_secs;
    let max_notes = cfg.introspection.max_notes;
    let tier = cfg.introspection.tier.clone();
    tokio::spawn(async move {
        // Let the daemon settle, then a short first look so a fresh install
        // produces notes within the first hour rather than a full day later.
        tokio::time::sleep(Duration::from_secs(120)).await;
        let mut tick = tokio::time::interval(interval);
        loop {
            tick.tick().await;
            match runtime.self_review(lookback, max_notes, &tier).await {
                Ok(review) => tracing::info!("self-review: {}", review.summary),
                Err(err) => tracing::warn!("self-review failed (will retry): {err:#}"),
            }
        }
    });
}
