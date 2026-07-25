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
/// Heartbeat cadence for a running job, in seconds. Backs off so a long job
/// reports ~6 times over 40 minutes instead of 80 — silence is the bug being
/// fixed, but a chatty job is a different way of being useless.
const HEARTBEAT_SCHEDULE: [u64; 5] = [30, 60, 120, 300, 600];

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
                Ok(n) if n > 0 => {
                    tracing::info!("jobs: requeued {n} in-flight job(s) after restart")
                }
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
        // "Still working" beacon. Runs beside the job and stops when `_beacon` is
        // dropped at the end of this function — including on the panic path, so a
        // crashed job cannot leave a heartbeat claiming progress forever.
        let _beacon = Heartbeat::start(
            self.manager.runtime().events.clone(),
            job.id,
            job.label.clone(),
        );
        let outcome = match job.kind.as_str() {
            "code" => self.run_code_job(&job).await,
            "reminder" => self.run_reminder_job(&job).await,
            "send_media" => self.run_send_media_job(&job).await,
            other => Err(anyhow::anyhow!("unknown job kind '{other}'")),
        };
        let events = &self.manager.runtime().events;
        match outcome {
            Ok(output) => {
                let _ = store.job_complete(job.id, &output).await;
                tracing::info!("jobs: #{} done", job.id);
                // Close the loop: a queued async task must report back, not
                // vanish. Reminders already emit their own event; code jobs
                // (and any future kind) surface completion here.
                if job.kind != "reminder" && job.kind != "send_media" {
                    events.emit(revenant_core::Event::JobFinished {
                        id: job.id,
                        label: job.label.clone(),
                        ok: true,
                        detail: summarize(&job.kind, &output).render(),
                    });
                }
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
                    if retry {
                        format!("retry in {backoff}s")
                    } else {
                        "gave up".into()
                    },
                );
                // Only surface a TERMINAL failure — retryable ones stay quiet
                // so a flaky task doesn't spam the owner between attempts.
                if !retry {
                    // A terminal failure gets a POST-MORTEM, not just an error
                    // string: what class of failure it was and what to do about
                    // it. Also journalled, so self-review reads it as friction and
                    // can change behaviour — a failure that only ever reaches a
                    // chat message teaches the agent nothing.
                    let raw = format!("{err:#}");
                    let pm = post_mortem(&raw, job.attempts);
                    let _ = store
                        .journal_add(
                            "job_failed",
                            None,
                            &format!("[{}] {} — {}", pm.class, job.label, pm.action),
                        )
                        .await;
                    events.emit(revenant_core::Event::JobFinished {
                        id: job.id,
                        label: job.label.clone(),
                        ok: false,
                        detail: pm.render(&raw),
                    });
                }
            }
        }
    }

    /// A one-shot reminder/timer: the job was scheduled with `run_after` = the
    /// due time, so simply firing it here delivers the message. Emit a
    /// ReminderFired event — the Telegram channel (and web UI) pushes it to the
    /// owner. Fire-once by construction: the job goes `done` and never repeats.
    async fn run_reminder_job(&self, job: &JobRow) -> Result<String> {
        #[derive(serde::Deserialize)]
        struct ReminderPayload {
            message: String,
        }
        let p: ReminderPayload = serde_json::from_str(&job.payload)
            .context("bad `reminder` job payload (need message)")?;
        self.manager
            .runtime()
            .events
            .emit(revenant_core::Event::ReminderFired {
                message: p.message.clone(),
            });
        Ok(format!(
            "reminder delivered: {}",
            p.message.chars().take(80).collect::<String>()
        ))
    }

    /// A file/image ready to push (e.g. a rendered chart): emit a SendMedia
    /// event — the Telegram channel (and any future channel) reads the file
    /// off disk and delivers it to every paired peer. Fire-once by
    /// construction, same as a reminder.
    async fn run_send_media_job(&self, job: &JobRow) -> Result<String> {
        #[derive(serde::Deserialize)]
        struct SendMediaPayload {
            kind: String,
            file_path: String,
            caption: Option<String>,
        }
        let p: SendMediaPayload = serde_json::from_str(&job.payload)
            .context("bad `send_media` job payload (need kind + file_path)")?;
        self.manager
            .runtime()
            .events
            .emit(revenant_core::Event::SendMedia {
                kind: p.kind.clone(),
                file_path: p.file_path.clone(),
                caption: p.caption.clone(),
            });
        Ok(format!("media queued for delivery: {}", p.file_path))
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
        let p: CodePayload = serde_json::from_str(&job.payload)
            .context("bad `code` job payload (need root + task)")?;
        let root = std::path::Path::new(&p.root);
        if !root.join(".git").exists() {
            bail!(
                "code root {} is not a git repo (needed for a safe isolated worktree)",
                p.root
            );
        }
        // Escalate on retry, but ONLY within the paid API hierarchy — a cheap
        // API tier can narrate an edit without making it, so bump it. A `local`
        // (free) job must NEVER auto-escalate to a paid tier: that would silently
        // charge you for a test you deliberately ran for free. local stays local.
        let base = p.tier.as_deref().unwrap_or("balanced");
        let tier_name = if job.attempts >= 2 {
            match base {
                "fast" => "balanced",
                "balanced" => "deep",
                other => other, // deep stays deep; local/unknown never escalate
            }
        } else {
            base
        };
        let tier: Tier = tier_name.parse().unwrap_or(Tier::Balanced);

        // Build the worktree OUTSIDE the target repo so we never litter the
        // user's working tree or pollute their `git status`.
        let branch = format!("job/{}", job.id);
        let wt = std::env::temp_dir()
            .join("revenant-jobs")
            .join(job.id.to_string());
        let _ = std::fs::remove_dir_all(&wt);
        if let Some(parent) = wt.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        git(root, &["worktree", "prune"]).ok();
        git(root, &["branch", "-D", &branch]).ok();
        git(
            root,
            &[
                "worktree",
                "add",
                "-b",
                &branch,
                &wt.to_string_lossy(),
                "HEAD",
            ],
        )
        .context("creating isolated worktree")?;

        // Do the work. Capture the diff regardless of how it goes, then clean up.
        let coded = self.manager.runtime().code_once(&wt, &p.task, tier).await;
        // Stage everything first so brand-new untracked files the coder created
        // are captured too — a plain `git diff` omits untracked files, which made
        // real new files (e.g. Kalshi integration attempts) look like no-ops.
        let _ = git(&wt, &["add", "-A"]);
        let diff = git(&wt, &["diff", "--cached"]).unwrap_or_default();
        let _ = git(
            root,
            &["worktree", "remove", "--force", &wt.to_string_lossy()],
        );
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
        bail!(
            "git {:?} failed: {}",
            args,
            String::from_utf8_lossy(&out.stderr)
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// "Still working" beacon for a running job.
///
/// Aborts its task on [`Drop`], which is what makes it honest: the beacon exists
/// only while the job's scope does, so a panicking or cancelled job cannot leave
/// something behind reporting progress it isn't making.
struct Heartbeat {
    handle: tokio::task::JoinHandle<()>,
}

impl Heartbeat {
    fn start(events: revenant_core::EventBus, id: i64, label: String) -> Self {
        let handle = tokio::spawn(async move {
            let started = std::time::Instant::now();
            let mut step = 0usize;
            loop {
                // Walk the schedule, then hold at its last (longest) interval.
                let wait = HEARTBEAT_SCHEDULE[step.min(HEARTBEAT_SCHEDULE.len() - 1)];
                step += 1;
                tokio::time::sleep(Duration::from_secs(wait)).await;
                let elapsed = started.elapsed().as_secs() as i64;
                events.emit(revenant_core::Event::JobProgress {
                    id,
                    label: label.clone(),
                    note: "still working".into(),
                    elapsed_secs: elapsed,
                });
            }
        });
        Heartbeat { handle }
    }
}

impl Drop for Heartbeat {
    fn drop(&mut self) {
        self.handle.abort();
    }
}

/// A terminal failure, classified, with something to do about it.
#[derive(Debug, PartialEq, Eq)]
pub struct PostMortem {
    pub class: &'static str,
    pub action: String,
    /// Whether the owner is the one who has to act. A transient provider blip is
    /// noise; a missing key is not.
    pub needs_owner: bool,
}

impl PostMortem {
    /// The message the owner actually reads: what broke, what kind of problem it
    /// is, and the next step — with the raw error kept last so it is available
    /// without leading.
    pub fn render(&self, raw: &str) -> String {
        let raw = raw.lines().next().unwrap_or(raw).chars().take(200).collect::<String>();
        format!("{} — {}\n{}", self.class, self.action, raw)
    }
}

/// Classify a job failure and say what to do.
///
/// Deliberately pattern-matching rather than an LLM call: a post-mortem must work
/// when the failure IS the model being unreachable, and it must not cost money to
/// explain that something already cost money and failed.
pub fn post_mortem(err: &str, attempts: i64) -> PostMortem {
    let e = err.to_lowercase();
    let has = |needles: &[&str]| needles.iter().any(|n| e.contains(n));

    if has(&["overloaded", "rate limit", "429", "timed out", "timeout", "connection", "dns", "temporarily"]) {
        return PostMortem {
            class: "transient",
            action: format!(
                "provider or network blip, and it already retried {attempts}x. Nothing for you to \
                 do — requeue it if you still want the work."
            ),
            needs_owner: false,
        };
    }
    // NB "api_key" as well as "api key": the most common form of this failure
    // names an environment variable (ANTHROPIC_API_KEY), and matching only the
    // spaced spelling sent a missing-key error to "unclassified" — the single
    // most likely real failure, misrouted.
    if has(&[
        "api key", "api_key", "unauthorized", "401", "403", "no key", "credential",
        "insufficient balance", "quota", "not authenticated",
    ]) {
        return PostMortem {
            class: "credentials/billing",
            action: "a key is missing, wrong, or out of credit. Check `revenant doctor` and \
                     secrets.env — retrying cannot fix this."
                .into(),
            needs_owner: true,
        };
    }
    if has(&["not a git repo", "no such file", "not found", "permission denied", "no config"]) {
        return PostMortem {
            class: "environment",
            action: "the job's assumptions about this machine were wrong (missing path, repo or \
                     permission). Fix the target or the job's payload, not the agent."
                .into(),
            needs_owner: true,
        };
    }
    if has(&["produced no file changes", "described the edit"]) {
        return PostMortem {
            class: "skill gap",
            action: "the coder narrated an edit instead of making one — the tier is too weak for \
                     this task, or the task needs splitting. Escalate the tier or decompose it."
                .into(),
            needs_owner: false,
        };
    }
    if has(&["panicked", "unwrap", "index out of bounds", "compile", "cannot find"]) {
        return PostMortem {
            class: "bug",
            action: "this looks like a defect in the agent or the code under test, not bad input. \
                     Worth a real fix rather than a retry."
                .into(),
            needs_owner: true,
        };
    }
    PostMortem {
        class: "unclassified",
        action: format!(
            "gave up after {attempts} attempt(s) and I could not classify why. The raw error is \
             below — this one needs eyes."
        ),
        needs_owner: true,
    }
}

#[cfg(test)]
mod status_tests {
    use super::*;

    #[test]
    fn transient_failures_do_not_demand_the_owner() {
        for err in [
            "MCP overloaded",
            "HTTP 429 Too Many Requests",
            "operation timed out",
            "connection refused",
        ] {
            let pm = post_mortem(err, 3);
            assert_eq!(pm.class, "transient", "{err}");
            assert!(!pm.needs_owner, "a provider blip must not page the owner: {err}");
        }
    }

    #[test]
    fn failures_only_the_owner_can_fix_say_so() {
        // The distinction that matters: retrying cannot fix any of these, so the
        // message has to point at the owner rather than suggest patience.
        for (err, class) in [
            ("missing ANTHROPIC_API_KEY", "credentials/billing"),
            ("account is suspended due to insufficient balance", "credentials/billing"),
            ("code root /nope is not a git repo", "environment"),
            ("thread panicked at unwrap on None", "bug"),
        ] {
            let pm = post_mortem(err, 1);
            assert_eq!(pm.class, class, "{err}");
            assert!(pm.needs_owner, "{err} needs the owner");
            assert!(!pm.action.is_empty());
        }
    }

    /// The case worth classifying specially: the coder describing an edit instead
    /// of making one is a capability problem, and the fix is tier or scope — not a
    /// retry and not the owner's keys.
    #[test]
    fn a_narrating_coder_is_a_skill_gap_with_a_concrete_next_step() {
        let pm = post_mortem(
            "coder produced no file changes (it may have described the edit without applying it)",
            2,
        );
        assert_eq!(pm.class, "skill gap");
        assert!(pm.action.contains("tier") || pm.action.contains("decompose"));
    }

    #[test]
    fn an_unknown_failure_admits_it_rather_than_guessing() {
        let pm = post_mortem("flurbled the wibbit", 4);
        assert_eq!(pm.class, "unclassified");
        assert!(pm.needs_owner, "if we cannot explain it, a human must look");
        assert!(pm.action.contains("4 attempt"), "attempt count is part of the story");
    }

    #[test]
    fn the_rendered_message_leads_with_meaning_not_the_stack() {
        let pm = post_mortem("HTTP 429 rate limit exceeded", 3);
        let msg = pm.render("HTTP 429 rate limit exceeded\n  at some::frame\n  at another::frame");
        assert!(msg.starts_with("transient — "), "class and action come first: {msg}");
        assert!(msg.contains("429"), "the raw error is still available");
        assert!(!msg.contains("another::frame"), "only the first raw line is kept");
    }
}

/// What a finished job actually accomplished, and what it did NOT.
///
/// "Done" is rarely the whole truth. A code job that succeeds has produced a
/// *proposal* — a diff in an ephemeral worktree that was torn down — and the
/// change is not in the tree. Reporting only "finished" invites the owner to
/// believe work landed that hasn't, which is a worse failure than reporting an
/// error.
#[derive(Debug, PartialEq, Eq)]
pub struct JobReport {
    /// One or two lines on what happened.
    pub did: String,
    /// Anything left undone that the owner has to decide about. `None` means
    /// genuinely nothing outstanding — not "unknown".
    pub remaining: Option<String>,
}

impl JobReport {
    pub fn render(&self) -> String {
        match &self.remaining {
            Some(r) => format!("{}\n⏭ Remaining: {r}", self.did),
            None => self.did.clone(),
        }
    }
}

/// Derive a report from a job's raw output.
///
/// Pure and per-kind, rather than a generic first-line clip: only the kind knows
/// what "remaining" means. Kept as a function over the output string so the job
/// runners keep their existing signatures.
pub fn summarize(kind: &str, output: &str) -> JobReport {
    const DIFF_MARKER: &str = "--- proposed diff ---";
    match kind {
        "code" => {
            let (summary, diff) = match output.split_once(DIFF_MARKER) {
                Some((s, d)) => (s.trim(), Some(d.trim())),
                None => (output.trim(), None),
            };
            let did = clip(first_lines(summary, 2), 400);
            match diff {
                // The whole point: a code job PROPOSES. The worktree is gone and
                // the live tree is untouched, so "applied" is never implied.
                Some(d) if !d.is_empty() => JobReport {
                    did,
                    remaining: Some(format!(
                        "the diff is a proposal — nothing was applied to your tree ({} changed line(s) to review)",
                        d.lines()
                            .filter(|l| {
                                // `+++ b/file` and `--- a/file` are HEADERS, not
                                // changes — counting them inflated every report
                                // by two per touched file.
                                !l.starts_with("+++")
                                    && !l.starts_with("---")
                                    && (l.starts_with('+') || l.starts_with('-'))
                            })
                            .count()
                    )),
                },
                // Success with no diff shouldn't happen (the job fails on an empty
                // diff), but if it does, say so rather than implying a change.
                _ => JobReport {
                    did,
                    remaining: Some("no diff was produced — nothing to apply".into()),
                },
            }
        }
        _ => JobReport { did: clip(first_lines(output.trim(), 2), 400), remaining: None },
    }
}

/// Char-safe truncation (this module has no `clip` of its own).
fn clip(s: String, max: usize) -> String {
    if s.chars().count() <= max {
        return s;
    }
    s.chars().take(max).collect::<String>() + "…"
}

fn first_lines(s: &str, n: usize) -> String {
    s.lines().filter(|l| !l.trim().is_empty()).take(n).collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod report_tests {
    use super::*;

    /// A successful code job must never read as "the change landed". It produced a
    /// diff in a worktree that was then destroyed; the owner still has to apply it.
    #[test]
    fn a_code_job_reports_the_diff_as_unapplied() {
        let out = "Added retry to the fetch path.\nCovered by a new unit test.\n\n\
                   --- proposed diff ---\n\
                   --- a/src/lib.rs\n+++ b/src/lib.rs\n+    retry(3);\n-    once();\n";
        let r = summarize("code", out);
        assert!(r.did.contains("Added retry"));
        let remaining = r.remaining.clone().expect("a proposal is always outstanding");
        assert!(remaining.contains("nothing was applied"), "{remaining}");
        // Counts changed lines, not the +++/--- file headers.
        assert!(remaining.contains("2 changed line"), "{remaining}");

        let rendered = r.render();
        assert!(rendered.contains("⏭ Remaining:"), "the split must be visible");
    }

    #[test]
    fn success_without_a_diff_says_so_instead_of_implying_a_change() {
        let r = summarize("code", "Investigated; no edit was necessary.");
        assert_eq!(
            r.remaining.as_deref(),
            Some("no diff was produced — nothing to apply")
        );
    }

    #[test]
    fn other_kinds_have_nothing_outstanding_and_say_None_not_unknown() {
        let r = summarize("something-else", "delivered the thing\nand tidied up");
        assert_eq!(r.remaining, None, "None means nothing outstanding, not unknown");
        assert_eq!(r.did, "delivered the thing and tidied up");
        assert_eq!(r.render(), r.did, "no Remaining line when there is nothing to say");
    }

    #[test]
    fn a_long_summary_is_clipped_but_the_remaining_line_survives() {
        let long = "x".repeat(5_000);
        let out = format!("{long}\n\n--- proposed diff ---\n+one\n");
        let r = summarize("code", &out);
        // clip() bounds CHARS and appends an ellipsis; assert in the same unit it
        // works in rather than in bytes.
        assert!(r.did.chars().count() <= 401, "did is clipped to something sendable");
        assert!(
            r.render().contains("⏭ Remaining:"),
            "clipping must never eat the outstanding-work line"
        );
    }
}
