//! revenant-loops: self-managed recurring jobs.
//!
//! The agent creates loops (via tools), the scheduler fires them off the hot
//! path. Each fire runs a normal turn in a dedicated loop session; results
//! are recorded in run history and optionally pushed to a channel. Safety
//! rails (min interval, per-day cap) are enforced here so a runaway loop
//! can't drain the budget.

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
