//! Hierarchical task budgets: the rail that makes a recursive agent safe to run
//! unattended.
//!
//! The failure this exists to stop: an agent spawns a child, the child spawns
//! another, one of them fails in a way that looks retryable, and the tree burns
//! money until someone notices. Per-turn iteration caps do not help — they bound
//! ONE turn, and a fan-out is many turns.
//!
//! ## Why the pool is shared, not carved
//!
//! The obvious reading of "a child may not exceed its parent's remaining budget"
//! is to clamp each child's ceiling to what the parent has left. That is not
//! enough: N children each clamped to the parent's remainder can still sum to N
//! times it, which is exactly the runaway. So a [`TaskBudget`] holds a handle to
//! ONE counter shared down the whole subtree, and every child debits it. The
//! bound is therefore on total spend across the fan-out, not per-branch.
//!
//! ## Depth
//!
//! Nesting is capped ([`MAX_TASK_DEPTH`]). Depth is not a cost control — the
//! shared pool already is — it is a *shape* control: it stops an agent that has
//! convinced itself the answer is one more layer down from building a tower
//! whose intermediate results nobody will ever read.

use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::Arc;

/// Hard nesting limit for spawned work (root = depth 0).
pub const MAX_TASK_DEPTH: u8 = 4;

/// What happened when work asked to spend.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Spend {
    /// Charged in full; keep going.
    Ok,
    /// The pool is exhausted. The caller must return what it HAS — a structured
    /// partial result — never panic and never silently truncate. `remaining` is
    /// what was left (always ≤ the amount asked for).
    Exhausted { asked: i64, remaining: i64 },
}

/// A budget shared by one task and everything it spawns.
///
/// Cloning shares the pool; [`TaskBudget::child`] shares it AND increments depth.
#[derive(Debug, Clone)]
pub struct TaskBudget {
    remaining: Arc<AtomicI64>,
    /// Total the root started with — kept for reporting a meaningful "spent X of
    /// Y" rather than only a remainder.
    total: i64,
    depth: u8,
}

impl TaskBudget {
    /// A fresh root budget of `tokens`. A non-positive total means "unlimited",
    /// which is represented honestly as [`TaskBudget::unlimited`] rather than as
    /// a sentinel number that later arithmetic could mistake for a real bound.
    pub fn root(tokens: i64) -> Self {
        if tokens <= 0 {
            return Self::unlimited();
        }
        TaskBudget { remaining: Arc::new(AtomicI64::new(tokens)), total: tokens, depth: 0 }
    }

    /// No accounting. Used when the owner has configured no budget — the gateway
    /// spend cap is still the outer moat, so this is "untracked here", not
    /// "unbounded everywhere".
    pub fn unlimited() -> Self {
        TaskBudget { remaining: Arc::new(AtomicI64::new(i64::MAX)), total: i64::MAX, depth: 0 }
    }

    pub fn is_unlimited(&self) -> bool {
        self.total == i64::MAX
    }

    pub fn depth(&self) -> u8 {
        self.depth
    }

    pub fn remaining(&self) -> i64 {
        self.remaining.load(Ordering::Relaxed).max(0)
    }

    pub fn total(&self) -> i64 {
        self.total
    }

    pub fn spent(&self) -> i64 {
        if self.is_unlimited() {
            return 0;
        }
        self.total - self.remaining()
    }

    /// A budget for spawned work: same pool, one level deeper.
    ///
    /// `None` means the depth cap is reached and the caller must NOT spawn. It is
    /// deliberately not an error type — refusing to nest further is a normal
    /// outcome the caller reports as a partial result, the same as exhaustion.
    pub fn child(&self) -> Option<Self> {
        if self.depth >= MAX_TASK_DEPTH {
            return None;
        }
        Some(TaskBudget {
            remaining: Arc::clone(&self.remaining),
            total: self.total,
            depth: self.depth + 1,
        })
    }

    /// Charge `tokens` against the shared pool.
    ///
    /// Debits even when it cannot cover the full amount — the spend already
    /// HAPPENED (the tokens went to a provider), so hiding it would understate
    /// real cost. The pool floors at zero so a large overrun cannot wrap or make
    /// later reporting nonsensical.
    pub fn charge(&self, tokens: i64) -> Spend {
        if self.is_unlimited() || tokens <= 0 {
            return Spend::Ok;
        }
        let before = self.remaining.fetch_sub(tokens, Ordering::Relaxed);
        if before >= tokens {
            return Spend::Ok;
        }
        // Overshot (or already empty): clamp the floor and report honestly.
        self.remaining.store(0, Ordering::Relaxed);
        Spend::Exhausted { asked: tokens, remaining: before.max(0) }
    }

