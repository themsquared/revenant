//! The materiality judge (Gate 1.5).
//!
//! An eval-proven change isn't automatically worth the HORDE'S attention. Two
//! very different things pass the scorecard: a genuine core improvement that
//! makes any revenant do more, faster, with less — and an owner-specific tweak
//! (a config value, personal data, a one-off) that helps only this box. Only
//! the first deserves an auto-PR to the OSS repo; the second should stay local.
//!
//! This judge decides. It reads the diff + the eval evidence and rules on
//! (a) generalizability — would this help ANY revenant, not just this owner —
//! and (b) which fitness axis it moves (accuracy / speed / cost / capability).
//! It fails CLOSED to "keep local": when in doubt we do NOT spend the horde's
//! review budget or the OSS repo's signal-to-noise on a marginal change.

use crate::EvidenceBundle;
use anyhow::Result;
use revenant_client::Client;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MaterialityVerdict {
    /// The gate: PR to the horde only when true.
    pub horde_worthy: bool,
    /// Helps any revenant (true) vs. only this owner's box (false).
    #[serde(default)]
    pub generalizable: bool,
    /// Primary fitness axis moved: accuracy | speed | cost | capability | none.
    #[serde(default)]
    pub axis: String,
    #[serde(default)]
    pub reasons: Vec<String>,
}

impl MaterialityVerdict {
    /// The safe default: don't PR, keep the change on this box.
    pub fn keep_local(reason: impl Into<String>) -> Self {
        MaterialityVerdict {
            horde_worthy: false,
            generalizable: false,
            axis: "none".into(),
            reasons: vec![reason.into()],
        }
    }
}

/// Judge whether a proven change is worth auto-PRing to the horde. Fails closed
/// (keep local) on any unparseable or doubtful answer.
pub async fn judge_materiality(
    client: &Client,
    tier: &str,
    evidence: &EvidenceBundle,
    diff: &str,
) -> Result<MaterialityVerdict> {
    let reply = crate::review::ask(client, &build_prompt(evidence, diff), tier).await?;
    Ok(parse(&reply))
}

fn build_prompt(evidence: &EvidenceBundle, diff: &str) -> String {
    format!(
        "You are the MATERIALITY JUDGE for an autonomous agent's self-improvement pipeline. A \
         change has already passed build/test/clippy and an eval bar. Your ONLY question: is it \
         worth opening a pull request to the shared open-source repository that the whole horde of \
         agents runs?\n\n\
         Approve (horde_worthy=true) ONLY if BOTH hold:\n\
         1. GENERALIZABLE — it improves the agent for ANY owner, not just this box. Reject \
         owner-specific edits: hard-coded config values, personal data/paths, credentials, \
         one-off content, or anything that only makes sense for one user.\n\
         2. MATERIAL — it meaningfully moves a real fitness axis: accuracy (better answers), \
         speed (lower latency), cost (fewer tokens/$), or capability (does more). The guiding \
         principle is 'do more, faster, with less'. Cosmetic, trivial, or speculative changes are \
         NOT material.\n\n\
         Default to horde_worthy=FALSE when uncertain — a false PR wastes the horde's review \
         budget and pollutes the repo. Keeping a good-but-local change on this box is fine.\n\n\
         ## Eval evidence\n{}\n\n## Diff\n```diff\n{}\n```\n\n\
         Respond with ONLY this JSON:\n\
         {{\"horde_worthy\": bool, \"generalizable\": bool, \"axis\": \"accuracy|speed|cost|capability|none\", \"reasons\": [\"...\"]}}",
        evidence.markdown(),
        clip(diff, 12000),
    )
}

fn clip(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        format!("{}\n…(diff truncated)…", &s[..max])
    }
}

fn parse(reply: &str) -> MaterialityVerdict {
    let (Some(start), Some(end)) = (reply.find('{'), reply.rfind('}')) else {
        return MaterialityVerdict::keep_local("judge returned no JSON verdict");
    };
    match serde_json::from_str::<MaterialityVerdict>(&reply[start..=end]) {
        Ok(v) => v,
        Err(e) => MaterialityVerdict::keep_local(format!("unparseable materiality verdict: {e}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_horde_worthy() {
        let v = parse(r#"ok: {"horde_worthy": true, "generalizable": true, "axis": "speed", "reasons": ["cuts p50 latency for everyone"]}"#);
        assert!(v.horde_worthy && v.generalizable);
        assert_eq!(v.axis, "speed");
    }

    #[test]
    fn parse_keep_local() {
        let v = parse(r#"{"horde_worthy": false, "generalizable": false, "axis": "none", "reasons": ["hard-codes an owner path"]}"#);
        assert!(!v.horde_worthy);
    }

    #[test]
    fn unparseable_keeps_local() {
        assert!(!parse("looks generalizable to me!").horde_worthy);
        assert!(!parse("").horde_worthy);
    }
}
