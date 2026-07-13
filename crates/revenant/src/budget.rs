//! Background budget alerts.
//!
//! A soft, warn-only complement to the hard gateway spend cap: on an interval,
//! compare today's spend against a configured daily budget and, when it crosses
//! a threshold (default 50/80/100%), tell the owner ONCE per level per day. The
//! gateway cap is still the moat that actually blocks; this just removes the
//! "how did I burn $40 overnight?" surprise.
//!
//! Fail-soft like the auto-updater: a bad query or a missing budget just logs
//! and waits for the next tick. It never blocks a turn and never panics.

use revenant_core::config::Config;
use revenant_core::event::{Event, EventBus};
use revenant_core::home::Home;
use revenant_store::Store;
use std::time::Duration;

/// What the budget is measured in for a given config — dollars (when a USD
/// budget is set and pricing exists) or raw tokens.
#[derive(Clone, Copy)]
enum Unit {
    Usd,
    Tokens,
}

/// Spawn the budget monitor unless no daily budget is configured. Cheap no-op
/// otherwise, so it's always safe to call from `cmd_up`.
pub fn spawn(home: Home, cfg: Config, events: EventBus, store: Store) {
    let has_usd = cfg.spending.daily_budget_usd.is_some_and(|b| b > 0.0);
    let has_tokens = cfg.spending.daily_budget_tokens.is_some_and(|b| b > 0);
    if !has_usd && !has_tokens {
        return; // no daily budget → nothing to alert on
    }
    if cfg.spending.alert_thresholds.is_empty() {
        return;
    }
    let interval = Duration::from_secs(cfg.spending.alert_interval_secs.max(60));
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_secs(20)).await; // let startup settle
        let mut tick = tokio::time::interval(interval);
        loop {
            tick.tick().await;
            if let Err(err) = check_once(&home, &cfg, &events, &store).await {
                tracing::warn!("budget check failed (will retry): {err:#}");
            }
        }
    });
}

async fn check_once(
    home: &Home,
    cfg: &Config,
    events: &EventBus,
    store: &Store,
) -> anyhow::Result<()> {
    let day_start = day_start_ts();

    // Prefer a dollar budget when we can actually price the spend; otherwise
    // fall back to a raw-token budget.
    let priced = !cfg.pricing.is_empty();
    let (unit, budget) = match (cfg.spending.daily_budget_usd, cfg.spending.daily_budget_tokens) {
        (Some(usd), _) if priced && usd > 0.0 => (Unit::Usd, usd),
        (_, Some(tok)) if tok > 0 => (Unit::Tokens, tok as f64),
        // A USD budget with no pricing can't be evaluated — say so once and bail.
        (Some(_), None) => {
            tracing::debug!("budget alerts: daily_budget_usd set but [pricing] is empty — skipping");
            return Ok(());
        }
        _ => return Ok(()),
    };

    let spent = match unit {
        Unit::Usd => {
            let rows = store.spend_since(day_start).await?;
            rows.iter()
                .filter_map(|r| {
                    cfg.pricing.get(&r.model).map(|p| {
                        r.tokens_in as f64 / 1e6 * p.input_per_mtok
                            + r.tokens_out as f64 / 1e6 * p.output_per_mtok
                    })
                })
                .sum()
        }
        Unit::Tokens => {
            let (tin, tout) = store.spend_today().await?;
            (tin + tout) as f64
        }
    };

    let fraction = if budget > 0.0 { spent / budget } else { 0.0 };

    // Sorted-ascending thresholds; the highest one we've crossed is our level.
    let mut thresholds = cfg.spending.alert_thresholds.clone();
    thresholds.retain(|t| t.is_finite() && *t > 0.0);
    thresholds.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let Some(level) = crossed_level(fraction, &thresholds) else {
        return Ok(());
    };

    // Dedup on an on-disk marker so it survives restarts (mirrors the updater).
    // Format: "YYYYMMDD:<highest level index alerted today>".
    let marker = home.root().join("budget-alert-state");
    let today = day_key(day_start);
    let last = read_state(&marker, &today);
    if (level as i64) <= last {
        return Ok(()); // already alerted this level (or higher) today
    }
    write_state(&marker, &today, level);

    let threshold = thresholds[level];
    let pct = (threshold * 100.0).round() as u8;
    let (spent_s, budget_s) = match unit {
        Unit::Usd => (format!("${spent:.2}"), format!("${budget:.2}")),
        Unit::Tokens => (fmt_tokens(spent), fmt_tokens(budget)),
    };
    tracing::warn!("budget alert: today's spend {spent_s} / {budget_s} ({pct}% of daily budget)");
    events.emit(Event::BudgetAlert { pct, spent: spent_s, budget: budget_s });
    Ok(())
}