    /// Would `tokens` fit? Advisory only — check before an expensive call to fail
    /// early, but [`charge`] is the authority because it is atomic.
    pub fn would_fit(&self, tokens: i64) -> bool {
        self.is_unlimited() || self.remaining() >= tokens
    }

    /// One line for a partial-result payload or a log: "spent 8000 of 10000
    /// tokens (depth 2)".
    pub fn summary(&self) -> String {
        if self.is_unlimited() {
            return format!("untracked budget (depth {})", self.depth);
        }
        format!("spent {} of {} tokens (depth {})", self.spent(), self.total, self.depth)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_fan_out_cannot_exceed_the_root_total() {
        // THE property this module exists for. Three children, each individually
        // "within the parent's remaining budget" at the moment it starts, must
        // still not sum past the root — which is what per-branch clamping alone
        // would allow.
        let root = TaskBudget::root(1000);
        let kids: Vec<TaskBudget> = (0..3).map(|_| root.child().unwrap()).collect();

        assert_eq!(kids[0].charge(400), Spend::Ok);
        assert_eq!(kids[1].charge(400), Spend::Ok);
        // The third would take the total to 1200 — refused, with the truth about
        // how much was actually left.
        assert_eq!(kids[2].charge(400), Spend::Exhausted { asked: 400, remaining: 200 });

        assert_eq!(root.remaining(), 0, "pool is shared, so the root sees the drain");
        assert_eq!(root.spent(), 1000);
    }

    #[test]
    fn depth_is_capped_and_refusal_is_not_an_error() {
        let mut budget = TaskBudget::root(100);
        for expected in 1..=MAX_TASK_DEPTH {
            budget = budget.child().expect("within cap");
            assert_eq!(budget.depth(), expected);
        }
        assert!(budget.child().is_none(), "must refuse to nest past MAX_TASK_DEPTH");
        // The budget itself is still usable — a refusal to go deeper does not
        // poison the level that is already running.
        assert_eq!(budget.charge(10), Spend::Ok);
    }

    #[test]
    fn an_overrun_is_recorded_not_hidden() {
        // The tokens were already spent at the provider, so the pool must reflect
        // it even though the request "failed".
        let b = TaskBudget::root(100);
        assert_eq!(b.charge(250), Spend::Exhausted { asked: 250, remaining: 100 });
        assert_eq!(b.remaining(), 0, "floors at zero, never negative");
        assert_eq!(b.spent(), 100);
        // Every later attempt is refused, reporting nothing left.
        assert_eq!(b.charge(1), Spend::Exhausted { asked: 1, remaining: 0 });
    }

    #[test]
    fn unlimited_is_explicit_rather_than_a_magic_number() {
        for b in [TaskBudget::unlimited(), TaskBudget::root(0), TaskBudget::root(-5)] {
            assert!(b.is_unlimited());
            assert_eq!(b.charge(i64::MAX / 2), Spend::Ok);
            assert_eq!(b.spent(), 0, "untracked, so 'spent' must not be invented");
            assert!(b.would_fit(1_000_000));
            assert!(b.summary().contains("untracked"));
        }
    }

    #[test]
    fn children_share_the_pool_across_threads() {
        // The counter is the only thing bounding a concurrent fan-out, so it has
        // to hold under real parallelism, not just sequential calls.
        let root = TaskBudget::root(1_000);
        let handles: Vec<_> = (0..10)
            .map(|_| {
                let child = root.child().unwrap();
                std::thread::spawn(move || {
                    let mut ok = 0;
                    for _ in 0..100 {
                        if child.charge(10) == Spend::Ok {
                            ok += 1;
                        }
                    }
                    ok
                })
            })
            .collect();
        let granted: i32 = handles.into_iter().map(|h| h.join().unwrap()).sum();
        // 1000 tokens at 10 each = exactly 100 successful charges, no more.
        assert_eq!(granted, 100, "shared counter must not over-grant under contention");
        assert_eq!(root.remaining(), 0);
    }

    #[test]
    fn summary_reports_real_numbers() {
        let b = TaskBudget::root(10_000);
        let kid = b.child().unwrap();
        assert_eq!(kid.charge(8_000), Spend::Ok);
        assert_eq!(kid.summary(), "spent 8000 of 10000 tokens (depth 1)");
    }
}
