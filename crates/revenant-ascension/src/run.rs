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
    /// Apply the materiality judge before offering: only auto-PR changes that
    /// are generalizable + material for the horde. When false, any
    /// reviewer-approved change is offered (owner-specific tweaks included).
    pub materiality: bool,
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

    // Materiality gate (Gate 1.5): even a reviewer-approved change is only
    // auto-PR'd if it's worth the HORDE's attention — generalizable + material.
    // Owner-specific-but-proven changes are kept local (not offered). Only
    // runs when the change would otherwise be offered.
    if approved && cfg.materiality {
        match crate::materiality::judge_materiality(client, &cfg.reviewer_tier, &evidence, &diff).await {
            Ok(m) if !m.horde_worthy => {
                notes.push(format!(
                    "kept local (not horde-material): {} [axis={}]",
                    m.reasons.first().map(String::as_str).unwrap_or("owner-specific or marginal"),
                    m.axis,
                ));
                return Ok(RunOutcome {
                    candidate,
                    build_ok,
                    test_ok,
                    clippy_ok,
                    changed_files,
                    reviewer_approved: Some(approved),
                    offer: Some("kept-local (not horde-material)".to_string()),
                    notes,
                });
            }
            Ok(m) => notes.push(format!("horde-material [axis={}] — offering", m.axis)),
            Err(e) => notes.push(format!("materiality judge errored ({e:#}) — proceeding to offer")),
        }
    }

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

/// Resolve the cargo binary. `cargo` is often NOT on the daemon/CLI PATH.
/// Two rustup layouts are common:
///   - a shim at `~/.cargo/bin/cargo` (most installs), or
///   - no shim at all, with the real binary living under
///     `~/.rustup/toolchains/<host-triple>/bin/cargo` (seen on this box).
///
/// So we check both, plus `RUSTUP_HOME`/`CARGO_HOME` overrides, before
/// falling back to bare `cargo` on PATH. Getting this wrong makes the prove
/// step fail to spawn and look like a compile failure.
fn cargo_bin() -> String {
    let cargo_env = std::env::var("CARGO").ok();
    let cargo_home = std::env::var_os("CARGO_HOME")
        .map(std::path::PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| std::path::Path::new(&h).join(".cargo")));
    let rustup_home = std::env::var_os("RUSTUP_HOME")
        .map(std::path::PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| std::path::Path::new(&h).join(".rustup")));
    resolve_cargo_bin(cargo_env, cargo_home, rustup_home)
}

/// Pure resolution logic, factored out so it can be unit-tested without
/// mutating process-global env vars (which races under parallel tests).
/// Priority: `$CARGO` override > `<cargo_home>/bin/cargo` (the common rustup
/// shim) > the first `<rustup_home>/toolchains/*/bin/cargo` found (the
/// no-shim layout seen on some boxes) > bare `cargo` on PATH.
fn resolve_cargo_bin(
    cargo_env: Option<String>,
    cargo_home: Option<std::path::PathBuf>,
    rustup_home: Option<std::path::PathBuf>,
) -> String {
    if let Some(c) = cargo_env {
        return c;
    }
    if let Some(home) = &cargo_home {
        let p = home.join("bin/cargo");
        if p.exists() {
            return p.to_string_lossy().into_owned();
        }
    }
    if let Some(home) = &rustup_home {
        let toolchains = home.join("toolchains");
        if let Ok(entries) = std::fs::read_dir(&toolchains) {
            for entry in entries.flatten() {
                let p = entry.path().join("bin/cargo");
                if p.exists() {
                    return p.to_string_lossy().into_owned();
                }
            }
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

    #[test]
    fn cargo_bin_prefers_env_override() {
        let got = resolve_cargo_bin(
            Some("/explicit/cargo".to_string()),
            Some(std::path::PathBuf::from("/whatever")),
            Some(std::path::PathBuf::from("/whatever")),
        );
        assert_eq!(got, "/explicit/cargo");
    }

    #[test]
    fn cargo_bin_finds_cargo_home_shim() {
        let dir = tempfile::tempdir().unwrap();
        let bin_dir = dir.path().join("bin");
        std::fs::create_dir_all(&bin_dir).unwrap();
        std::fs::write(bin_dir.join("cargo"), b"").unwrap();
        let got = resolve_cargo_bin(None, Some(dir.path().to_path_buf()), None);
        assert_eq!(got, bin_dir.join("cargo").to_string_lossy());
    }

    #[test]
    fn cargo_bin_falls_back_to_rustup_toolchain_layout() {
        // No ~/.cargo/bin/cargo shim (some installs only have this), but a real
        // binary under ~/.rustup/toolchains/<triple>/bin/cargo — the layout
        // that caused "cargo failed to spawn" until this fix.
        let cargo_home = tempfile::tempdir().unwrap(); // exists but has no bin/cargo
        let rustup_home = tempfile::tempdir().unwrap();
        let toolchain_bin = rustup_home.path().join("toolchains/stable-aarch64-apple-darwin/bin");
        std::fs::create_dir_all(&toolchain_bin).unwrap();
        std::fs::write(toolchain_bin.join("cargo"), b"").unwrap();
        let got = resolve_cargo_bin(
            None,
            Some(cargo_home.path().to_path_buf()),
            Some(rustup_home.path().to_path_buf()),
        );
        assert_eq!(got, toolchain_bin.join("cargo").to_string_lossy());
    }

    #[test]
    fn cargo_bin_falls_back_to_bare_cargo_when_nothing_found() {
        let cargo_home = tempfile::tempdir().unwrap();
        let rustup_home = tempfile::tempdir().unwrap();
        let got = resolve_cargo_bin(
            None,
            Some(cargo_home.path().to_path_buf()),
            Some(rustup_home.path().to_path_buf()),
        );
        assert_eq!(got, "cargo");
    }
}
