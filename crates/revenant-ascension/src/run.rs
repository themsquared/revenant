//! The actuator: takes one candidate through the whole rite end to end —
//! isolate → implement (repair-looped) → prove → attest → review → offer.
//! The coding turns run on the daemon (via the client's `/code` endpoint,
//! jailed to the worktree); builds/tests run here; the reviewer gate and offer
//! are the same ones the rest of the crate defines. Nothing merges — a proven,
//! reviewed change becomes a PR for a human's final call.

use crate::{
    check_denylist, offer, review, Candidate, CandidateKind, EvidenceBundle, Verdict, Worktree,
};
use anyhow::{Context, Result};
use revenant_client::Client;
use std::path::Path;

pub struct RunConfig {
    pub coder_tier: String,
    pub reviewer_tier: String,
    pub base_branch: String,
    pub staging_prefix: String,
    pub max_prs_per_day: u32,
    pub denylist: Vec<String>,
    /// Max edit→build repair passes before giving up on a candidate.
    pub max_repair: usize,
    /// false → offer is a dry-run (no push / no PR).
    pub live: bool,
}

#[derive(Debug)]
pub struct RunOutcome {
    pub candidate: Candidate,
    pub build_ok: bool,
    pub test_ok: bool,
    pub clippy_ok: bool,
    pub changed_files: Vec<String>,
    pub reviewer_approved: Option<bool>,
    pub offer: Option<String>,
    pub notes: Vec<String>,
}

/// Drive one candidate. Returns an outcome no matter where it stops — a
/// candidate that won't compile, trips a ward, fails the gate, or is rejected
/// by the reviewer simply doesn't reach the offer.
pub async fn run_candidate(
    client: &Client,
    repo: &Path,
    candidate: Candidate,
    cfg: &RunConfig,
    state_dir: &Path,
    today: &str,
    task_override: Option<&str>,
) -> Result<RunOutcome> {
    let mut notes = Vec::new();
    let slug = slugify(&candidate.target);
    let branch = format!("{}{}", cfg.staging_prefix, slug);
    let base = current_branch(repo)?;
    let wt_root = repo.join(".ascension-worktrees").join(&slug);
    let _ = std::fs::remove_dir_all(&wt_root);
    if let Some(parent) = wt_root.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    // Clean any stale worktree registration for this branch/path.
    let _ = std::process::Command::new("git")
        .args(["-C", &repo.to_string_lossy(), "worktree", "prune"])
        .output();
    let _ = std::process::Command::new("git")
        .args(["-C", &repo.to_string_lossy(), "branch", "-D", &branch])
        .output();

    let wt = Worktree::create(repo, &base, &branch, &wt_root).context("creating worktree")?;
    let root = wt.path.to_string_lossy().to_string();

    // Implement, with a repair loop: feed compile errors back to the agent.
    // An explicit override lets a caller point the actuator at any task, not
    // just an eval-derived candidate.
    let base_task = task_override.map(String::from).unwrap_or_else(|| task_for(&candidate));
    let mut task = base_task.clone();
    let mut build_ok = false;
    let mut last_build = String::new();
    for pass in 0..cfg.max_repair.max(1) {
        client
            .code(&root, &task, Some(&cfg.coder_tier))
            .await
            .context("actuator coding turn")?;
        let (ok, out) = cargo(&wt, &["check", "--workspace", "--message-format=short"]);
        last_build = out;
        if ok {
            build_ok = true;
            notes.push(format!("compiles after {} edit pass(es)", pass + 1));
            break;
        }
        task = format!(
            "{}\n\nYour previous edit does NOT compile. Fix exactly these errors, minimally:\n{}",
            base_task,
            tail(&last_build, 3000)
        );
    }

    let changed_files = wt.changed_files().unwrap_or_default();

    if !build_ok {
        notes.push(format!(
            "did not compile within the repair budget — discarded. last build output:\n{}",
            tail(&last_build, 1200)
        ));
        return Ok(stop(candidate, build_ok, false, false, changed_files, notes));
    }

    // Ward: never let the actuator touch a protected path.
    if let Err(e) = check_denylist(&changed_files, &cfg.denylist) {
        notes.push(format!("REJECT (ward): {e}"));
        return Ok(stop(candidate, build_ok, false, false, changed_files, notes));
    }

    // Prove: the deterministic bar. (Behavioural eval-in-worktree is a later
    // refinement; build+test+clippy already blocks regressions hard.)
    let (test_ok, _) = cargo(&wt, &["test", "--workspace", "--quiet"]);
    let (clippy_ok, _) = cargo(&wt, &["clippy", "--workspace", "--", "-D", "warnings"]);

    let evidence = EvidenceBundle {
        candidate: candidate.clone(),
        verdict: Verdict {
            accepted: build_ok && test_ok && clippy_ok,
            fixed_tasks: match candidate.kind {
                CandidateKind::FailingTask => vec![candidate.target.clone()],
                _ => vec![],
            },
            regressions: vec![],
            reasons: vec![],
            latency_delta_pct: 0.0,
            token_delta_pct: 0.0,
        },
        changed_files: changed_files.clone(),
        build_ok,
        test_ok,
        clippy_ok,
    };

    if !evidence.verdict.accepted {
        notes.push(format!(
            "gate not clean (build={build_ok} test={test_ok} clippy={clippy_ok}) — not offered"
        ));
        return Ok(stop(candidate, build_ok, test_ok, clippy_ok, changed_files, notes));
    }

    // Review: the adversarial gate sees the real diff.
    let diff = git_diff(&wt);
    let verdict = review::review(client, &cfg.reviewer_tier, &evidence, &diff, &cfg.denylist).await?;
    let approved = verdict.approved;

    // Offer: gated on the verdict; dry-run unless live.
    let offered = offer::offer(
        &wt,
        &evidence,
        &verdict,
        &cfg.base_branch,
        cfg.max_prs_per_day,
        state_dir,
        today,
        !cfg.live,
    )?;
    let offer_str = format!("{offered:?}");

    Ok(RunOutcome {
        candidate,
        build_ok,
        test_ok,
        clippy_ok,
        changed_files,
        reviewer_approved: Some(approved),
        offer: Some(offer_str),
        notes,
    })
}

