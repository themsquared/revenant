//! revenant-ascension: the rite by which a revenant betters itself and offers
//! the improvement back to the horde — under a bar high enough that "better"
//! is a measurement, not an opinion.
//!
//! The loop: **observe** its own eval scorecard for candidates → **isolate**
//! the work in an ephemeral git worktree → **implement** a bounded change →
//! **prove** it (build + test + clippy clean, and the eval bar met across N
//! runs with zero regressions) → **attest** with an evidence bundle → **offer**
//! a PR into a staging namespace for a human to promote. It can open a PR; it
//! can never merge one (branch protection is the backstop).
//!
//! This crate is the provable machinery. The step that actually writes the
//! change is an agent turn driven by the daemon; everything here — candidate
//! detection, the bar, the denylist ward, the worktree, the offer — is
//! deterministic and tested.

use anyhow::{bail, Context, Result};
use revenant_evals::Report;
use serde::Serialize;
use std::path::{Path, PathBuf};
use std::process::Command;

pub mod config;
pub mod materiality;
pub mod offer;
pub mod review;
pub mod run;
pub use config::AscensionConfig;

// --- observe -------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Serialize)]
pub enum CandidateKind {
    /// A task the harness currently fails — the clearest improvement target.
    FailingTask,
    /// A task far slower than its peers.
    LatencyOutlier,
    /// A task burning far more tokens than its peers.
    TokenOutlier,
}

#[derive(Debug, Clone, Serialize)]
pub struct Candidate {
    pub kind: CandidateKind,
    /// The eval task id this candidate is about.
    pub target: String,
    pub detail: String,
    /// Higher = worked on first.
    pub priority: f64,
}

/// Read a scorecard and propose what to improve, most-promising first. A
/// failing task always outranks a mere outlier — correctness before polish.
pub fn detect(report: &Report) -> Vec<Candidate> {
    let mut out = Vec::new();
    for o in &report.outcomes {
        if !o.passed {
            out.push(Candidate {
                kind: CandidateKind::FailingTask,
                target: o.id.clone(),
                detail: o.reason.clone(),
                priority: 100.0,
            });
        }
    }
    // Outliers are judged against the median of passing tasks so a single
    // slow failure doesn't skew the baseline.
    let lat: Vec<u128> =
        report.outcomes.iter().filter(|o| o.passed).map(|o| o.result.latency_ms).collect();
    let tok: Vec<u64> = report
        .outcomes
        .iter()
        .filter(|o| o.passed)
        .map(|o| o.result.input_tokens + o.result.output_tokens)
        .collect();
    let lat_med = median_u128(&lat);
    let tok_med = median_u64(&tok);
    for o in report.outcomes.iter().filter(|o| o.passed) {
        if lat_med > 0 && o.result.latency_ms as f64 > 2.0 * lat_med as f64 {
            out.push(Candidate {
                kind: CandidateKind::LatencyOutlier,
                target: o.id.clone(),
                detail: format!("{} ms vs median {} ms", o.result.latency_ms, lat_med),
                priority: 50.0,
            });
        }
        let t = o.result.input_tokens + o.result.output_tokens;
        if tok_med > 0 && t as f64 > 2.0 * tok_med as f64 {
            out.push(Candidate {
                kind: CandidateKind::TokenOutlier,
                target: o.id.clone(),
                detail: format!("{t} tok vs median {tok_med} tok"),
                priority: 40.0,
            });
        }
    }
    out.sort_by(|a, b| b.priority.total_cmp(&a.priority));
    out
}

// --- prove (the bar) -----------------------------------------------------

/// The acceptance bar. Deliberately strict: a change earns a PR only by
/// fixing a previously-failing task (holding across every proof run) or by a
/// repeatable metric win, and only if it breaks nothing that worked before.
#[derive(Debug, Clone)]
pub struct Bar {
    pub proof_runs: usize,
    /// Minimum mean improvement for the metric path, as a percentage.
    pub min_gain_pct: f64,
}

