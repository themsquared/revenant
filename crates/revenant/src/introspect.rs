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
//! The timer is deliberately quiet. Two guards stop it from nagging:
//!   - it persists the last-run timestamp, so it runs at most once per
//!     `interval_secs` even though the daemon restarts often (canary rollouts) —
//!     a fresh boot no longer triggers a fresh review, and
//!   - it only surfaces a review to chat when the content actually CHANGED since
//!     the last one (fingerprint dedup); an unchanged review updates the notes
//!     silently and says nothing.
//! Both fail-soft: a journal write or a review that errors just logs.

use revenant_agent::AgentRuntime;
use revenant_core::config::Config;
use revenant_core::event::Event;
use revenant_core::home::Home;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Spawn the journal subscriber (always) and the review timer (unless disabled).
pub fn spawn(home: Home, cfg: Config, runtime: Arc<AgentRuntime>) {
    spawn_journal(runtime.clone());
    if cfg.introspection.enabled {
        spawn_review_timer(home, cfg, runtime);
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

/// Run a self-review on an interval — gated so restarts don't re-trigger it and
/// unchanged results stay silent.
fn spawn_review_timer(home: Home, cfg: Config, runtime: Arc<AgentRuntime>) {
    let interval_secs = cfg.introspection.interval_secs.max(3600);
    let lookback = cfg.introspection.lookback_secs;
    let max_notes = cfg.introspection.max_notes;
    let tier = cfg.introspection.tier.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_secs(30)).await; // let the daemon settle
        let mut tick = tokio::time::interval(Duration::from_secs(interval_secs));
        loop {
            tick.tick().await;
            let now = now_secs();
            let (last_run, last_fp) = read_state(&home);
            // Survives restarts: only actually review once per interval, no
            // matter how often the daemon reboots.
            if now - last_run < interval_secs as i64 {
                continue;
            }
            match runtime.self_review(lookback, max_notes, &tier).await {
                Ok(review) => {
                    let fp = fp_hash(&review.fingerprint());
                    write_state(&home, now, fp);
                    if fp != last_fp {
                        tracing::info!("self-review: {} (surfacing — changed)", review.summary);
                        runtime.events.emit(Event::SelfReviewCompleted {
                            summary: review.summary,
                            lessons: review.lessons.len() as u32,
                            suggestions: review.suggestions,
                        });
                    } else {
                        tracing::info!("self-review: no change since last — staying quiet");
                    }
                }
                Err(err) => tracing::warn!("self-review failed (will retry): {err:#}"),
            }
        }
    });
}

fn now_secs() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

/// Persistent dedup/throttle state: `<last_run_ts> <last_fingerprint_hash>`.
fn state_path(home: &Home) -> std::path::PathBuf {
    home.root().join("introspection-state")
}

fn read_state(home: &Home) -> (i64, u64) {
    let Ok(s) = std::fs::read_to_string(state_path(home)) else {
        return (0, 0);
    };
    let mut it = s.split_whitespace();
    let ts = it.next().and_then(|v| v.parse().ok()).unwrap_or(0);
    let fp = it.next().and_then(|v| v.parse().ok()).unwrap_or(0);
    (ts, fp)
}

fn write_state(home: &Home, ts: i64, fp: u64) {
    let _ = std::fs::write(state_path(home), format!("{ts} {fp}"));
}

fn fp_hash(fp: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    fp.hash(&mut h);
    h.finish()
}
