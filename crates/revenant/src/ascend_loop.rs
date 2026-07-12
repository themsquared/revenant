//! The unattended Ascension loop.
//!
//! On a timer, inside the running daemon, the revenant runs the whole rite with
//! no human in the loop up to the pull request:
//!   observe (eval scorecard) → actuate (drive the top candidate through an
//!   isolated worktree, prove, author-review) → offer (open a PR) →
//!   gatekeep (owner-side Gate 2 over open machine PRs) →
//!   publish (sign + push landed molts to the Necropolis).
//!
//! It can NEVER merge. The four gates hold exactly as they do for manual
//! `revenant ascend`: proof(0) → author reviewer(1) → owner gatekeeper(2) →
//! human merge(3), with branch protection as the backstop. The loop is behind
//! its own switch (`ascension.loop_enabled`) — a deliberately stronger opt-in
//! than the `enabled` flag that only unlocks the manual command.

use anyhow::{Context, Result};
use revenant_core::config::{AscensionConfig, Config, NetworkConfig};
use revenant_core::home::Home;
use std::collections::BTreeSet;
use std::path::PathBuf;
use std::time::Duration;

/// Background scheduler for the unattended loop. Cheap to hold: it rebuilds its
/// clients each tick so a transient daemon/gateway blip self-heals.
pub struct AscendScheduler {
    home: Home,
    cfg: Config,
}

impl AscendScheduler {
    pub fn new(home: Home, cfg: Config) -> Self {
        AscendScheduler { home, cfg }
    }

    /// Spawn the loop if (and only if) it's switched on. Returns whether it
    /// started so the caller can log a single honest line.
    pub fn start(self) -> bool {
        let asc = self.cfg.ascension.clone();
        if !(asc.enabled && asc.loop_enabled) {
            return false;
        }
        let interval = asc.interval_secs.max(300); // floor at 5min — a runaway guard
        tokio::spawn(async move {
            // Let the control plane + gateway settle before the first heavy tick.
            tokio::time::sleep(Duration::from_secs(30)).await;
            let mut tick = tokio::time::interval(Duration::from_secs(interval));
            loop {
                tick.tick().await;
                if let Err(err) = self.tick_once().await {
                    tracing::warn!("ascension loop tick failed: {err:#}");
                }
            }
        });
        true
    }

    async fn tick_once(&self) -> Result<()> {
        let asc = &self.cfg.ascension;
        let client = revenant_client::Client::from_env(&self.home)
            .context("building self client for ascension loop")?;
        // If the daemon/gateway isn't ready, skip this tick quietly.
        if client.health().await.is_err() {
            tracing::debug!("ascension loop: daemon not ready, skipping tick");
            return Ok(());
        }

        // Leg 1 — actuate: raise a new PR from the top eval candidate. Only if a
        // repo checkout is configured (the daemon has no meaningful cwd).
        match &asc.repo_path {
            Some(rp) => {
                if let Err(err) = actuate_once(&client, &self.home, asc, rp.into()).await {
                    tracing::warn!("ascension actuator leg failed: {err:#}");
                }
            }
            None => tracing::debug!(
                "ascension loop: no ascension.repo_path set — skipping actuator leg (gatekeep/publish only)"
            ),
        }

        // Leg 2 — gatekeep: owner-side review of any open machine-authored PRs.
        if asc.gatekeep {
            if let Err(err) = gatekeep_open_prs(&client, asc, &asc.pr_repo, None, false).await {
                tracing::warn!("ascension gatekeeper leg failed: {err:#}");
            }
        }

        // Leg 3 — publish: sign + push landed molts to the network.
        if self.cfg.network.enabled && self.cfg.network.auto_publish {
            if let Err(err) =
                publish_landed_molts(&self.home, asc, &self.cfg.network, false).await
            {
                tracing::warn!("ascension publish leg failed: {err:#}");
            }
        }

        // Leg 4 — promote: cut a CalVer release when enough molts have landed.
        if asc.auto_release {
            match promote_release(&self.home, asc, false).await {
                Ok(Some(msg)) => tracing::info!("ascension release: {msg}"),
                Ok(None) => {}
                Err(err) => tracing::warn!("ascension release leg failed: {err:#}"),
            }
        }
        Ok(())
    }
}