#[derive(Debug, Clone, Serialize)]
pub struct Verdict {
    pub accepted: bool,
    pub fixed_tasks: Vec<String>,
    pub regressions: Vec<String>,
    pub reasons: Vec<String>,
    pub latency_delta_pct: f64,
    pub token_delta_pct: f64,
}

fn passed_set(r: &Report) -> std::collections::BTreeSet<String> {
    r.outcomes.iter().filter(|o| o.passed).map(|o| o.id.clone()).collect()
}

fn mean_latency(r: &Report) -> f64 {
    if r.outcomes.is_empty() {
        return 0.0;
    }
    r.outcomes.iter().map(|o| o.result.latency_ms as f64).sum::<f64>() / r.outcomes.len() as f64
}

fn total_tokens(r: &Report) -> f64 {
    r.outcomes.iter().map(|o| (o.result.input_tokens + o.result.output_tokens) as f64).sum()
}

/// Judge `after` (one report per proof run) against the `before` baseline.
///
/// Accept iff **zero regressions** across every run, AND either:
/// - a task that failed in `before` now passes in **all** runs (a fix), or
/// - the mean metric improved by ≥ `min_gain_pct` while **no single run**
///   regressed the metric past the baseline (a repeatable win).
pub fn evaluate(bar: &Bar, before: &Report, after: &[Report]) -> Verdict {
    let mut reasons = Vec::new();
    if after.len() < bar.proof_runs {
        return Verdict {
            accepted: false,
            fixed_tasks: vec![],
            regressions: vec![],
            reasons: vec![format!(
                "need {} proof runs, got {}",
                bar.proof_runs,
                after.len()
            )],
            latency_delta_pct: 0.0,
            token_delta_pct: 0.0,
        };
    }
    let before_pass = passed_set(before);

    // Regression = anything that passed before and fails in ANY run.
    let mut regressions: Vec<String> = Vec::new();
    for run in after {
        let p = passed_set(run);
        for id in before_pass.difference(&p) {
            if !regressions.contains(id) {
                regressions.push(id.clone());
            }
        }
    }
    regressions.sort();

    // Fix = failed before, passes in EVERY run.
    let fixed_tasks: Vec<String> = after
        .iter()
        .map(passed_set)
        .fold(None::<std::collections::BTreeSet<String>>, |acc, p| match acc {
            None => Some(p),
            Some(a) => Some(a.intersection(&p).cloned().collect()),
        })
        .unwrap_or_default()
        .into_iter()
        .filter(|id| !before_pass.contains(id))
        .collect();

    // Metric deltas (negative = better; we report improvement as positive %).
    let base_lat = mean_latency(before).max(1.0);
    let mean_after_lat =
        after.iter().map(mean_latency).sum::<f64>() / after.len() as f64;
    let latency_delta_pct = (base_lat - mean_after_lat) / base_lat * 100.0;
    let base_tok = total_tokens(before).max(1.0);
    let mean_after_tok = after.iter().map(total_tokens).sum::<f64>() / after.len() as f64;
    let token_delta_pct = (base_tok - mean_after_tok) / base_tok * 100.0;

    // Repeatable metric win: mean beats the bar AND every single run beats
    // the baseline (no run regressed the metric).
    let every_run_faster = after.iter().all(|r| mean_latency(r) <= base_lat);
    let metric_win =
        latency_delta_pct >= bar.min_gain_pct && every_run_faster && regressions.is_empty();

    if !regressions.is_empty() {
        reasons.push(format!("REJECT: regressions in {regressions:?}"));
    }
    if !fixed_tasks.is_empty() {
        reasons.push(format!("fixed previously-failing task(s): {fixed_tasks:?}"));
    }
    if metric_win {
        reasons.push(format!(
            "repeatable latency win: {latency_delta_pct:.1}% (>= {:.1}%)",
            bar.min_gain_pct
        ));
    }
    let accepted = regressions.is_empty() && (!fixed_tasks.is_empty() || metric_win);
    if !accepted && regressions.is_empty() {
        reasons.push("REJECT: no task fixed and no repeatable metric win".into());
    }

    Verdict { accepted, fixed_tasks, regressions, reasons, latency_delta_pct, token_delta_pct }
}

