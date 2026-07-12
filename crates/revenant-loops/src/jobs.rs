//! The durable background-job runner — revenant's "ninja coding agent" and any
//! other one-shot work item, executed OFF the hot path so the agent keeps
//! working in real time.
//!
//! Reliability is the whole point (the thing OpenClaw got wrong): jobs live in
//! SQLite, so they survive restart; anything left `running` when the daemon
//! died is requeued on startup (at-least-once); failures retry with exponential
//! backoff up to a cap and then land in a terminal `failed` state — a job is
//! NEVER silently dropped. The claim is a single atomic transition, so a job is
//! never run twice concurrently. The state machine itself is unit-tested in
//! revenant-store; this module is a thin, honest driver over it.

use anyhow::{bail, Context, Result};
use revenant_agent::SessionManager;
use revenant_core::Tier;
use revenant_store::JobRow;
use std::sync::Arc;
use std::time::Duration;

/// How often the runner looks for due jobs.
const TICK_SECS: u64 = 5;
/// Base retry backoff; doubles per attempt (30s, 60s, 120s, …), capped.
const BACKOFF_BASE_SECS: i64 = 30;
const BACKOFF_MAX_SECS: i64 = 3600;

pub struct JobRunner {
    manager: SessionManager,
}

impl JobRunner {
    pub fn new(manager: SessionManager) -> Self {
        JobRunner { manager }
    }

    pub fn start(self: Arc<Self>) {
        tokio::spawn(async move {
            // Crash recovery: requeue anything stuck mid-run from a prior life.
            let store = &self.manager.runtime().store;
            match store.jobs_recover_running(unix_now()).await {
                Ok(n) if n > 0 => tracing::info!("jobs: requeued {n} in-flight job(s) after restart"),
                Ok(_) => {}
                Err(err) => tracing::warn!("jobs: recovery scan failed: {err:#}"),
            }
            let mut tick = tokio::time::interval(Duration::from_secs(TICK_SECS));
            loop {
                tick.tick().await;
                if let Err(err) = self.tick_once().await {
                    tracing::warn!("jobs: tick failed: {err:#}");
                }
            }
        });
    }

    async fn tick_once(self: &Arc<Self>) -> Result<()> {
        let store = &self.manager.runtime().store;
        // Drain everything currently due this tick (bounded by wall-clock: each
        // job runs to completion before the next is claimed — simple, and a
        // coding job is long enough that serial is fine at personal scale).
        while let Some(job) = store.job_claim_due(unix_now()).await? {
            tracing::info!("jobs: running #{} [{}] {}", job.id, job.kind, job.label);
            self.run_job(job).await;
        }
        Ok(())
    }

    async fn run_job(&self, job: JobRow) {
        let store = &self.manager.runtime().store;
        let outcome = match job.kind.as_str() {
            "code" => self.run_code_job(&job).await,
            other => Err(anyhow::anyhow!("unknown job kind '{other}'")),
        };
        match outcome {
            Ok(output) => {
                let _ = store.job_complete(job.id, &output).await;
                tracing::info!("jobs: #{} done", job.id);
            }
            Err(err) => {
                let backoff = (BACKOFF_BASE_SECS << job.attempts.min(6)).min(BACKOFF_MAX_SECS);
                let retry = store
                    .job_fail(job.id, &format!("{err:#}"), unix_now(), backoff)
                    .await
                    .unwrap_or(false);
                tracing::warn!(
                    "jobs: #{} failed (attempt {}, {}): {err:#}",
                    job.id,
                    job.attempts,
                    if retry { format!("retry in {backoff}s") } else { "gave up".into() },
                );
            }
        }
    }

    /// A coding subtask: run a jailed coder in an EPHEMERAL git worktree of the
    /// target repo (never the live checkout), capture the summary + diff, then
    /// tear the worktree down. Result is a proposal to review/apply — consistent
    /// with revenant never mutating a real tree without a human gate.
    async fn run_code_job(&self, job: &JobRow) -> Result<String> {
        #[derive(serde::Deserialize)]
        struct CodePayload {
            root: String,
            task: String,
            #[serde(default)]
            tier: Option<String>,
        }
        let p: CodePayload =
            serde_json::from_str(&job.payload).context("bad `code` job payload (need root + task)")?;
        let root = std::path::Path::new(&p.root);
        if !root.join(".git").exists() {
            bail!("code root {} is not a git repo (needed for a safe isolated worktree)", p.root);
        }
        // Escalate on retry: a first pass that produced nothing gets a stronger
        // model next time (a cheap tier can narrate an edit without making it).
        let base = p.tier.as_deref().unwrap_or("balanced");
        let tier_name = if job.attempts >= 2 {
            match base {
                "fast" => "balanced",
                _ => "deep",
            }
        } else {
            base
        };
        let tier: Tier = tier_name.parse().unwrap_or(Tier::Balanced);

        // Build the worktree OUTSIDE the target repo so we never litter the
        // user's working tree or pollute their `git status`.
        let branch = format!("job/{}", job.id);
        let wt = std::env::temp_dir().join("revenant-jobs").join(job.id.to_string());
        let _ = std::fs::remove_dir_all(&wt);
        if let Some(parent) = wt.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        git(root, &["worktree", "prune"]).ok();
        git(root, &["branch", "-D", &branch]).ok();
        git(root, &["worktree", "add", "-b", &branch, &wt.to_string_lossy(), "HEAD"])
            .context("creating isolated worktree")?;

        // Do the work. Capture the diff regardless of how it goes, then clean up.
        let coded = self.manager.runtime().code_once(&wt, &p.task, tier).await;
        let diff = git(&wt, &["diff"]).unwrap_or_default();
        let _ = git(root, &["worktree", "remove", "--force", &wt.to_string_lossy()]);
        let _ = git(root, &["branch", "-D", &branch]);
        let _ = std::fs::remove_dir_all(&wt); // belt-and-suspenders

        let summary = coded?;
        // A coding task that produced ZERO changes did not do its job — fail so
        // it retries (with an escalated tier), rather than reporting a hollow
        // "done". This is what turned lazy no-op runs into silent successes.
        if diff.trim().is_empty() {
            bail!("coder produced no file changes (it may have described the edit without applying it). Summary: {}", summary.chars().take(300).collect::<String>());
        }
        Ok(format!("{summary}\n\n--- proposed diff ---\n{diff}"))
    }
}

fn git(dir: &std::path::Path, args: &[&str]) -> Result<String> {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .context("running git")?;
    if !out.status.success() {
        bail!("git {:?} failed: {}", args, String::from_utf8_lossy(&out.stderr));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}
