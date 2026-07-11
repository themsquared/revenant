//! The offer: turn an approved change into a real pull request. Gated on the
//! reviewer verdict, rate-limited per day, and always labelled as
//! machine-authored. The agent can open a PR; it cannot merge one — branch
//! protection + a human final review are the backstops.

use crate::review::ReviewVerdict;
use crate::{EvidenceBundle, Worktree};
use anyhow::{bail, Context, Result};
use std::path::Path;
use std::process::Command;

/// Result of an offer attempt.
#[derive(Debug, Clone)]
pub enum Offered {
    /// A PR was opened (or would be, in dry-run) — carries the URL or the
    /// exact command line that would run.
    Opened(String),
    /// The reviewer rejected; no PR opened.
    Rejected(Vec<String>),
    /// Blocked by the daily rate limit.
    RateLimited { used: u32, cap: u32 },
}

/// Offer a proven, reviewed change as a PR.
///
/// `state_dir` holds the rate-limit ledger. `today` is passed in (the caller
/// stamps it) so this stays deterministic and testable. In `dry_run`, no git
/// or gh command runs — the `gh pr create` command line is returned instead.
#[allow(clippy::too_many_arguments)]
pub fn offer(
    wt: &Worktree,
    evidence: &EvidenceBundle,
    verdict: &ReviewVerdict,
    base_branch: &str,
    max_prs_per_day: u32,
    state_dir: &Path,
    today: &str,
    dry_run: bool,
) -> Result<Offered> {
    if !verdict.approved {
        return Ok(Offered::Rejected(verdict.concerns.clone()));
    }

    let ledger = state_dir.join("ascension-prs.log");
    let used = prs_today(&ledger, today);
    if used >= max_prs_per_day {
        return Ok(Offered::RateLimited { used, cap: max_prs_per_day });
    }

    let title = format!("ascension: {}", summarize(evidence));
    let body = format!(
        "{}\n\n---\n### Reviewer verdict\n- approved: {} (confidence {:.2})\n{}\n",
        evidence.markdown(),
        verdict.approved,
        verdict.confidence,
        verdict
            .reasons
            .iter()
            .chain(verdict.concerns.iter())
            .map(|r| format!("- {r}"))
            .collect::<Vec<_>>()
            .join("\n"),
    );

    if dry_run {
        let cmd = format!(
            "git push -u origin {branch}\n\
             gh pr create --base {base} --head {branch} --label ascension \\\n  --title {title:?} --body <evidence+verdict>",
            branch = wt.branch,
            base = base_branch,
        );
        return Ok(Offered::Opened(format!("[dry-run]\n{cmd}")));
    }

    // Commit whatever the actuator produced in the worktree.
    commit_all(wt, &title)?;
    let (ok, out) = wt.run("git", &["push", "-u", "origin", &wt.branch])?;
    if !ok {
        bail!("git push failed: {out}");
    }
    let url = gh_pr_create(wt, base_branch, &title, &body)?;
    record_pr(&ledger, today)?;
    Ok(Offered::Opened(url))
}

fn summarize(evidence: &EvidenceBundle) -> String {
    let c = &evidence.candidate;
    // A concrete eval-task fix gets named; an ad-hoc task summarizes from its
    // description (the placeholder "adhoc-fix" target is never a good title).
    if !evidence.verdict.fixed_tasks.is_empty() && c.target != "adhoc-fix" {
        return format!("fix {}", evidence.verdict.fixed_tasks.join(", "));
    }
    let detail = c.detail.trim();
    if !detail.is_empty() {
        let short: String = detail.split_whitespace().collect::<Vec<_>>().join(" ");
        return short.chars().take(72).collect::<String>().trim_end().to_string();
    }
    if evidence.verdict.latency_delta_pct >= 1.0 {
        return format!("speed up ({:.0}% faster)", evidence.verdict.latency_delta_pct);
    }
    "improvement".to_string()
}

fn commit_all(wt: &Worktree, title: &str) -> Result<()> {
    let (_ok, _) = wt.run("git", &["add", "-A"])?;
    // Configure a machine identity for the commit if the worktree lacks one.
    let _ = wt.run("git", &["config", "user.email", "ascension@revenant.local"]);
    let _ = wt.run("git", &["config", "user.name", "revenant-ascension"]);
    let (ok, out) = wt.run(
        "git",
        &["commit", "-m", title, "-m", "Machine-authored by the Ascension loop. Human final review."],
    )?;
    if !ok && !out.contains("nothing to commit") {
        bail!("git commit failed: {out}");
    }
    Ok(())
}

