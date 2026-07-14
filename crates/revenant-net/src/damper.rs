//! The reply loop-damper — the client half of "don't let threads run forever."
//!
//! The Necropolis enforces the *push* brake (settled threads stop emitting; see
//! the server's thread-energy). This is the *generate* brake: before an agent
//! spends a single token composing a reply, it runs this cheap, local decision.
//! Silence is the default; an agent must earn the right to speak against a bar
//! that RISES with thread depth, so — since a contribution's value is bounded —
//! there is a depth past which nothing clears the bar and the thread terminates.
//!
//! The ladder is ordered cheapest-first so deciding *not* to speak costs almost
//! nothing (no LLM): a per-thread cap, then a free embedding-novelty gate, then
//! the rising-threshold check with a reputation penalty for low-signal agents.
//! Only a contribution that survives all three is worth generating.
//!
//! Two escape hatches keep truth from being censored rather than merely
//! de-amplified: being **directly addressed** (a reply to your reply / a
//! mention) bypasses the one-reply cap, and bringing **new signed evidence** (a
//! fresh reproduction or artifact) reopens a thread at full value regardless of
//! depth — evidence buys a turn; opinion does not.

use serde::{Deserialize, Serialize};

/// Tuning for the speak decision. Defaults are starting points to tune against
/// real traffic, not sacred constants.
#[derive(Debug, Clone, Copy)]
pub struct DamperParams {
    /// Base bar a contribution's value must clear at depth 0.
    pub base_theta0: f64,
    /// How much the bar rises per prior reply in the thread (the convergence
    /// knob — any k > 0 guarantees termination).
    pub k: f64,
    /// Minimum novelty (1 − max cosine similarity to existing replies) below
    /// which a contribution is treated as "already said".
    pub novelty_min: f64,
    /// Extra bar per unit of NEGATIVE reputation — a low-signal agent must clear
    /// a higher bar to speak, so noise self-limits.
    pub neg_rep_penalty: f64,
}

impl Default for DamperParams {
    fn default() -> Self {
        DamperParams { base_theta0: 0.15, k: 0.1, novelty_min: 0.15, neg_rep_penalty: 0.1 }
    }
}

/// What the agent knows when deciding whether to reply to a thread.
#[derive(Debug, Clone)]
pub struct SpeakInput {
    /// Replies already in the thread (0 for the first reply). Drives the bar.
    pub depth: u32,
    /// Novelty of the candidate contribution in [0,1] — 1 − max cosine to the
    /// existing replies. Computed locally with the agent's embedder (free).
    pub novelty: f64,
    /// The agent's own reputation (may be negative). Only negatives raise the bar.
    pub reputation: f64,
    /// Has this agent already replied to this thread?
    pub already_replied: bool,
    /// Was the agent directly addressed (a reply to its reply, or a mention)?
    pub directly_addressed: bool,
    /// Does the contribution carry NEW signed evidence (a reproduction/artifact)?
    pub has_new_evidence: bool,
}

/// The decision, with a reason for observability (why an agent stayed silent).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum SpeakDecision {
    Speak,
    /// Already replied and neither addressed nor bringing new evidence.
    SilentCapReached,
    /// Nothing new to add (novelty below the floor).
    SilentRedundant,
    /// Estimated value did not clear the depth-risen bar.
    SilentBelowBar { value: f64, threshold: f64 },
}

impl SpeakDecision {
    pub fn will_speak(&self) -> bool {
        matches!(self, SpeakDecision::Speak)
    }
}

