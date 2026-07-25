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

/// Most spawned tasks alive at once across the WHOLE tree.
///
/// This is the rail the token pool cannot provide. A spiral that fans out wide
/// and fast can queue hundreds of children before any of them has spent enough
/// for the pool to notice — the money is committed before the accounting catches
/// up. Conversely a serial grind keeps the live count at 1 forever, which only
/// the pool catches. Depth catches neither: a two-level tree can still be
/// enormous. All three rails are needed; none subsumes another.
pub const MAX_LIVE_DESCENDANTS: i64 = 16;

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
/// Proof that a spawn slot was claimed. Releasing it is [`Drop`], deliberately:
/// a slot must come back on the error and cancellation paths too, not only on a
/// clean return. Tying release to scope exit rather than to an explicit call is
/// what stops a panicking or aborted child from leaking the tree's capacity.
#[derive(Debug)]
pub struct DescendantSlot {
    live: Arc<AtomicI64>,
}

impl Drop for DescendantSlot {
    fn drop(&mut self) {
        // Floor at zero: a double-release bug must not manufacture capacity.
        let _ = self.live.fetch_update(Ordering::AcqRel, Ordering::Acquire, |n| {
            Some((n - 1).max(0))
        });
    }
}

#[derive(Debug, Clone)]
pub struct TaskBudget {
    remaining: Arc<AtomicI64>,
    /// Spawned tasks currently alive anywhere in this tree. Shared like the token
    /// pool, so the cap bounds the whole fan-out rather than each parent.
    live: Arc<AtomicI64>,
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
        TaskBudget {
            remaining: Arc::new(AtomicI64::new(tokens)),
            live: Arc::new(AtomicI64::new(0)),
            total: tokens,
            depth: 0,
        }
    }

    /// No accounting. Used when the owner has configured no budget — the gateway
    /// spend cap is still the outer moat, so this is "untracked here", not
    /// "unbounded everywhere".
    pub fn unlimited() -> Self {
        TaskBudget {
            remaining: Arc::new(AtomicI64::new(i64::MAX)),
            live: Arc::new(AtomicI64::new(0)),
            total: i64::MAX,
            depth: 0,
        }
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
    fn child(&self) -> Option<Self> {
        if self.depth >= MAX_TASK_DEPTH {
            return None;
        }
        Some(TaskBudget {
            remaining: Arc::clone(&self.remaining),
            live: Arc::clone(&self.live),
            total: self.total,
            depth: self.depth + 1,
        })
    }

    /// Spawned tasks currently alive in this tree.
    pub fn live_descendants(&self) -> i64 {
        self.live.load(Ordering::Relaxed).max(0)
    }

    /// Claim capacity for a child: a budget one level deeper plus the slot that
    /// holds its place in the live count.
    ///
    /// `None` means refused — depth cap, live cap, or an exhausted pool. Refusal
    /// is IMMEDIATE and never waits for a slot to free. A blocking semaphore here
    /// would deadlock under nesting: a parent holds its own slot while awaiting
    /// its children, so waiting for capacity that only a descendant can release
    /// is a cycle. Refusing lets the caller return a partial result instead of
    /// hanging.
    pub fn spawn_child(&self) -> Option<(TaskBudget, DescendantSlot)> {
        let slot = self.try_spawn_at(self.depth)?;
        // depth was already validated by try_spawn_at, so child() cannot fail.
        let child = self.child()?;
        Some((child, slot))
    }

    /// Claim a spawn slot when the caller tracks depth itself (the agent threads
    /// `depth` through its turn loop). Same three refusals as [`spawn_child`].
    pub fn try_spawn_at(&self, depth: u8) -> Option<DescendantSlot> {
        if depth >= MAX_TASK_DEPTH {
            return None;
        }
        // A spawn with nothing left to spend would only produce a child that
        // immediately fails — refuse at the parent, where a partial result can
        // still be assembled.
        if !self.is_unlimited() && self.remaining() <= 0 {
            return None;
        }
        // Lock-free claim: only succeed if we are strictly under the cap. CAS
        // rather than fetch_add so a burst of concurrent spawns cannot briefly
        // overshoot and then correct.
        self.live
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |n| {
                (n < MAX_LIVE_DESCENDANTS).then_some(n + 1)
            })
            .ok()
            .map(|_| DescendantSlot { live: Arc::clone(&self.live) })
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
    fn the_seventeenth_concurrent_descendant_is_refused() {
        let root = TaskBudget::root(1_000_000);
        // Hold the slots: capacity is about what is ALIVE, not what has ever run.
        let mut slots: Vec<_> = (0..MAX_LIVE_DESCENDANTS)
            .map(|i| root.spawn_child().unwrap_or_else(|| panic!("child {i} within cap")))
            .collect();
        assert_eq!(root.live_descendants(), MAX_LIVE_DESCENDANTS);
        assert!(root.spawn_child().is_none(), "17th concurrent descendant must be refused");

        // Free exactly ONE → one more may spawn, and no more. (`pop` rather than
        // `into_iter().next()`, which would drop the whole Vec and release all 16.)
        drop(slots.pop().expect("a slot to free"));
        assert_eq!(root.live_descendants(), MAX_LIVE_DESCENDANTS - 1);
        let reused = root.spawn_child();
        assert!(reused.is_some(), "a freed slot must be reusable");
        assert!(root.spawn_child().is_none(), "still capped after reuse");
    }

    #[test]
    fn a_slot_frees_on_the_error_path_not_just_a_clean_return() {
        // Drop-based release is the point: a child that panics or is cancelled
        // must still give its capacity back.
        let root = TaskBudget::root(1_000);
        let before = root.live_descendants();

        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _slot = root.spawn_child().expect("slot").1;
            assert_eq!(root.live_descendants(), before + 1);
            panic!("child blew up");
        }));
        assert!(result.is_err(), "the child did panic");
        assert_eq!(root.live_descendants(), before, "slot released by unwinding");

        // Early return (the ordinary error path) releases too.
        fn bail_early(b: &TaskBudget) -> Result<(), &'static str> {
            let _slot = b.spawn_child().ok_or("refused")?.1;
            Err("failed after spawning")
        }
        assert!(bail_early(&root).is_err());
        assert_eq!(root.live_descendants(), before, "slot released on early return");
    }

    #[test]
    fn depth_and_descendant_caps_refuse_independently() {
        // Depth exhausted, live count empty ⇒ still refused.
        let mut deep = TaskBudget::root(1_000);
        let mut keep = Vec::new();
        for _ in 0..MAX_TASK_DEPTH {
            let (child, slot) = deep.spawn_child().expect("within depth");
            keep.push(slot);
            deep = child;
        }
        assert!(deep.spawn_child().is_none(), "depth cap refuses on its own");

        // Live count exhausted at depth 0 ⇒ refused even though depth is fine.
        let shallow = TaskBudget::root(1_000);
        let _held: Vec<_> =
            (0..MAX_LIVE_DESCENDANTS).map(|_| shallow.spawn_child().unwrap()).collect();
        assert_eq!(shallow.depth(), 0);
        assert!(shallow.spawn_child().is_none(), "live cap refuses on its own");
    }

    #[test]
    fn an_exhausted_pool_refuses_a_spawn() {
        // A child with nothing to spend would only fail immediately; refuse at the
        // parent, where a partial result can still be assembled.
        let root = TaskBudget::root(100);
        assert_eq!(root.charge(100), Spend::Ok);
        assert_eq!(root.remaining(), 0);
        assert!(root.spawn_child().is_none(), "no budget left ⇒ no spawn");
        // ...and refusing to spawn does not poison the level already running.
        assert!(root.would_fit(0));
    }

    #[test]
    fn slot_claiming_does_not_over_grant_under_contention() {
        // The CAS is the only thing bounding a concurrent spiral.
        let root = TaskBudget::root(i64::MAX - 1);
        let granted = Arc::new(AtomicI64::new(0));
        let hold = Arc::new(std::sync::Mutex::new(Vec::new()));
        let handles: Vec<_> = (0..8)
            .map(|_| {
                let (b, g, h) = (root.clone(), Arc::clone(&granted), Arc::clone(&hold));
                std::thread::spawn(move || {
                    for _ in 0..50 {
                        if let Some((_, slot)) = b.spawn_child() {
                            g.fetch_add(1, Ordering::Relaxed);
                            h.lock().unwrap().push(slot); // hold, never release
                        }
                    }
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }
        assert_eq!(
            granted.load(Ordering::Relaxed),
            MAX_LIVE_DESCENDANTS,
            "concurrent spawns must not exceed the live cap"
        );
    }

    #[test]
    fn summary_reports_real_numbers() {
        let b = TaskBudget::root(10_000);
        let kid = b.child().unwrap();
        assert_eq!(kid.charge(8_000), Spend::Ok);
        assert_eq!(kid.summary(), "spent 8000 of 10000 tokens (depth 1)");
    }
}
