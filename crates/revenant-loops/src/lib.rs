//! revenant-loops: self-managed recurring jobs.
//!
//! The agent creates loops (via tools), the scheduler fires them off the hot
//! path. Each fire runs a normal turn in a dedicated loop session; results
//! are recorded in run history and optionally pushed to a channel. Safety
//! rails (min interval, per-day cap) are enforced here so a runaway loop
//! can't drain the budget.

pub mod jobs;

use anyhow::Result;
use revenant_agent::{SessionManager, SessionMsg};
use revenant_core::loops::Schedule;
use revenant_core::{Event, Tier};
use std::sync::Arc;
use std::time::Duration;

/// How many loops may execute at once. Due loops fire concurrently (a slow
/// one never blocks the rest), but this bounds the burst so a pile-up of due
/// loops can't thrash the box — the excess queues on the semaphore.
const MAX_CONCURRENT_FIRES: usize = 4;
/// How many times an idle loop's gap may double (4 ⇒ at most 16x its cadence).
const MAX_IDLE_DOUBLINGS: i64 = 4;
/// A backed-off loop still runs at least this often, so it can always find news.
const BACKOFF_CEILING_SECS: i64 = 86_400;

pub struct LoopScheduler {
    manager: SessionManager,
    default_tier: Tier,
    sem: Arc<tokio::sync::Semaphore>,
}

impl LoopScheduler {
    pub fn new(manager: SessionManager, default_tier: Tier) -> Self {
        LoopScheduler {
            manager,
            default_tier,
            sem: Arc::new(tokio::sync::Semaphore::new(MAX_CONCURRENT_FIRES)),
        }
    }

    /// Start the background scheduler: wakes every 15s, fires due loops.
    pub fn start(self: Arc<Self>) {
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_secs(15));
            loop {
                tick.tick().await;
                if let Err(err) = self.tick_once().await {
                    tracing::warn!("loop scheduler tick failed: {err:#}");
                }
            }
        });
    }

    async fn tick_once(self: &Arc<Self>) -> Result<()> {
        let now = unix_now();
        let store = &self.manager.runtime().store;
        let due = store.loops_due(now).await?;
        for lp in due {
            // Compute next_run first so a slow/failed run never re-fires in a
            // tight spin.
            let next = match Schedule::parse(&lp.schedule).and_then(|s| s.next_after(now)) {
                Ok(n) => n,
                Err(err) => {
                    tracing::warn!("loop {} has a bad schedule ({err:#}); pausing", lp.id);
                    let _ = store.loop_set_enabled(&lp.id, false).await;
                    continue;
                }
            };
            store.loop_mark_run(&lp.id, next).await?;

            // Per-day rail.
            let day_ago = now - 86_400;
            let today = store.loop_runs_since(&lp.id, day_ago).await.unwrap_or(0);
            if today >= lp.max_per_day {
                tracing::warn!("loop {} hit its {}/day cap; skipping", lp.name, lp.max_per_day);
                continue;
            }

            // Fire concurrently: a slow loop never blocks the others, but the
            // semaphore bounds the burst (excess queues). Non-blocking, capped.
            let this = Arc::clone(self);
            let sem = Arc::clone(&self.sem);
            let lp = lp.clone();
            tokio::spawn(async move {
                let _permit = match sem.acquire().await {
                    Ok(p) => p,
                    Err(_) => return,
                };
                this.fire(&lp).await;
            });
        }
        Ok(())
    }

    async fn fire(&self, lp: &revenant_store::LoopRow) {
        let runtime = self.manager.runtime();
        let tier = lp.tier.parse().unwrap_or(self.default_tier);
        let run_id = match runtime.store.loop_run_start(&lp.id).await {
            Ok(id) => id,
            Err(err) => {
                tracing::error!("loop {} run_start failed: {err:#}", lp.name);
                return;
            }
        };
        // Dedicated loop session (channel='loop', peer=loop id).
        let session_id = match runtime.store.ensure_session("loop", &lp.id, "loop").await {
            Ok(id) => id,
            Err(err) => {
                tracing::error!("loop {} session failed: {err:#}", lp.name);
                let _ = runtime.store.loop_run_finish(run_id, "error", 0, 0, &format!("{err:#}")).await;
                return;
            }
        };

        // Run the loop's prompt as a turn, capturing the outcome from the bus.
        let mut rx = runtime.events.subscribe();
        if let Err(err) = self
            .manager
            .submit(session_id, SessionMsg::UserInput { content: lp.prompt.clone(), tier })
            .await
        {
            let _ = runtime.store.loop_run_finish(run_id, "error", 0, 0, &format!("{err:#}")).await;
            return;
        }

        // Await this session's completion (bounded).
        let outcome = tokio::time::timeout(Duration::from_secs(300), async {
            loop {
                match rx.recv().await {
                    Ok(Event::TurnCompleted { session_id: s, text, input_tokens, output_tokens, .. })
                        if s == session_id =>
                    {
                        return Some(("ok", text, input_tokens as i64, output_tokens as i64));
                    }
                    Ok(Event::TurnFailed { session_id: s, error }) if s == session_id => {
                        return Some(("error", error, 0, 0));
                    }
                    Ok(_) => continue,
                    Err(_) => return None,
                }
            }
        })
        .await
        .ok()
        .flatten();

        match outcome {
            Some((status, text, tin, tout)) => {
                let _ = runtime
                    .store
                    .loop_run_finish(run_id, status, tin, tout, &clip(&text, 2000))
                    .await;
                // Cadence follows information, not the clock: a loop that keeps
                // saying the same thing gets asked less often, and snaps back to
                // full cadence the moment it says something new. Only successful
                // runs count — an error is not "no news", it is a failure, and
                // backing off would hide it.
                if status == "ok" {
                    self.tune_cadence(lp, &text).await;
                }
                // Push results to a channel if configured (channels listen
                // for LoopCompleted on the bus).
                if status == "ok" {
                    if let Some(channel) = &lp.channel_out {
                        runtime.events.emit(Event::LoopCompleted {
                            loop_id: lp.id.clone(),
                            name: lp.name.clone(),
                            channel_out: channel.clone(),
                            text,
                        });
                    }
                }
                tracing::info!("loop '{}' fired: {status}", lp.name);
            }
            None => {
                let _ = runtime
                    .store
                    .loop_run_finish(run_id, "error", 0, 0, "timed out or bus closed")
                    .await;
            }
        }
    }
}