/// Observe the live scorecard, detect candidates, and drive the top one through
/// the actuator. `live` follows the autonomy dial: anything but `propose` opens
/// a real PR (the rate limit inside `offer` still applies).
async fn actuate_once(
    client: &revenant_client::Client,
    home: &Home,
    asc: &AscensionConfig,
    repo: PathBuf,
) -> Result<()> {
    if !repo.join(".git").exists() {
        anyhow::bail!("ascension.repo_path {} is not a git checkout", repo.display());
    }
    let suite = revenant_evals::default_suite();
    let report = revenant_evals::run_suite(client, &suite).await?;
    let candidates = revenant_ascension::detect(&report);
    let Some(top) = candidates.into_iter().next() else {
        tracing::info!("ascension loop: scorecard clean — nothing to raise");
        return Ok(());
    };
    let live = asc.autonomy != "propose";
    tracing::info!(
        "ascension loop: driving candidate `{}` (autonomy={}, {})",
        top.target,
        asc.autonomy,
        if live { "LIVE — may open a PR" } else { "dry-run" }
    );
    let run_cfg = crate::ascend_run_cfg(asc, live);
    let today = (crate::now_ts() / 86_400).to_string();
    let outcome = revenant_ascension::run::run_candidate(
        client, &repo, top, &run_cfg, home.root(), &today, None,
    )
    .await?;
    tracing::info!(
        "ascension loop: actuator outcome build={} test={} clippy={} reviewer={:?} offer={:?}",
        outcome.build_ok,
        outcome.test_ok,
        outcome.clippy_ok,
        outcome.reviewer_approved,
        outcome.offer,
    );
    Ok(())
}

/// Owner-side gatekeeper (Gate 2): independently review every open PR carrying
/// the `ascension` label, comment the verdict, and label it approved/blocked.
/// Never merges. Shared by the CLI (`revenant pr-review`) and the loop.
/// Returns (approved, blocked) counts.
pub(crate) async fn gatekeep_open_prs(
    client: &revenant_client::Client,
    asc: &AscensionConfig,
    repo: &str,
    limit: Option<usize>,
    verbose: bool,
) -> Result<(usize, usize)> {
    // Ensure the verdict labels exist (idempotent).
    for (name, color, desc) in [
        ("ascension-approved", "0e8a16", "gatekeeper approved — ready for human merge"),
        ("ascension-blocked", "b60205", "gatekeeper blocked — do not merge as-is"),
    ] {
        let _ = std::process::Command::new("gh")
            .args(["label", "create", name, "--repo", repo, "--color", color, "--description", desc, "--force"])
            .output();
    }

    let list = crate::gh_capture(&[
        "pr", "list", "--repo", repo, "--label", "ascension", "--state", "open", "--json", "number,title",
    ])
    .context("gh pr list failed (is gh authed?)")?;
    let mut prs: Vec<serde_json::Value> = serde_json::from_str(&list).unwrap_or_default();
    if let Some(n) = limit {
        prs.truncate(n);
    }
    if prs.is_empty() {
        if verbose {
            println!("No open `ascension` PRs to review on {repo}.");
        }
        return Ok((0, 0));
    }
    if verbose {
        println!("🜁 Gatekeeper — independently reviewing {} PR(s) on {repo}\n", prs.len());
    }

    let (mut approved, mut blocked) = (0usize, 0usize);
    for pr in prs {
        let n = pr["number"].as_i64().unwrap_or(0);
        let title = pr["title"].as_str().unwrap_or("").to_string();
        let ns = n.to_string();
        let body = crate::gh_capture(&["pr", "view", &ns, "--repo", repo, "--json", "body", "-q", ".body"])
            .unwrap_or_default();
        let diff = crate::gh_capture(&["pr", "diff", &ns, "--repo", repo]).unwrap_or_default();
        if diff.trim().is_empty() {
            if verbose {
                println!("  PR #{n} {title}\n    → skipped (no diff available)");
            }
            continue;
        }
        let verdict = revenant_ascension::review::review_pr(
            client, &asc.reviewer_tier, &title, &body, &diff, &asc.denylist,
        )
        .await?;
        let (add, rm, mark) = if verdict.approved {
            approved += 1;
            ("ascension-approved", "ascension-blocked", "✅ APPROVED")
        } else {
            blocked += 1;
            ("ascension-blocked", "ascension-approved", "🛑 CHANGES REQUESTED")
        };
        let reasons = verdict
            .reasons
            .iter()
            .chain(verdict.concerns.iter())
            .map(|r| format!("- {r}"))
            .collect::<Vec<_>>()
            .join("\n");
        let comment = format!(
            "## 🜁 Ascension gatekeeper — {mark} (confidence {:.2})\n\n{reasons}\n\n_Independent owner-side review (Gate 2). A human still makes the final merge decision._",
            verdict.confidence
        );
        let _ = std::process::Command::new("gh")
            .args(["pr", "comment", &ns, "--repo", repo, "--body", &comment])
            .output();
        let _ = std::process::Command::new("gh")
            .args(["pr", "edit", &ns, "--repo", repo, "--add-label", add])
            .output();
        let _ = std::process::Command::new("gh")
            .args(["pr", "edit", &ns, "--repo", repo, "--remove-label", rm])
            .output();
        if verbose {
            println!("  PR #{n} {title}\n    → {mark} (conf {:.2}) · labeled `{add}`", verdict.confidence);
        } else {
            tracing::info!("gatekeeper: PR #{n} '{title}' → {mark} (conf {:.2})", verdict.confidence);
        }
    }
    if verbose {
        println!(
            "\n{approved} approved · {blocked} blocked. Review the `ascension-approved` PRs and merge the ones you want:\n  gh pr list --repo {repo} --label ascension-approved"
        );
    }
    Ok((approved, blocked))
}

