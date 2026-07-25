//! Subscription-backed inference: using an interactive coding CLI (Claude Code)
//! as a deliberate, metered execution path alongside the metered API path.
//!
//! ## Why this exists as its own module
//!
//! The agent discovered on its own that it could shell out to `claude -p` and get
//! work done. That is a real capability, but as a bare `exec` it bypassed every
//! cost rail in the tree: no `record_spend`, so invisible to the ledger and to
//! budget alerts; no `TaskBudget::charge`, so invisible to the task pool; no
//! gateway request, so invisible to the rolling spend cap. Three rails, none of
//! which applied. Making it first-class is how it becomes visible.
//!
//! ## Two accounting decisions that matter
//!
//! **1. Tokens are recorded, dollars are NOT.** The CLI reports
//! `total_cost_usd`, but that is the API-equivalent list price of the work — not
//! what a subscription holder pays, which is a flat fee already spent. Feeding it
//! into the spend ledger as cost would invent charges that never happen and make
//! `revenant spend` lie. So tokens go in under a label with no price entry
//! (`sub:<model>`), and the reported figure is kept separately as
//! `api_equivalent_usd` — useful for deciding whether a job belongs on the
//! subscription or the API, and useless as a bill.
//!
//! **2. Cache-creation tokens count as consumption.** Measured against the real
//! CLI: a prompt of 3 input / 4 output tokens also created 30,685 cache tokens,
//! because each invocation re-establishes the CLI's own context. Ignoring those
//! would under-report a one-shot call by four orders of magnitude and make the
//! path look free. They are counted, which is also what makes the fixed overhead
//! per invocation visible enough to discourage spraying one-shot calls.

use serde::{Deserialize, Serialize};

/// Prefix marking a model as subscription-metered. Deliberately not a real
/// provider model id, so no pricing table matches it and no dollar figure is
/// ever derived from it.
pub const SUB_MODEL_PREFIX: &str = "sub:";

/// What one subscription-backed invocation consumed.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SubUsage {
    /// Model label for the spend ledger, e.g. `sub:claude-opus-4-6`.
    pub model_label: String,
    /// Prompt tokens, INCLUDING cache creation and cache reads — see the module
    /// note: excluding them makes a one-shot call look ~10,000x cheaper.
    pub input_tokens: i64,
    pub output_tokens: i64,
    /// Cache-creation tokens alone, kept separately so the fixed per-invocation
    /// overhead is inspectable rather than buried in the input total.
    pub cache_creation_tokens: i64,
    /// What this work would have cost on the metered API. NOT a charge — the
    /// subscription is flat-rate. Never summed into spend.
    pub api_equivalent_usd: f64,
    /// True when the CLI itself reported failure; the caller should treat the
    /// output as an error even though tokens were still consumed.
    pub is_error: bool,
    /// Tool calls the CLI refused (its own permission prompts). Non-empty means
    /// the work may be incomplete for reasons unrelated to our own gating.
    pub permission_denials: i64,
}

impl SubUsage {
    /// Total tokens to charge against a [`crate::budget::TaskBudget`].
    pub fn billable_tokens(&self) -> i64 {
        self.input_tokens.saturating_add(self.output_tokens)
    }
}

/// Parse the `--output-format json` result envelope.
///
/// Deliberately tolerant: an unexpected or partial envelope must still yield
/// usage where possible, because the alternative is recording NOTHING for work
/// that really happened. Every field falls back to a defensible zero, and the
/// model label falls back to a generic one rather than being dropped — an
/// unlabelled charge is worse than a coarsely-labelled one.
pub fn parse_result_envelope(json: &str) -> Option<SubUsage> {
    let v: serde_json::Value = serde_json::from_str(json).ok()?;
    // A stream-json tail or a non-result frame is not usage; refuse rather than
    // fabricate zeros that would look like a free call.
    if v.get("usage").is_none() && v.get("modelUsage").is_none() {
        return None;
    }
    let usage = v.get("usage");

    let num = |parent: Option<&serde_json::Value>, key: &str| -> i64 {
        parent.and_then(|u| u.get(key)).and_then(|n| n.as_i64()).unwrap_or(0)
    };
    let cache_creation = num(usage, "cache_creation_input_tokens");
    let cache_read = num(usage, "cache_read_input_tokens");
    let input = num(usage, "input_tokens") + cache_creation + cache_read;
    let output = num(usage, "output_tokens");

    // modelUsage is keyed by model id; take the busiest entry so a multi-model
    // run is attributed to where the work actually went.
    let model = v
        .get("modelUsage")
        .and_then(|m| m.as_object())
        .and_then(|m| {
            m.iter()
                .max_by_key(|(_, u)| {
                    u.get("outputTokens").and_then(|n| n.as_i64()).unwrap_or(0)
                })
                .map(|(k, _)| k.clone())
        })
        .unwrap_or_else(|| "unknown".to_string());

    Some(SubUsage {
        model_label: format!("{SUB_MODEL_PREFIX}{model}"),
        input_tokens: input,
        output_tokens: output,
        cache_creation_tokens: cache_creation,
        api_equivalent_usd: v.get("total_cost_usd").and_then(|c| c.as_f64()).unwrap_or(0.0),
        is_error: v.get("is_error").and_then(|e| e.as_bool()).unwrap_or(false),
        permission_denials: v
            .get("permission_denials")
            .and_then(|d| d.as_array())
            .map(|a| a.len() as i64)
            .unwrap_or(0),
    })
}