// --- the ward: denylist --------------------------------------------------

/// Paths the self-improver may never touch without a human. The wards guard
/// themselves: security, gateway key handling, the approval broker, and the
/// sandbox are off-limits to autonomous change.
pub fn check_denylist(changed: &[String], denylist: &[String]) -> Result<()> {
    for file in changed {
        for deny in denylist {
            if file.starts_with(deny) {
                bail!("change touches warded path {file:?} (matches denylist {deny:?})");
            }
        }
    }
    Ok(())
}

// --- attest --------------------------------------------------------------

#[derive(Debug, Clone, Serialize)]
pub struct EvidenceBundle {
    pub candidate: Candidate,
    pub verdict: Verdict,
    pub changed_files: Vec<String>,
    pub build_ok: bool,
    pub test_ok: bool,
    pub clippy_ok: bool,
}

impl EvidenceBundle {
    /// The PR body — the proof travels with the proposal.
    pub fn markdown(&self) -> String {
        let mut s = String::new();
        s.push_str("## 🜁 Ascension: machine-authored, eval-proven improvement\n\n");
        s.push_str(&format!(
            "**Candidate** ({:?}): `{}` — {}\n\n",
            self.candidate.kind, self.candidate.target, self.candidate.detail
        ));
        s.push_str("### Gate\n\n");
        s.push_str(&format!(
            "- build: {} · tests: {} · clippy: {}\n",
            yn(self.build_ok),
            yn(self.test_ok),
            yn(self.clippy_ok)
        ));
        if !self.verdict.fixed_tasks.is_empty() {
            s.push_str(&format!("- fixed tasks: {:?}\n", self.verdict.fixed_tasks));
        }
        s.push_str(&format!(
            "- latency Δ {:.1}% · tokens Δ {:.1}% (positive = better)\n",
            self.verdict.latency_delta_pct, self.verdict.token_delta_pct
        ));
        if !self.verdict.regressions.is_empty() {
            s.push_str(&format!("- ⚠️ regressions: {:?}\n", self.verdict.regressions));
        }
        for r in &self.verdict.reasons {
            s.push_str(&format!("- {r}\n"));
        }
        s.push_str("\n### Changed files\n\n");
        for f in &self.changed_files {
            s.push_str(&format!("- `{f}`\n"));
        }
        s.push_str("\n> Opened by the Ascension loop into a staging namespace. A human promotes it. The agent cannot merge.\n");
        s
    }
}

fn yn(b: bool) -> &'static str {
    if b {
        "✅"
    } else {
        "❌"
    }
}

// --- isolate: ephemeral git worktree ------------------------------------

/// An ephemeral git worktree — a private copy of the repo the improver edits
/// so the live tree is never touched. Removed on drop.
pub struct Worktree {
    repo: PathBuf,
    pub path: PathBuf,
    pub branch: String,
    pub base: String,
}

impl Worktree {
    pub fn create(repo: &Path, base: &str, branch: &str, at: &Path) -> Result<Self> {
        let out = Command::new("git")
            .args(["-C", &repo.to_string_lossy()])
            .args(["worktree", "add", "-b", branch, &at.to_string_lossy(), base])
            .output()
            .context("git worktree add")?;
        if !out.status.success() {
            bail!("git worktree add failed: {}", String::from_utf8_lossy(&out.stderr));
        }
        Ok(Worktree {
            repo: repo.to_path_buf(),
            path: at.to_path_buf(),
            branch: branch.to_string(),
            base: base.to_string(),
        })
    }

    /// Run a command inside the worktree; returns (success, combined output).
    pub fn run(&self, program: &str, args: &[&str]) -> Result<(bool, String)> {
        let out = Command::new(program)
            .current_dir(&self.path)
            .args(args)
            .output()
            .with_context(|| format!("running {program} in worktree"))?;
        let mut s = String::from_utf8_lossy(&out.stdout).into_owned();
        s.push_str(&String::from_utf8_lossy(&out.stderr));
        Ok((out.status.success(), s))
    }