/// Agent-driven release promotion (idea 2). When enough merged-but-unreleased
/// molts have landed on main, cut the next CalVer release by tagging main and
/// pushing — the existing CI (`v*` tag trigger) builds + publishes the assets.
/// Safe: everything in the release already cleared the 4 gates + human merge.
/// Idempotent via `released.json`. Returns a status line, or None when there's
/// nothing (enough) to cut.
pub(crate) async fn promote_release(
    home: &Home,
    asc: &AscensionConfig,
    dry_run: bool,
) -> Result<Option<String>> {
    let Some(repo) = asc.repo_path.as_deref() else {
        tracing::debug!("promote: no ascension.repo_path — skipping");
        return Ok(None);
    };
    // Merged, human-landed molts.
    let list = crate::gh_capture(&[
        "pr", "list", "--repo", &asc.pr_repo, "--label", "ascension", "--state", "merged",
        "--json", "number,title", "--limit", "100",
    ])
    .context("gh pr list --state merged failed")?;
    let prs: Vec<serde_json::Value> = serde_json::from_str(&list).unwrap_or_default();
    let merged: Vec<(i64, String)> = prs
        .iter()
        .filter_map(|p| Some((p["number"].as_i64()?, p["title"].as_str().unwrap_or("").to_string())))
        .collect();

    let mut released = load_released(home);
    let fresh: Vec<(i64, String)> =
        merged.into_iter().filter(|(n, _)| !released.contains(n)).collect();
    if (fresh.len() as u32) < asc.release_min_molts {
        tracing::debug!("promote: {} fresh molt(s) < {} threshold", fresh.len(), asc.release_min_molts);
        return Ok(None);
    }

    // Next CalVer: bump patch within the current month, else start a new month.
    let (y, m) = current_year_month()?;
    let last = latest_calver_tag(repo);
    let patch = match last {
        Some((ly, lm, lp)) if ly == y && lm == m => lp + 1,
        _ => 0,
    };
    let tag = format!("v{y}.{m}.{patch}");
    let notes = format!(
        "Ascension release {tag} — {} molt(s) landed on main:\n{}",
        fresh.len(),
        fresh.iter().map(|(n, t)| format!("- #{n} {t}")).collect::<Vec<_>>().join("\n"),
    );

    if dry_run {
        return Ok(Some(format!("[dry-run] would cut {tag} bundling {} molt(s)", fresh.len())));
    }

    // Tag main's HEAD and push — CI (v* trigger) builds and publishes.
    run_git(repo, &["tag", "-a", &tag, "-m", &notes]).context("creating release tag")?;
    if let Err(e) = run_git(repo, &["push", "origin", &tag]) {
        // Roll back the local tag so a transient push failure doesn't wedge us.
        let _ = run_git(repo, &["tag", "-d", &tag]);
        return Err(e).context("pushing release tag");
    }
    for (n, _) in &fresh {
        released.insert(*n);
    }
    save_released(home, &released);
    Ok(Some(format!("🜁 cut release {tag} ({} molt(s)) — CI is building assets", fresh.len())))
}

fn current_year_month() -> Result<(u32, u32)> {
    let out = std::process::Command::new("date")
        .args(["-u", "+%Y-%m"])
        .output()
        .context("running `date`")?;
    let s = String::from_utf8_lossy(&out.stdout);
    let (y, m) = s.trim().split_once('-').context("unexpected date output")?;
    Ok((y.parse()?, m.trim_start_matches('0').parse()?))
}

/// Highest *stable* CalVer tag already in the repo (so patch bumps are
/// monotonic). Rolling `-main.<sha>` prerelease tags are skipped: their patch
/// is a commit count, which would otherwise hijack the stable patch sequence.
fn latest_calver_tag(repo: &str) -> Option<(u32, u32, u32)> {
    let out = std::process::Command::new("git")
        .args(["-C", repo, "tag", "--list"])
        .output()
        .ok()?;
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter(|t| !t.contains('-')) // stable tags only; skip prereleases
        .filter_map(crate::parse_calver)
        .max()
}

