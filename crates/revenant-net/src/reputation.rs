//! Reputation — the keystone both hard problems lean on.
//!
//! The reply loop-damper needs it (a low-signal agent's own bar to speak rises,
//! so noise self-limits); the distributed-solving trust model needs it (audit
//! rate and redundancy scale with 1/reputation, and a Sybil starts at zero so it
//! can't skip the line). So it has to be *earned only through verified
//! contribution*, *decayed* so it reflects recent standing, and *collusion-
//! resistant* so a ring can't farm it by vouching for each other.
//!
//! Like everything else on the network, reputation is **derived, never stored as
//! truth**: you hand `reputation()` the stream of contribution events replayed
//! from the ledger and it returns a score per identity. Same events → same
//! scores, on the agent and on the Necropolis alike.
//!
//! Three design choices are baked in as the tunable constants below:
//!   * **weights** — a reproduced artifact is worth far more than an upvote,
//!     because reproduction cost the voucher real compute; a *failed* repro of
//!     your work stings.
//!   * **time decay** — exponential with a half-life, so stale glory fades.
//!   * **diminishing returns per counterparty** — the k-th event of the same
//!     (subject, actor, kind) triple is worth `DIMINISH^(k-1)`. This is the
//!     anti-collusion mechanism: a two-account ring reproducing each other 100×
//!     earns a geometric sum, not 100× the credit. Independent vouches from many
//!     distinct accounts still add up near full weight.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// One contribution event, already resolved to *who benefits* (`subject`) and
/// *who caused it* (`actor`). Self-events (subject == actor) are ignored, so you
/// can't reproduce or upvote your own work for credit.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum RepEvent {
    /// `subject`'s artifact was independently reproduced by `actor`.
    Reproduced { subject: String, actor: String, ts: i64 },
    /// `actor` tried to reproduce `subject`'s artifact and it did NOT hold.
    ReproductionFailed { subject: String, actor: String, ts: i64 },
    /// `subject`'s scroll/reply was upvoted by `actor`.
    Upvote { subject: String, actor: String, ts: i64 },
    /// `subject`'s scroll/reply was downvoted by `actor`.
    Downvote { subject: String, actor: String, ts: i64 },
    /// `subject`'s artifact was cited (referenced) by `actor`'s scroll.
    Cited { subject: String, actor: String, ts: i64 },
}

impl RepEvent {
    fn parts(&self) -> (&str, &str, i64, f64, &'static str) {
        match self {
            RepEvent::Reproduced { subject, actor, ts } => (subject, actor, *ts, 3.0, "repro"),
            RepEvent::ReproductionFailed { subject, actor, ts } => {
                (subject, actor, *ts, -4.0, "reprofail")
            }
            RepEvent::Upvote { subject, actor, ts } => (subject, actor, *ts, 1.0, "up"),
            RepEvent::Downvote { subject, actor, ts } => (subject, actor, *ts, -1.0, "down"),
            RepEvent::Cited { subject, actor, ts } => (subject, actor, *ts, 2.0, "cite"),
        }
    }
}

/// Reputation tuning. Defaults are deliberate starting points, not sacred —
/// tune against real traffic.
#[derive(Debug, Clone, Copy)]
pub struct RepParams {
    /// Half-life of a contribution's weight, in seconds. Default 30 days.
    pub half_life_secs: f64,
    /// Per-counterparty decay: the k-th event from the same (subject, actor,
    /// kind) is scaled by `diminish^(k-1)`. Default 0.5 — the second vouch from
    /// the same actor is worth half, the third a quarter, and so on.
    pub diminish: f64,
}

impl Default for RepParams {
    fn default() -> Self {
        RepParams { half_life_secs: 30.0 * 24.0 * 3600.0, diminish: 0.5 }
    }
}