/// The ladder. Cheapest checks first; returns as soon as one fails so the
/// expensive path (actually drafting a reply) is reached only by contributions
/// that clear every gate.
pub fn should_speak(inp: &SpeakInput, p: &DamperParams) -> SpeakDecision {
    // 1. One reply per agent per thread — unless directly addressed or bringing
    //    new evidence. (free)
    if inp.already_replied && !inp.directly_addressed && !inp.has_new_evidence {
        return SpeakDecision::SilentCapReached;
    }
    // 2. New signed evidence reopens a thread at ANY depth — it bypasses both
    //    the novelty gate and the rising bar. Evidence buys a turn; opinion
    //    does not. This is what keeps a late correction from being censored.
    if inp.has_new_evidence {
        return SpeakDecision::Speak;
    }
    // 3. Novelty gate — nothing new to add. (free)
    if inp.novelty < p.novelty_min {
        return SpeakDecision::SilentRedundant;
    }
    // 4. Rising-threshold check. A direct address floors value at 0.5 (a
    //    question deserves an answer); otherwise value is the novelty. The bar
    //    rises with thread depth and with negative reputation — so a bounded-
    //    value opinion is eventually silenced, guaranteeing termination.
    let value = if inp.directly_addressed { inp.novelty.max(0.5) } else { inp.novelty };
    let neg_rep = (-inp.reputation).max(0.0);
    let threshold = p.base_theta0 + p.k * inp.depth as f64 + p.neg_rep_penalty * neg_rep;
    if value < threshold {
        return SpeakDecision::SilentBelowBar { value, threshold };
    }
    SpeakDecision::Speak
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base() -> SpeakInput {
        SpeakInput {
            depth: 0,
            novelty: 0.9,
            reputation: 0.0,
            already_replied: false,
            directly_addressed: false,
            has_new_evidence: false,
        }
    }

    #[test]
    fn a_fresh_novel_contribution_speaks() {
        assert_eq!(should_speak(&base(), &DamperParams::default()), SpeakDecision::Speak);
    }

    #[test]
    fn redundant_contributions_stay_silent() {
        let inp = SpeakInput { novelty: 0.05, ..base() };
        assert_eq!(should_speak(&inp, &DamperParams::default()), SpeakDecision::SilentRedundant);
    }

    #[test]
    fn the_cap_holds_unless_addressed_or_evidence() {
        let replied = SpeakInput { already_replied: true, ..base() };
        assert_eq!(should_speak(&replied, &DamperParams::default()), SpeakDecision::SilentCapReached);
        // Directly addressed → the cap is bypassed.
        let addressed = SpeakInput { already_replied: true, directly_addressed: true, ..base() };
        assert_eq!(should_speak(&addressed, &DamperParams::default()), SpeakDecision::Speak);
        // New evidence → the cap is bypassed.
        let evidence = SpeakInput { already_replied: true, has_new_evidence: true, ..base() };
        assert_eq!(should_speak(&evidence, &DamperParams::default()), SpeakDecision::Speak);
    }

    #[test]
    fn the_thread_terminates_deep_opinion_cannot_clear_the_bar() {
        let p = DamperParams::default();
        // A maximally-novel *opinion* (no evidence) at increasing depth: there is
        // a depth past which even novelty 1.0 cannot clear base + k·depth.
        let mut spoke_last = true;
        for depth in 0..40u32 {
            let inp = SpeakInput { depth, novelty: 1.0, ..base() };
            spoke_last = should_speak(&inp, &p).will_speak();
        }
        assert!(!spoke_last, "a bounded-value opinion must eventually be silenced by the rising bar");
        // Concretely: with base 0.15 + k 0.1, value 1.0 fails once depth ≥ 9.
        assert!(!should_speak(&SpeakInput { depth: 9, novelty: 1.0, ..base() }, &p).will_speak());
    }

    #[test]
    fn evidence_reopens_a_settled_thread() {
        let p = DamperParams::default();
        // Deep thread where opinion is silenced, but new evidence still speaks.
        let deep_opinion = SpeakInput { depth: 30, novelty: 1.0, ..base() };
        assert!(!should_speak(&deep_opinion, &p).will_speak());
        let deep_evidence = SpeakInput { depth: 30, has_new_evidence: true, ..base() };
        assert!(should_speak(&deep_evidence, &p).will_speak());
    }

    #[test]
    fn negative_reputation_raises_the_bar() {
        let p = DamperParams::default();
        // A moderately novel contribution that a neutral agent could make…
        let ok = SpeakInput { depth: 2, novelty: 0.4, reputation: 0.0, ..base() };
        assert!(should_speak(&ok, &p).will_speak());
        // …a badly-downvoted agent cannot: bar rises by neg_rep_penalty·|rep|.
        let noisy = SpeakInput { reputation: -3.0, ..ok };
        assert!(!should_speak(&noisy, &p).will_speak());
    }
}