fn run_git(repo: &str, args: &[&str]) -> Result<()> {
    let mut full = vec!["-C", repo];
    full.extend_from_slice(args);
    let out = std::process::Command::new("git").args(&full).output().context("running git")?;
    if !out.status.success() {
        anyhow::bail!("git {:?} failed: {}", args, String::from_utf8_lossy(&out.stderr));
    }
    Ok(())
}

fn released_state_path(home: &Home) -> PathBuf {
    home.root().join("ascension").join("released.json")
}

fn load_released(home: &Home) -> BTreeSet<i64> {
    std::fs::read_to_string(released_state_path(home))
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

fn save_released(home: &Home, set: &BTreeSet<i64>) {
    let p = released_state_path(home);
    if let Some(parent) = p.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(json) = serde_json::to_string_pretty(set) {
        let _ = std::fs::write(p, json);
    }
}

/// The set of merged PR numbers already published, persisted so a restart or a
/// re-tick never double-publishes. Content addressing on the server dedupes
/// too, but this keeps us from re-signing + re-uploading needlessly.
fn published_state_path(home: &Home) -> PathBuf {
    home.root().join("ascension").join("published.json")
}

fn load_published(home: &Home) -> BTreeSet<i64> {
    std::fs::read_to_string(published_state_path(home))
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

fn save_published(home: &Home, set: &BTreeSet<i64>) {
    let p = published_state_path(home);
    if let Some(parent) = p.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(json) = serde_json::to_string_pretty(set) {
        let _ = std::fs::write(p, json);
    }
}

/// Find machine-authored PRs that a human actually merged (landed) and publish
/// each as a signed Improvement artifact to the network — the horde learns only
/// from changes that cleared every gate INCLUDING the human merge. Publishing
/// requires this node's identity to be bound to a verified account; a 403 is
/// surfaced as a warning, not a crash.
pub(crate) async fn publish_landed_molts(
    home: &Home,
    asc: &AscensionConfig,
    netcfg: &NetworkConfig,
    verbose: bool,
) -> Result<usize> {
    let Some(url) = &netcfg.necropolis_url else {
        tracing::debug!("publish: no necropolis_url set — skipping");
        return Ok(0);
    };
    let list = crate::gh_capture(&[
        "pr", "list", "--repo", &asc.pr_repo, "--label", "ascension", "--state", "merged",
        "--json", "number,title,body", "--limit", "30",
    ])
    .context("gh pr list --state merged failed")?;
    let prs: Vec<serde_json::Value> = serde_json::from_str(&list).unwrap_or_default();

    let mut published = load_published(home);
    let id = revenant_net::Identity::load_or_create(&home.identity_dir())?;
    let client = revenant_net::NecropolisClient::new(url);
    let mut count = 0usize;

    for pr in prs {
        let n = pr["number"].as_i64().unwrap_or(0);
        if n == 0 || published.contains(&n) {
            continue;
        }
        let title = pr["title"].as_str().unwrap_or("").to_string();
        let body = pr["body"].as_str().unwrap_or("").to_string();
        let ns = n.to_string();
        let diff = crate::gh_capture(&["pr", "diff", &ns, "--repo", &asc.pr_repo]).unwrap_or_default();
        if diff.trim().is_empty() {
            tracing::warn!("publish: PR #{n} has no diff — skipping");
            continue;
        }
        // Payload = the landed patch; description records provenance.
        let description = format!(
            "Ascension molt — landed in {}#{n} (merged by a human).\n\n{body}",
            asc.pr_repo
        );
        let artifact = revenant_net::Artifact::create(
            &id,
            revenant_net::ArtifactKind::Improvement,
            title.clone(),
            description,
            diff.as_bytes(),
            None,
            crate::now_ts(),
        );
        match client.publish(&artifact).await {
            Ok(aid) => {
                published.insert(n);
                count += 1;
                if verbose {
                    println!("published molt from PR #{n} '{title}' → {}", &aid[..12.min(aid.len())]);
                } else {
                    tracing::info!("published landed molt PR #{n} '{title}' → {}", &aid[..12.min(aid.len())]);
                }
            }
            Err(err) => {
                // 403 (unbound/unverified) or transient — log and retry next tick.
                tracing::warn!("publish: PR #{n} failed: {err:#}");
                if verbose {
                    println!("could not publish PR #{n}: {err:#}");
                }
            }
        }
    }
    save_published(home, &published);
    Ok(count)
}