/// Highest index in `thresholds` (ascending) whose value is <= `fraction`, or
/// None if none crossed. Emitting only the highest crossed level means a jump
/// straight to 100% produces one alert, not three.
fn crossed_level(fraction: f64, thresholds: &[f64]) -> Option<usize> {
    thresholds
        .iter()
        .enumerate()
        .filter(|(_, t)| fraction >= **t)
        .map(|(i, _)| i)
        .next_back()
}

/// Start-of-day (UTC) unix timestamp.
fn day_start_ts() -> i64 {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    now - (now % 86_400)
}

fn day_key(day_start: i64) -> String {
    (day_start / 86_400).to_string() // days-since-epoch: unique per UTC day
}

fn read_state(marker: &std::path::Path, today: &str) -> i64 {
    let Ok(s) = std::fs::read_to_string(marker) else {
        return -1;
    };
    let mut parts = s.trim().splitn(2, ':');
    match (parts.next(), parts.next()) {
        (Some(day), Some(level)) if day == today => level.trim().parse().unwrap_or(-1),
        _ => -1, // different day (or garbage) → nothing alerted yet today
    }
}

fn write_state(marker: &std::path::Path, today: &str, level: usize) {
    let _ = std::fs::write(marker, format!("{today}:{level}"));
}

fn fmt_tokens(n: f64) -> String {
    let n = n as u64;
    if n >= 1_000_000 {
        format!("{:.1}M tok", n as f64 / 1e6)
    } else if n >= 1_000 {
        format!("{:.1}K tok", n as f64 / 1e3)
    } else {
        format!("{n} tok")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crossed_level_picks_highest() {
        let t = vec![0.5, 0.8, 1.0];
        assert_eq!(crossed_level(0.4, &t), None);
        assert_eq!(crossed_level(0.5, &t), Some(0));
        assert_eq!(crossed_level(0.79, &t), Some(0));
        assert_eq!(crossed_level(0.8, &t), Some(1));
        assert_eq!(crossed_level(1.2, &t), Some(2)); // jump straight past → one alert at top
    }

    #[test]
    fn state_roundtrip_and_day_reset() {
        let dir = std::env::temp_dir().join(format!("rev-budget-test-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let marker = dir.join("budget-alert-state");
        assert_eq!(read_state(&marker, "100"), -1); // no file yet
        write_state(&marker, "100", 1);
        assert_eq!(read_state(&marker, "100"), 1); // same day → remembered
        assert_eq!(read_state(&marker, "101"), -1); // new day → reset
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn fmt_tokens_reads_naturally() {
        assert_eq!(fmt_tokens(500.0), "500 tok");
        assert_eq!(fmt_tokens(1500.0), "1.5K tok");
        assert_eq!(fmt_tokens(2_000_000.0), "2.0M tok");
    }

    // End-to-end of the glue: seed today's spend, run one check against a tiny
    // token budget, and assert a single BudgetAlert lands — then that a second
    // check the same day is deduped to silence.
    #[tokio::test]
    async fn check_once_emits_once_when_budget_crossed() {
        let dir = std::env::temp_dir().join(format!("rev-budget-e2e-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let home = Home::at(&dir);
        let store = Store::open(&dir.join("t.db")).unwrap();
        // 120 tokens of spend today; budget 100 → 120% → crosses the 100% level.
        store
            .record_spend(
                1,
                "fast",
                Some("some-model"),
                revenant_core::Usage { input_tokens: 60, output_tokens: 60, ..Default::default() },
            )
            .await
            .unwrap();

        let mut cfg = Config::default_config();
        cfg.spending.daily_budget_tokens = Some(100);
        cfg.spending.alert_thresholds = vec![0.5, 0.8, 1.0];

        let events = EventBus::new(16);
        let mut rx = events.subscribe();

        check_once(&home, &cfg, &events, &store).await.unwrap();
        let evt = rx.try_recv().expect("expected a BudgetAlert");
        match evt {
            Event::BudgetAlert { pct, .. } => assert_eq!(pct, 100),
            other => panic!("unexpected event: {other:?}"),
        }

        // Second check, same day, same level → no new alert.
        check_once(&home, &cfg, &events, &store).await.unwrap();
        assert!(rx.try_recv().is_err(), "second check should be deduped");

        let _ = std::fs::remove_dir_all(&dir);
    }
}