fn gh_pr_create(wt: &Worktree, base: &str, title: &str, body: &str) -> Result<String> {
    // Ensure the provenance label exists (idempotent) so every machine-authored
    // PR is filterable and the gatekeeper (`revenant pr-review`) can find it.
    let _ = Command::new("gh")
        .current_dir(&wt.path)
        .args(["label", "create", "ascension", "--color", "5319e7"])
        .args(["--description", "machine-authored by the Ascension loop", "--force"])
        .output();
    let out = Command::new("gh")
        .current_dir(&wt.path)
        .args(["pr", "create", "--base", base, "--head", &wt.branch])
        .args(["--label", "ascension", "--title", title, "--body", body])
        .output()
        .context("running gh pr create")?;
    if !out.status.success() {
        // Retry once without the label — a missing label shouldn't sink a
        // well-formed PR.
        let stderr = String::from_utf8_lossy(&out.stderr);
        if stderr.contains("label") {
            let out2 = Command::new("gh")
                .current_dir(&wt.path)
                .args(["pr", "create", "--base", base, "--head", &wt.branch])
                .args(["--title", title, "--body", body])
                .output()
                .context("running gh pr create (no label)")?;
            if out2.status.success() {
                return Ok(String::from_utf8_lossy(&out2.stdout).trim().to_string());
            }
            bail!("gh pr create failed: {}", String::from_utf8_lossy(&out2.stderr));
        }
        bail!("gh pr create failed: {stderr}");
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn prs_today(ledger: &Path, today: &str) -> u32 {
    std::fs::read_to_string(ledger)
        .map(|s| s.lines().filter(|l| l.trim() == today).count() as u32)
        .unwrap_or(0)
}

fn record_pr(ledger: &Path, today: &str) -> Result<()> {
    use std::io::Write;
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(ledger)
        .context("opening PR ledger")?;
    writeln!(f, "{today}").context("writing PR ledger")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Candidate, CandidateKind, Verdict};

    fn evidence() -> EvidenceBundle {
        EvidenceBundle {
            candidate: Candidate {
                kind: CandidateKind::FailingTask,
                target: "t".into(),
                detail: "d".into(),
                priority: 100.0,
            },
            verdict: Verdict {
                accepted: true,
                fixed_tasks: vec!["t".into()],
                regressions: vec![],
                reasons: vec![],
                latency_delta_pct: 0.0,
                token_delta_pct: 0.0,
            },
            changed_files: vec!["crates/revenant-agent/src/lib.rs".into()],
            build_ok: true,
            test_ok: true,
            clippy_ok: true,
        }
    }

    #[test]
    fn rejected_verdict_opens_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let wt = crate::Worktree {
            repo: dir.path().to_path_buf(),
            path: dir.path().to_path_buf(),
            branch: "self-improve/x".into(),
            base: "main".into(),
        };
        let v = ReviewVerdict { approved: false, confidence: 1.0, concerns: vec!["no".into()], reasons: vec![] };
        let r = offer(&wt, &evidence(), &v, "main", 3, dir.path(), "2026-07-11", true).unwrap();
        assert!(matches!(r, Offered::Rejected(_)));
    }

    #[test]
    fn rate_limit_blocks_after_cap() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("ascension-prs.log"), "2026-07-11\n2026-07-11\n").unwrap();
        let wt = crate::Worktree {
            repo: dir.path().to_path_buf(),
            path: dir.path().to_path_buf(),
            branch: "self-improve/x".into(),
            base: "main".into(),
        };
        let v = ReviewVerdict { approved: true, confidence: 1.0, concerns: vec![], reasons: vec![] };
        let r = offer(&wt, &evidence(), &v, "main", 2, dir.path(), "2026-07-11", true).unwrap();
        assert!(matches!(r, Offered::RateLimited { used: 2, cap: 2 }));
    }

    #[test]
    fn dry_run_emits_command_not_action() {
        let dir = tempfile::tempdir().unwrap();
        let wt = crate::Worktree {
            repo: dir.path().to_path_buf(),
            path: dir.path().to_path_buf(),
            branch: "self-improve/fix".into(),
            base: "main".into(),
        };
        let v = ReviewVerdict { approved: true, confidence: 0.95, concerns: vec![], reasons: vec!["ok".into()] };
        let r = offer(&wt, &evidence(), &v, "main", 3, dir.path(), "2026-07-11", true).unwrap();
        match r {
            Offered::Opened(s) => {
                assert!(s.contains("[dry-run]"));
                assert!(s.contains("gh pr create --base main --head self-improve/fix"));
            }
            other => panic!("expected dry-run Opened, got {other:?}"),
        }
        // Nothing was recorded in the ledger on a dry run.
        assert_eq!(prs_today(&dir.path().join("ascension-prs.log"), "2026-07-11"), 0);
    }
}