/// Is this spend-ledger model label subscription-metered? Pricing and any
/// dollar-denominated report must skip these — the money was already spent as a
/// flat fee, so pricing them would double-count.
pub fn is_subscription_label(model: &str) -> bool {
    model.starts_with(SUB_MODEL_PREFIX)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The real envelope, captured verbatim from `claude -p --output-format json`
    /// on 2026-07-25. Using the actual bytes rather than a hand-written sample is
    /// the point: the field names and nesting are the contract, and a fixture I
    /// invented could agree with my assumptions and still be wrong.
    const REAL: &str = r#"{"is_error":false,"duration_api_ms":2313,"num_turns":1,"stop_reason":"end_turn","session_id":"9533657f","total_cost_usd":0.306965,"usage":{"input_tokens":3,"cache_creation_input_tokens":30685,"cache_read_input_tokens":0,"output_tokens":4,"service_tier":"standard","inference_geo":"not_available","speed":"standard"},"modelUsage":{"claude-opus-4-6":{"inputTokens":3,"outputTokens":4,"cacheReadInputTokens":0,"cacheCreationInputTokens":30685,"costUSD":0.306965,"contextWindow":200000,"provider":"firstParty"}},"permission_denials":[],"terminal_reason":"completed","subtype":"success","result":"ok","type":"result"}"#;

    #[test]
    fn parses_the_real_envelope() {
        let u = parse_result_envelope(REAL).expect("real envelope parses");
        assert_eq!(u.model_label, "sub:claude-opus-4-6");
        assert!(!u.is_error);
        assert_eq!(u.output_tokens, 4);
        assert_eq!(u.permission_denials, 0);
        assert!((u.api_equivalent_usd - 0.306965).abs() < 1e-9);
    }

    /// The measurement that justifies counting cache tokens: a 3-token prompt
    /// consumed 30,685 cache-creation tokens. Charging only `input_tokens` would
    /// under-report this invocation by more than four orders of magnitude and
    /// make the subscription path look free.
    #[test]
    fn cache_creation_dominates_a_one_shot_call() {
        let u = parse_result_envelope(REAL).unwrap();
        assert_eq!(u.cache_creation_tokens, 30_685);
        assert_eq!(u.input_tokens, 3 + 30_685, "cache tokens are consumption");
        assert_eq!(u.billable_tokens(), 30_692);
        // The naive reading, for contrast — this is the bug being avoided.
        assert!(
            u.billable_tokens() > 4_000 * (3 + 4),
            "ignoring cache tokens would understate this call by >4 orders of magnitude"
        );
    }

    /// Subscription labels must never be priced: the fee is already paid, so
    /// deriving dollars from them would invent charges that never occur.
    #[test]
    fn subscription_labels_are_excluded_from_pricing() {
        let u = parse_result_envelope(REAL).unwrap();
        assert!(is_subscription_label(&u.model_label));
        // Real API model ids must NOT match, or genuine spend would go unpriced.
        for api in ["claude-sonnet-5", "claude-haiku-4-5-20251001", "kimi-k3", "gpt-4o"] {
            assert!(!is_subscription_label(api), "{api} must still be priced");
        }
        // The reported figure is carried, but as api_equivalent — a decision
        // input, not a charge.
        assert!(u.api_equivalent_usd > 0.0);
    }

    #[test]
    fn a_failed_run_still_reports_what_it_burned() {
        // Tokens are spent whether or not the CLI succeeded; hiding them would
        // let repeated failures cost real money invisibly.
        let failed = r#"{"is_error":true,"total_cost_usd":0.02,
            "usage":{"input_tokens":100,"output_tokens":5,"cache_creation_input_tokens":9000,"cache_read_input_tokens":1000},
            "modelUsage":{"claude-sonnet-5":{"outputTokens":5}},
            "permission_denials":[{"tool":"Bash"},{"tool":"Write"}],"type":"result"}"#;
        let u = parse_result_envelope(failed).unwrap();
        assert!(u.is_error);
        assert_eq!(u.input_tokens, 100 + 9_000 + 1_000);
        assert_eq!(u.billable_tokens(), 10_105);
        assert_eq!(u.permission_denials, 2, "the CLI's own refusals are surfaced");
    }

    #[test]
    fn partial_envelopes_degrade_rather_than_drop_the_charge() {
        // Missing modelUsage: still charge, under a coarse label. An unlabelled
        // charge beats a silent one.
        let no_model = r#"{"usage":{"input_tokens":10,"output_tokens":20},"type":"result"}"#;
        let u = parse_result_envelope(no_model).unwrap();
        assert_eq!(u.model_label, "sub:unknown");
        assert_eq!(u.billable_tokens(), 30);
        assert_eq!(u.api_equivalent_usd, 0.0, "absent cost is 0, never invented");

        // Something that carries no usage at all is NOT a free call — it is not a
        // usage record, and must be refused rather than logged as zero.
        assert!(parse_result_envelope(r#"{"type":"system","subtype":"init"}"#).is_none());
        assert!(parse_result_envelope("not json").is_none());
        assert!(parse_result_envelope("").is_none());
    }
}