    /// Files changed vs the base ref (staged + unstaged + untracked).
    pub fn changed_files(&self) -> Result<Vec<String>> {
        let (_ok, out) = self.run("git", &["add", "-A"]).unwrap_or((false, String::new()));
        let _ = out;
        let out = Command::new("git")
            .current_dir(&self.path)
            .args(["diff", "--cached", "--name-only"])
            .output()
            .context("git diff --name-only")?;
        Ok(String::from_utf8_lossy(&out.stdout)
            .lines()
            .map(|l| l.trim().to_string())
            .filter(|l| !l.is_empty())
            .collect())
    }
}

impl Drop for Worktree {
    fn drop(&mut self) {
        // Best-effort cleanup; a leaked worktree is recoverable with
        // `git worktree prune`, so never panic here.
        let _ = Command::new("git")
            .args(["-C", &self.repo.to_string_lossy()])
            .args(["worktree", "remove", "--force", &self.path.to_string_lossy()])
            .output();
    }
}

fn median_u128(v: &[u128]) -> u128 {
    if v.is_empty() {
        return 0;
    }
    let mut s = v.to_vec();
    s.sort_unstable();
    s[s.len() / 2]
}
fn median_u64(v: &[u64]) -> u64 {
    if v.is_empty() {
        return 0;
    }
    let mut s = v.to_vec();
    s.sort_unstable();
    s[s.len() / 2]
}

#[cfg(test)]
mod tests {
    use super::*;
    use revenant_evals::{TaskOutcome, TurnResult};

    fn outcome(id: &str, passed: bool, lat: u128, tok: u64) -> TaskOutcome {
        TaskOutcome {
            id: id.into(),
            tags: vec![],
            passed,
            reason: if passed { "ok".into() } else { "boom".into() },
            result: TurnResult {
                final_text: String::new(),
                input_tokens: tok,
                output_tokens: 0,
                tools: vec![],
                latency_ms: lat,
                model: None,
                failed: None,
            },
        }
    }

    fn report(outcomes: Vec<TaskOutcome>) -> Report {
        Report { outcomes }
    }

    #[test]
    fn detect_prioritizes_failures_over_outliers() {
        let r = report(vec![
            outcome("fail", false, 100, 10),
            outcome("ok1", true, 100, 10),
            outcome("ok2", true, 100, 10),
            outcome("slow", true, 500, 10), // 5x median latency
        ]);
        let c = detect(&r);
        assert_eq!(c[0].kind, CandidateKind::FailingTask);
        assert!(c.iter().any(|x| x.kind == CandidateKind::LatencyOutlier && x.target == "slow"));
    }

    #[test]
    fn bar_accepts_a_fix_with_no_regressions() {
        let bar = Bar { proof_runs: 3, min_gain_pct: 5.0 };
        let before = report(vec![outcome("a", false, 100, 10), outcome("b", true, 100, 10)]);
        let after: Vec<_> = (0..3)
            .map(|_| report(vec![outcome("a", true, 100, 10), outcome("b", true, 100, 10)]))
            .collect();
        let v = evaluate(&bar, &before, &after);
        assert!(v.accepted);
        assert_eq!(v.fixed_tasks, vec!["a"]);
        assert!(v.regressions.is_empty());
    }

    #[test]
    fn bar_rejects_any_regression_even_with_a_fix() {
        let bar = Bar { proof_runs: 2, min_gain_pct: 5.0 };
        let before = report(vec![outcome("a", false, 100, 10), outcome("b", true, 100, 10)]);
        // Fixes "a" but breaks "b" in one run — must reject.
        let after = vec![
            report(vec![outcome("a", true, 100, 10), outcome("b", true, 100, 10)]),
            report(vec![outcome("a", true, 100, 10), outcome("b", false, 100, 10)]),
        ];
        let v = evaluate(&bar, &before, &after);
        assert!(!v.accepted);
        assert_eq!(v.regressions, vec!["b"]);
    }