fn stop(
    candidate: Candidate,
    build_ok: bool,
    test_ok: bool,
    clippy_ok: bool,
    changed_files: Vec<String>,
    notes: Vec<String>,
) -> RunOutcome {
    RunOutcome {
        candidate,
        build_ok,
        test_ok,
        clippy_ok,
        changed_files,
        reviewer_approved: None,
        offer: None,
        notes,
    }
}

fn task_for(c: &Candidate) -> String {
    match c.kind {
        CandidateKind::FailingTask => format!(
            "The behaviour behind eval task '{}' is wrong: {}. Find the responsible code in this \
workspace and make the smallest change that fixes it without breaking anything else.",
            c.target, c.detail
        ),
        CandidateKind::LatencyOutlier => format!(
            "Eval task '{}' is unusually slow ({}). Find a safe, correctness-preserving optimization \
in the relevant code path.",
            c.target, c.detail
        ),
        CandidateKind::TokenOutlier => format!(
            "Eval task '{}' uses unusually many tokens ({}). Reduce prompt/context waste on that \
path without changing behaviour.",
            c.target, c.detail
        ),
    }
}

/// Resolve the cargo binary. `cargo` is often NOT on the daemon/CLI PATH
/// (rustup installs it at ~/.cargo/bin), so fall back to that explicitly —
/// otherwise the prove step fails to spawn and looks like a compile failure.
fn cargo_bin() -> String {
    if let Ok(c) = std::env::var("CARGO") {
        return c;
    }
    if let Some(home) = std::env::var_os("HOME") {
        let p = std::path::Path::new(&home).join(".cargo/bin/cargo");
        if p.exists() {
            return p.to_string_lossy().into_owned();
        }
    }
    "cargo".to_string()
}

fn cargo(wt: &Worktree, args: &[&str]) -> (bool, String) {
    let bin = cargo_bin();
    let mut a = vec!["--offline"];
    a.extend_from_slice(args);
    // Fall back to online if offline can't resolve (first run in a worktree).
    match wt.run(&bin, &a) {
        Ok((true, out)) => (true, out),
        _ => wt.run(&bin, args).unwrap_or((false, "cargo failed to spawn".into())),
    }
}

fn git_diff(wt: &Worktree) -> String {
    let _ = wt.run("git", &["add", "-A"]);
    wt.run("git", &["diff", "--cached"]).map(|(_, o)| o).unwrap_or_default()
}

fn current_branch(repo: &Path) -> Result<String> {
    let out = std::process::Command::new("git")
        .args(["-C", &repo.to_string_lossy(), "branch", "--show-current"])
        .output()
        .context("git branch --show-current")?;
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn slugify(s: &str) -> String {
    let slug: String = s
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c.to_ascii_lowercase() } else { '-' })
        .collect();
    slug.trim_matches('-').to_string()
}

fn tail(s: &str, n: usize) -> String {
    if s.len() <= n {
        s.to_string()
    } else {
        s[s.len() - n..].to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slugify_makes_branch_safe() {
        assert_eq!(slugify("tool-memory-save"), "tool-memory-save");
        assert_eq!(slugify("Weird Task/Name!"), "weird-task-name");
    }

    #[test]
    fn tail_bounds_output() {
        assert_eq!(tail("abcdef", 3), "def");
        assert_eq!(tail("ab", 5), "ab");
    }
}