impl LoopScheduler {
    /// Back a loop off when it has nothing new to report.
    ///
    /// The signal is deliberately LLM-free — a loop must not cost a model call
    /// just to decide whether it was worth running: fingerprint the output and
    /// count consecutive identical results.
    ///
    /// Two properties keep this from muting a useful loop:
    ///   - Backoff is bounded (MAX_IDLE_DOUBLINGS, plus an absolute
    ///     BACKOFF_CEILING_SECS), so a quiet loop still runs periodically and can
    ///     always discover news.
    ///   - Novelty resets to full cadence IMMEDIATELY, not gradually.
    ///
    /// Note what counts as "no news": byte-identical normalized output. A loop
    /// whose text always differs (an embedded timestamp, a changing count) will
    /// never back off. That is the safe direction to fail — it wastes some runs
    /// rather than silencing something that matters.
    async fn tune_cadence(&self, lp: &revenant_store::LoopRow, text: &str) {
        let store = &self.manager.runtime().store;
        let idle = match store.loop_note_output(&lp.id, &fingerprint(text)).await {
            Ok(n) => n,
            Err(err) => {
                tracing::warn!("loop {}: novelty check failed: {err:#}", lp.name);
                return;
            }
        };
        if idle == 0 {
            return; // novel: the base cadence already stands
        }
        let now = unix_now();
        let base_gap = match Schedule::parse(&lp.schedule).and_then(|s| s.next_after(now)) {
            Ok(next) => (next - now).max(1),
            Err(_) => return,
        };
        let doublings = idle.min(MAX_IDLE_DOUBLINGS) as u32;
        let extra = base_gap.saturating_mul((1i64 << doublings) - 1);
        if let Err(err) = store.loop_defer(&lp.id, extra, BACKOFF_CEILING_SECS).await {
            tracing::warn!("loop {}: defer failed: {err:#}", lp.name);
            return;
        }
        tracing::info!(
            "loop '{}' repeated itself {idle}x — next run pushed out {extra}s (capped)",
            lp.name
        );
    }
}

/// Stable fingerprint of a run's output, normalized so cosmetic differences
/// (case, wrapping, whitespace) do not read as news.
fn fingerprint(text: &str) -> String {
    use sha2::{Digest, Sha256};
    let normalized = text.split_whitespace().collect::<Vec<_>>().join(" ").to_lowercase();
    Sha256::digest(normalized.as_bytes()).iter().take(16).map(|b| format!("{b:02x}")).collect()
}

fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn clip(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut end = max;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &s[..end])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interval_floor_enforced() {
        assert!(Schedule::parse("every:30s").is_err());
        assert!(Schedule::parse("every:60s").is_ok());
        assert!(Schedule::parse("nonsense").is_err());
    }

    #[test]
    fn interval_next() {
        let s = Schedule::parse("every:600s").unwrap();
        assert_eq!(s.next_after(1000).unwrap(), 1600);
    }

    #[test]
    fn cron_parses_and_advances() {
        let s = Schedule::parse("cron:0 * * * *").unwrap(); // top of every hour
        let next = s.next_after(0).unwrap();
        assert!(next > 0 && next <= 3600);
    }
}

#[cfg(test)]
mod novelty_tests {
    use super::fingerprint;

    #[test]
    fn cosmetic_differences_are_not_news() {
        // Wrapping, indentation and case must not read as new information —
        // otherwise a loop never backs off and the signal is useless.
        assert_eq!(fingerprint("All systems healthy"), fingerprint("all   systems\nhealthy"));
        assert_eq!(fingerprint("  A B  "), fingerprint("a\tb"));
    }

    #[test]
    fn real_differences_are_news() {
        assert_ne!(fingerprint("2 alerts open"), fingerprint("3 alerts open"));
        assert_ne!(fingerprint("healthy"), fingerprint("degraded"));
        // Empty vs non-empty is a change.
        assert_ne!(fingerprint(""), fingerprint("something"));
    }

    #[test]
    fn fingerprint_is_stable_and_bounded() {
        let a = fingerprint("the horde rises");
        assert_eq!(a, fingerprint("the horde rises"), "must be deterministic across calls");
        assert_eq!(a.len(), 32, "16 bytes hex — short enough to store per loop");
    }
}