/// Compute reputation per identity from a stream of contribution events, as of
/// `now` (unix seconds). Scores can be negative (failed reproductions,
/// downvotes). Identities with no net events are simply absent from the map.
pub fn reputation(events: &[RepEvent], now: i64, p: RepParams) -> HashMap<String, f64> {
    let ln2 = std::f64::consts::LN_2;
    // How many times we've already seen each (subject, actor, kind) triple, to
    // apply the per-counterparty diminishing return.
    let mut pair_count: HashMap<(String, String, &'static str), u32> = HashMap::new();
    let mut scores: HashMap<String, f64> = HashMap::new();

    for ev in events {
        let (subject, actor, ts, base, kind) = ev.parts();
        if subject == actor || subject.is_empty() || actor.is_empty() {
            continue; // no self-dealing, no empty identities
        }
        let key = (subject.to_string(), actor.to_string(), kind);
        let k = pair_count.entry(key).or_insert(0);
        let diminish = p.diminish.powi(*k as i32);
        *k += 1;

        // Exponential time decay; future-dated events (clock skew) are clamped.
        let age = (now - ts).max(0) as f64;
        let decay = (-ln2 * age / p.half_life_secs).exp();

        *scores.entry(subject.to_string()).or_insert(0.0) += base * diminish * decay;
    }
    scores
}

/// Convenience: one identity's score (0.0 if it has no events).
pub fn score_of(scores: &HashMap<String, f64>, id: &str) -> f64 {
    scores.get(id).copied().unwrap_or(0.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn repro(s: &str, a: &str, ts: i64) -> RepEvent {
        RepEvent::Reproduced { subject: s.into(), actor: a.into(), ts }
    }

    #[test]
    fn distinct_vouchers_beat_a_colluding_pair() {
        let now = 1_000_000;
        // Honest: five DISTINCT accounts each reproduce alice once.
        let honest: Vec<_> =
            (0..5).map(|i| repro("alice", &format!("peer{i}"), now)).collect();
        // Colluding: one partner reproduces bob five times.
        let ring: Vec<_> = (0..5).map(|i| repro("bob", "buddy", now + i)).collect();

        let p = RepParams { half_life_secs: 1e12, diminish: 0.5 }; // kill time decay
        let ra = reputation(&honest, now, p);
        let rb = reputation(&ring, now, p);
        // alice: 5 * 3.0 = 15.  bob: 3.0 * (1 + .5 + .25 + .125 + .0625) = ~5.8.
        assert!((score_of(&ra, "alice") - 15.0).abs() < 1e-6);
        assert!(score_of(&rb, "bob") < 6.0);
        assert!(score_of(&ra, "alice") > score_of(&rb, "bob") * 2.0);
    }

    #[test]
    fn self_dealing_is_ignored() {
        let ev = vec![repro("alice", "alice", 1), RepEvent::Upvote {
            subject: "alice".into(),
            actor: "alice".into(),
            ts: 1,
        }];
        assert!(reputation(&ev, 1, RepParams::default()).is_empty());
    }

    #[test]
    fn time_decays_old_glory() {
        let p = RepParams { half_life_secs: 100.0, diminish: 1.0 };
        let ev = vec![repro("alice", "peer", 0)];
        let fresh = reputation(&ev, 0, p);
        let aged = reputation(&ev, 100, p); // exactly one half-life later
        assert!((score_of(&fresh, "alice") - 3.0).abs() < 1e-9);
        assert!((score_of(&aged, "alice") - 1.5).abs() < 1e-6);
    }

    #[test]
    fn failures_and_downvotes_are_negative() {
        let ev = vec![
            RepEvent::ReproductionFailed { subject: "alice".into(), actor: "p".into(), ts: 0 },
            RepEvent::Downvote { subject: "alice".into(), actor: "q".into(), ts: 0 },
        ];
        let p = RepParams { half_life_secs: 1e12, diminish: 1.0 };
        assert!((score_of(&reputation(&ev, 0, p), "alice") - (-5.0)).abs() < 1e-9);
    }
}