    #[test]
    fn bar_rejects_no_change() {
        let bar = Bar { proof_runs: 2, min_gain_pct: 5.0 };
        let before = report(vec![outcome("a", true, 100, 10)]);
        let after = vec![
            report(vec![outcome("a", true, 100, 10)]),
            report(vec![outcome("a", true, 100, 10)]),
        ];
        let v = evaluate(&bar, &before, &after);
        assert!(!v.accepted, "identical scorecards must not earn a PR");
    }

    #[test]
    fn bar_accepts_repeatable_latency_win() {
        let bar = Bar { proof_runs: 2, min_gain_pct: 10.0 };
        let before = report(vec![outcome("a", true, 1000, 10)]);
        let after = vec![
            report(vec![outcome("a", true, 800, 10)]),
            report(vec![outcome("a", true, 850, 10)]),
        ];
        let v = evaluate(&bar, &before, &after);
        assert!(v.accepted);
        assert!(v.latency_delta_pct >= 10.0);
    }

    #[test]
    fn bar_rejects_metric_win_if_one_run_regresses() {
        let bar = Bar { proof_runs: 2, min_gain_pct: 10.0 };
        let before = report(vec![outcome("a", true, 1000, 10)]);
        // Mean is better, but one run is slower than baseline → not repeatable.
        let after = vec![
            report(vec![outcome("a", true, 500, 10)]),
            report(vec![outcome("a", true, 1200, 10)]),
        ];
        let v = evaluate(&bar, &before, &after);
        assert!(!v.accepted);
    }

    #[test]
    fn denylist_blocks_warded_paths() {
        let deny = vec!["crates/revenant-security".to_string()];
        assert!(check_denylist(&["crates/revenant-agent/src/lib.rs".into()], &deny).is_ok());
        assert!(check_denylist(&["crates/revenant-security/src/lib.rs".into()], &deny).is_err());
    }

    #[test]
    fn worktree_isolates_and_cleans_up() {
        // Build a throwaway git repo, take a worktree, confirm isolation.
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path();
        let git = |args: &[&str]| {
            Command::new("git").current_dir(repo).args(args).output().unwrap()
        };
        git(&["init", "-q"]);
        git(&["config", "user.email", "t@t"]);
        git(&["config", "user.name", "t"]);
        std::fs::write(repo.join("f.txt"), "base").unwrap();
        git(&["add", "-A"]);
        git(&["commit", "-qm", "base"]);
        let base = String::from_utf8_lossy(&git(&["branch", "--show-current"]).stdout)
            .trim()
            .to_string();

        let wt_path = dir.path().join("wt");
        let wt = Worktree::create(repo, &base, "self-improve/test", &wt_path).unwrap();
        std::fs::write(wt.path.join("f.txt"), "changed").unwrap();
        std::fs::write(wt.path.join("new.txt"), "new").unwrap();
        let changed = wt.changed_files().unwrap();
        assert!(changed.contains(&"f.txt".to_string()));
        assert!(changed.contains(&"new.txt".to_string()));
        // Live tree untouched.
        assert_eq!(std::fs::read_to_string(repo.join("f.txt")).unwrap(), "base");
        drop(wt);
    }

    #[test]
    fn evidence_bundle_renders() {
        let bundle = EvidenceBundle {
            candidate: Candidate {
                kind: CandidateKind::FailingTask,
                target: "arithmetic".into(),
                detail: "missing '391'".into(),
                priority: 100.0,
            },
            verdict: Verdict {
                accepted: true,
                fixed_tasks: vec!["arithmetic".into()],
                regressions: vec![],
                reasons: vec!["fixed previously-failing task".into()],
                latency_delta_pct: 2.0,
                token_delta_pct: 1.0,
            },
            changed_files: vec!["crates/revenant-agent/src/lib.rs".into()],
            build_ok: true,
            test_ok: true,
            clippy_ok: true,
        };
        let md = bundle.markdown();
        assert!(md.contains("Ascension"));
        assert!(md.contains("arithmetic"));
        assert!(md.contains("cannot merge"));
    }
}

