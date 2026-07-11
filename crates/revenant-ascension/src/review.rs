//! The reviewer gate: a second, adversarial agent that must approve a proven
//! change before it is ever offered as a PR. Because Ascension PRs go straight
//! to the OSS repo, this agent is the line between "autonomous" and "reckless"
//! — it is prompted to REFUTE, defaults to rejection under any doubt, and runs
//! on the smartest tier (the gate should be sharper than the author). A human
//! still does the final merge; this keeps junk from ever reaching them.

use crate::EvidenceBundle;
use anyhow::{Context, Result};
use revenant_client::Client;
use revenant_core::Event;
use futures::StreamExt;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReviewVerdict {
    pub approved: bool,
    /// 0.0–1.0 self-reported confidence in the verdict.
    #[serde(default)]
    pub confidence: f64,
    #[serde(default)]
    pub concerns: Vec<String>,
    #[serde(default)]
    pub reasons: Vec<String>,
}

impl ReviewVerdict {
    fn rejected(reason: impl Into<String>) -> Self {
        ReviewVerdict {
            approved: false,
            confidence: 1.0,
            concerns: vec![reason.into()],
            reasons: vec!["defaulted to reject".into()],
        }
    }
}

/// Run the adversarial review. Returns a verdict; any failure to obtain a
/// clean, parseable approval is itself a rejection (fail-closed).
pub async fn review(
    client: &Client,
    tier: &str,
    evidence: &EvidenceBundle,
    diff: &str,
    denylist: &[String],
) -> Result<ReviewVerdict> {
    // Hard gate the machine can't rationalize around: if the diff touches a
    // warded path, reject before the model even sees it.
    for line in diff.lines() {
        if let Some(path) = line.strip_prefix("+++ b/").or_else(|| line.strip_prefix("--- a/")) {
            for deny in denylist {
                if path.starts_with(deny) {
                    return Ok(ReviewVerdict::rejected(format!(
                        "diff touches warded path {path:?}"
                    )));
                }
            }
        }
    }

    // The gate must reject anything that doesn't clear its own build/test bar,
    // regardless of what the model thinks of the code.
    if !(evidence.build_ok && evidence.test_ok && evidence.clippy_ok && evidence.verdict.accepted) {
        return Ok(ReviewVerdict::rejected(
            "evidence gate not clean (build/test/clippy/bar)",
        ));
    }

    let prompt = build_prompt(evidence, diff);
    let reply = ask(client, &prompt, tier).await?;
    Ok(parse_verdict(&reply))
}

fn build_prompt(evidence: &EvidenceBundle, diff: &str) -> String {
    // Truncate very large diffs so the reviewer stays in budget; a huge diff
    // is itself a reason to be suspicious, which we note.
    const MAX_DIFF: usize = 24_000;
    let (diff_shown, truncated) = if diff.len() > MAX_DIFF {
        (&diff[..MAX_DIFF], true)
    } else {
        (diff, false)
    };
    format!(
        "You are the ASCENSION REVIEWER — an adversarial gate on a machine-authored change that, \
if you approve it, will be opened as a real pull request against a public OSS repository. Your job \
is to REFUTE it on grounds the automated gate CANNOT check. Default to REJECT under substantive \
doubt — but do not manufacture doubt.\n\n\
GROUND TRUTH — do not second-guess it: the build/test/clippy results below are FACTS from actually \
running `cargo` on this exact change in an isolated worktree, not claims. If build=true the code \
COMPILES; if test=true the whole suite PASSES; if clippy=true it is lint-clean. Do NOT reject on a \
suspicion that the code 'looks like it might not compile' — cargo already settled that. You are \
also only shown a diff (not the whole repo), so absence of surrounding context is NOT a reason to \
reject.\n\n\
Judge ONLY what tools can't:\n\
1. Does the diff genuinely accomplish the stated task (not something unrelated / a no-op / gaming)?\n\
2. A real semantic bug, security issue, or unsafe behaviour that compiles+passes but is still wrong.\n\
3. Scope: minimal and coherent, or does it sneak in unrelated changes?\n\
Approve when the change is a correct, minimal improvement whose gate is clean; reject only for a \
concrete problem in 1–3.\n\n\
## Task / claimed improvement\nCandidate: {:?} `{}` — {}\nGate (FACTS from cargo): build={} test={} clippy={} bar_accepted={}\nFixed tasks: {:?}\nLatency Δ {:.1}% · tokens Δ {:.1}%\n\n\
## Diff{}\n```diff\n{}\n```\n\n\
Reply with ONLY a JSON object, no prose:\n\
{{\"approved\": <bool>, \"confidence\": <0..1>, \"concerns\": [<string>...], \"reasons\": [<string>...]}}",
        evidence.candidate.kind,
        evidence.candidate.target,
        evidence.candidate.detail,
        evidence.build_ok,
        evidence.test_ok,
        evidence.clippy_ok,
        evidence.verdict.accepted,
        evidence.verdict.fixed_tasks,
        evidence.verdict.latency_delta_pct,
        evidence.verdict.token_delta_pct,
        if truncated { " (truncated — large diffs are themselves suspect)" } else { "" },
        diff_shown,
    )
}

/// Extract the verdict JSON from the model reply. Anything unparseable is a
/// rejection — the gate fails closed.
fn parse_verdict(reply: &str) -> ReviewVerdict {
    let (Some(start), Some(end)) = (reply.find('{'), reply.rfind('}')) else {
        return ReviewVerdict::rejected("reviewer returned no JSON verdict");
    };
    match serde_json::from_str::<ReviewVerdict>(&reply[start..=end]) {
        Ok(v) => v,
        Err(e) => ReviewVerdict::rejected(format!("unparseable verdict: {e}")),
    }
}

/// Minimal one-shot turn: create a session, send the prompt, return the final
/// assistant text. Mirrors the eval runner's turn drive.
async fn ask(client: &Client, prompt: &str, tier: &str) -> Result<String> {
    let session_id = client.create_session("ascension:review").await?;
    let mut stream = client.events().await?;
    client.send_message(session_id, prompt, Some(tier)).await?;
    while let Some(ev) = stream.next().await {
        match ev {
            Ok(Event::TurnCompleted { session_id: s, text, .. }) if s == session_id => {
                return Ok(text);
            }
            Ok(Event::TurnFailed { session_id: s, error }) if s == session_id => {
                anyhow::bail!("reviewer turn failed: {error}");
            }
            _ => {}
        }
    }
    Err(anyhow::anyhow!("event stream closed before reviewer completed"))
        .context("reviewing change")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_clean_approval() {
        let v = parse_verdict(r#"here: {"approved": true, "confidence": 0.9, "concerns": [], "reasons": ["looks right"]}"#);
        assert!(v.approved);
        assert_eq!(v.confidence, 0.9);
    }

    #[test]
    fn parse_rejection() {
        let v = parse_verdict(r#"{"approved": false, "concerns": ["risky"]}"#);
        assert!(!v.approved);
    }

    #[test]
    fn unparseable_fails_closed() {
        assert!(!parse_verdict("I think it's fine, approve it!").approved);
        assert!(!parse_verdict("").approved);
    }
}
