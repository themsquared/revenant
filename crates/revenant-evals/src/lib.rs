//! revenant-evals: a reproducible scorecard for the harness.
//!
//! The eval runner drives the *real* running daemon over the control API —
//! it creates a session, streams `/v1/events`, sends a turn, and captures the
//! ground truth the gateway reports: end-to-end latency, tokens (in/out),
//! which model actually served the turn, and the tools the agent invoked.
//! Grading is deterministic (substring/regex/tool assertions) so a run is
//! fast, free, and repeatable — no LLM judge, no flaky numbers. The same task
//! files can be pointed at any harness that speaks the same surface, so the
//! scorecard is a head-to-head, not a self-graded victory lap.

use anyhow::{Context, Result};
use futures::StreamExt;
use revenant_client::Client;
use revenant_core::Event;
use serde::{Deserialize, Serialize};
use std::time::Duration;

/// The embedded default suite — runs out of the box with `revenant eval`.
const DEFAULT_SUITE: &str = include_str!("../suites/default.toml");
/// Agent-behaviour suite: tool trajectories, multi-turn memory, research.
const AGENT_SUITE: &str = include_str!("../suites/agents.toml");

/// Per-turn wall-clock ceiling. A turn that blocks (e.g. on an approval) fails
/// the task instead of hanging the whole suite.
const TURN_TIMEOUT: Duration = Duration::from_secs(120);

#[derive(Debug, Clone, Deserialize)]
pub struct Suite {
    #[serde(default, rename = "task")]
    pub tasks: Vec<EvalTask>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct EvalTask {
    pub id: String,
    /// The graded prompt.
    pub prompt: String,
    /// Model tier override (fast|balanced|deep|local). None → agent default.
    #[serde(default)]
    pub tier: Option<String>,
    /// Prior turns run (and ignored) to prime context/memory before grading.
    #[serde(default)]
    pub setup: Vec<String>,
    /// Free-form tags for slicing the report (e.g. "speed", "memory").
    #[serde(default)]
    pub tags: Vec<String>,
    /// Deterministic pass conditions — ALL specified conditions must hold.
    pub grade: GradeSpec,
}

/// A flat, TOML-friendly grading spec. Every field that is set becomes a
/// condition; the task passes only if all set conditions pass (AND).
#[derive(Debug, Clone, Default, Deserialize)]
pub struct GradeSpec {
    /// Every string must appear in the final answer (case-insensitive).
    #[serde(default)]
    pub contains: Vec<String>,
    /// At least one string must appear (case-insensitive).
    #[serde(default)]
    pub contains_any: Vec<String>,
    /// None of these may appear (case-insensitive) — refusals, leak checks.
    #[serde(default)]
    pub not_contains: Vec<String>,
    /// The final answer must match this regex.
    #[serde(default)]
    pub regex: Option<String>,
    /// A tool with this name must have been invoked during the turn.
    #[serde(default)]
    pub tool_used: Option<String>,
    /// Every one of these tools must have been invoked (any order) — a
    /// trajectory-coverage check for agent evals.
    #[serde(default)]
    pub tools_all: Vec<String>,
    /// At least this many tool calls must have occurred.
    #[serde(default)]
    pub min_tools: Option<usize>,
}

impl GradeSpec {
    /// Returns Ok(()) on pass, or Err(reason) on the first failed condition.
    fn check(&self, r: &TurnResult) -> std::result::Result<(), String> {
        if let Some(err) = &r.failed {
            return Err(format!("turn failed: {err}"));
        }
        let hay = r.final_text.to_lowercase();
        for s in &self.contains {
            if !hay.contains(&s.to_lowercase()) {
                return Err(format!("missing expected substring {s:?}"));
            }
        }
        if !self.contains_any.is_empty()
            && !self.contains_any.iter().any(|s| hay.contains(&s.to_lowercase()))
        {
            return Err(format!("none of {:?} present", self.contains_any));
        }
        for s in &self.not_contains {
            if hay.contains(&s.to_lowercase()) {
                return Err(format!("forbidden substring {s:?} present"));
            }
        }
        if let Some(pat) = &self.regex {
            let re = regex::Regex::new(pat).map_err(|e| format!("bad regex: {e}"))?;
            if !re.is_match(&r.final_text) {
                return Err(format!("regex {pat:?} did not match"));
            }
        }
        if let Some(tool) = &self.tool_used {
            if !r.tools.iter().any(|t| t == tool) {
                return Err(format!("tool {tool:?} was not used (used: {:?})", r.tools));
            }
        }
        for tool in &self.tools_all {
            if !r.tools.iter().any(|t| t == tool) {
                return Err(format!("required tool {tool:?} not used (used: {:?})", r.tools));
            }
        }
        if let Some(min) = self.min_tools {
            if r.tools.len() < min {
                return Err(format!("used {} tools, expected >= {min}", r.tools.len()));
            }
        }
        Ok(())
    }
}

/// Ground truth captured from one turn, as reported by the daemon/gateway.
#[derive(Debug, Clone, Serialize)]
pub struct TurnResult {
    pub final_text: String,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub tools: Vec<String>,
    pub latency_ms: u128,
    pub model: Option<String>,
    /// Set when the turn errored (TurnFailed or timeout).
    pub failed: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct TaskOutcome {
    pub id: String,
    pub tags: Vec<String>,
    pub passed: bool,
    pub reason: String,
    pub result: TurnResult,
}

#[derive(Debug, Clone, Serialize)]
pub struct Report {
    pub outcomes: Vec<TaskOutcome>,
}

/// Parse the embedded default suite.
pub fn default_suite() -> Suite {
    toml::from_str(DEFAULT_SUITE).expect("embedded default suite parses")
}

/// The embedded agent-behaviour suite (tool use, multi-turn, research).
pub fn agent_suite() -> Suite {
    toml::from_str(AGENT_SUITE).expect("embedded agent suite parses")
}

/// Load and merge every `*.toml` file in a directory into one suite.
pub fn load_suite_dir(dir: &std::path::Path) -> Result<Suite> {
    let mut tasks = Vec::new();
    let mut entries: Vec<_> = std::fs::read_dir(dir)
        .with_context(|| format!("reading suite dir {}", dir.display()))?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|x| x == "toml"))
        .collect();
    entries.sort();
    for path in entries {
        let text = std::fs::read_to_string(&path)
            .with_context(|| format!("reading {}", path.display()))?;
        let suite: Suite =
            toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
        tasks.extend(suite.tasks);
    }
    Ok(Suite { tasks })
}

/// Run one turn end-to-end: send `text`, drain events for `session_id` until
/// the turn completes or fails, and capture the metrics.
async fn run_turn(
    client: &Client,
    session_id: i64,
    text: &str,
    tier: Option<&str>,
) -> Result<TurnResult> {
    // Subscribe before sending so no ToolStarted/TurnCompleted is missed.
    let mut stream = client.events().await?;
    let start = std::time::Instant::now();
    client.send_message(session_id, text, tier).await?;

    let mut tools = Vec::new();
    let collect = async {
        while let Some(ev) = stream.next().await {
            match ev {
                Ok(Event::ToolStarted { session_id: s, tool, .. }) if s == session_id => {
                    tools.push(tool);
                }
                Ok(Event::TurnCompleted {
                    session_id: s,
                    text,
                    input_tokens,
                    output_tokens,
                    routed_model,
                }) if s == session_id => {
                    return TurnResult {
                        final_text: text,
                        input_tokens,
                        output_tokens,
                        tools: std::mem::take(&mut tools),
                        latency_ms: start.elapsed().as_millis(),
                        model: routed_model,
                        failed: None,
                    };
                }
                Ok(Event::TurnFailed { session_id: s, error }) if s == session_id => {
                    return TurnResult {
                        final_text: String::new(),
                        input_tokens: 0,
                        output_tokens: 0,
                        tools: std::mem::take(&mut tools),
                        latency_ms: start.elapsed().as_millis(),
                        model: None,
                        failed: Some(error),
                    };
                }
                _ => {}
            }
        }
        // Stream ended without a terminal event.
        TurnResult {
            final_text: String::new(),
            input_tokens: 0,
            output_tokens: 0,
            tools: std::mem::take(&mut tools),
            latency_ms: start.elapsed().as_millis(),
            model: None,
            failed: Some("event stream closed before turn completed".into()),
        }
    };

    match tokio::time::timeout(TURN_TIMEOUT, collect).await {
        Ok(r) => Ok(r),
        Err(_) => Ok(TurnResult {
            final_text: String::new(),
            input_tokens: 0,
            output_tokens: 0,
            tools: Vec::new(),
            latency_ms: start.elapsed().as_millis(),
            model: None,
            failed: Some(format!("timed out after {}s", TURN_TIMEOUT.as_secs())),
        }),
    }
}

/// Run a single task: fresh session, setup turns, then the graded turn.
pub async fn run_task(client: &Client, task: &EvalTask) -> Result<TaskOutcome> {
    let session_id = client.create_session(&format!("eval:{}", task.id)).await?;
    let tier = task.tier.as_deref();
    for s in &task.setup {
        // Priming turns are not graded; a failure here still surfaces via the
        // graded turn (e.g. memory not saved → recall misses).
        let _ = run_turn(client, session_id, s, tier).await?;
    }
    let result = run_turn(client, session_id, &task.prompt, tier).await?;
    let (passed, reason) = match task.grade.check(&result) {
        Ok(()) => (true, "ok".to_string()),
        Err(reason) => (false, reason),
    };
    Ok(TaskOutcome { id: task.id.clone(), tags: task.tags.clone(), passed, reason, result })
}

/// Run every task sequentially (clean, uncontended latency numbers).
pub async fn run_suite(client: &Client, suite: &Suite) -> Result<Report> {
    let mut outcomes = Vec::new();
    for task in &suite.tasks {
        outcomes.push(run_task(client, task).await?);
    }
    Ok(Report { outcomes })
}

/// The four fitness axes, each normalized to 0..1 (higher is better), plus a
/// weighted composite. This is the machine-readable expression of "do more,
/// faster, with less": accuracy + capability = *do more*; speed + cost =
/// *faster / with less*. The normalizations are heuristic and monotonic —
/// meant for RELATIVE comparison (before vs after a change, run over run), not
/// as absolute grades. Self-improvement aims at the weakest axis; release notes
/// and the materiality judge cite the deltas.
#[derive(Debug, Clone, Copy, Serialize)]
pub struct Scorecard {
    pub accuracy: f64,
    pub speed: f64,
    pub cost: f64,
    pub capability: f64,
    pub composite: f64,
}

impl Scorecard {
    /// The single weakest axis by score — what self-improvement should target.
    pub fn weakest_axis(&self) -> &'static str {
        let axes = [
            ("accuracy", self.accuracy),
            ("capability", self.capability),
            ("speed", self.speed),
            ("cost", self.cost),
        ];
        axes.iter().min_by(|a, b| a.1.total_cmp(&b.1)).map(|(n, _)| *n).unwrap_or("accuracy")
    }
}

impl Report {
    /// Compute the fitness scorecard. Empty reports score 0 across the board.
    pub fn scorecard(&self) -> Scorecard {
        let n = self.outcomes.len();
        if n == 0 {
            return Scorecard { accuracy: 0.0, speed: 0.0, cost: 0.0, capability: 0.0, composite: 0.0 };
        }
        // accuracy: fraction of graded tasks that passed.
        let accuracy = self.passed() as f64 / n as f64;

        // speed: monotonic in p50 latency (seconds). 1s→0.5, 3s→0.25, 0→1.
        let lat = self.latencies();
        let p50_s = Self::pctl(&lat, 50.0) as f64 / 1000.0;
        let speed = 1.0 / (1.0 + p50_s);

        // cost: monotonic in mean tokens/task (per 1k). 1k→0.5, 3k→0.25.
        let mean_tok = self.total_tokens() as f64 / n as f64;
        let cost = 1.0 / (1.0 + mean_tok / 1000.0);

        // capability: breadth — fraction of distinct tags with ≥1 passing task
        // (does it work ACROSS categories, not just on a few). Untagged suites
        // fall back to accuracy.
        let mut all_tags = std::collections::BTreeSet::new();
        let mut passed_tags = std::collections::BTreeSet::new();
        for o in &self.outcomes {
            for t in &o.tags {
                all_tags.insert(t.clone());
                if o.passed {
                    passed_tags.insert(t.clone());
                }
            }
        }
        let capability = if all_tags.is_empty() {
            accuracy
        } else {
            passed_tags.len() as f64 / all_tags.len() as f64
        };

        // Composite: "do more" (accuracy+capability) weighted above "faster/less"
        // (speed+cost). Correctness leads; efficiency refines.
        let composite = 0.40 * accuracy + 0.25 * capability + 0.20 * speed + 0.15 * cost;
        Scorecard { accuracy, speed, cost, capability, composite }
    }

    fn latencies(&self) -> Vec<u128> {
        let mut v: Vec<u128> = self.outcomes.iter().map(|o| o.result.latency_ms).collect();
        v.sort_unstable();
        v
    }

    /// Percentile (nearest-rank) over per-task latency in ms.
    fn pctl(sorted: &[u128], p: f64) -> u128 {
        if sorted.is_empty() {
            return 0;
        }
        let rank = ((p / 100.0) * sorted.len() as f64).ceil() as usize;
        sorted[rank.saturating_sub(1).min(sorted.len() - 1)]
    }

    pub fn passed(&self) -> usize {
        self.outcomes.iter().filter(|o| o.passed).count()
    }

    pub fn total_tokens(&self) -> u64 {
        self.outcomes.iter().map(|o| o.result.input_tokens + o.result.output_tokens).sum()
    }

    /// A compact JSON summary — for CI, dashboards, and trend tracking.
    pub fn json(&self) -> serde_json::Value {
        let lat = self.latencies();
        let n = self.outcomes.len().max(1);
        let sc = self.scorecard();
        serde_json::json!({
            "tasks": self.outcomes.len(),
            "passed": self.passed(),
            "pass_rate": self.passed() as f64 / n as f64,
            "fitness": {
                "accuracy": sc.accuracy,
                "speed": sc.speed,
                "cost": sc.cost,
                "capability": sc.capability,
                "composite": sc.composite,
                "weakest_axis": sc.weakest_axis(),
            },
            "latency_ms": {
                "p50": Self::pctl(&lat, 50.0),
                "p95": Self::pctl(&lat, 95.0),
                "mean": lat.iter().sum::<u128>() / n as u128,
            },
            "tokens": {
                "total": self.total_tokens(),
                "mean_per_task": self.total_tokens() / n as u64,
            },
            "outcomes": self.outcomes,
        })
    }

    /// A human scorecard for the terminal.
    pub fn markdown(&self) -> String {
        let lat = self.latencies();
        let n = self.outcomes.len().max(1);
        let mut out = String::new();
        out.push_str("# revenant eval scorecard\n\n");
        out.push_str("| task | result | latency | in→out tok | tools | model |\n");
        out.push_str("|------|--------|--------:|-----------:|------:|-------|\n");
        for o in &self.outcomes {
            let mark = if o.passed { "✅ pass" } else { "❌ FAIL" };
            let model = o.result.model.as_deref().unwrap_or("-");
            out.push_str(&format!(
                "| {} | {} | {} ms | {}→{} | {} | {} |\n",
                o.id,
                mark,
                o.result.latency_ms,
                o.result.input_tokens,
                o.result.output_tokens,
                o.result.tools.len(),
                model,
            ));
            if !o.passed {
                out.push_str(&format!("| ↳ | _{}_ |||||\n", o.reason));
            }
        }
        out.push_str(&format!(
            "\n**{}/{} passed** · latency p50 {} ms / p95 {} ms / mean {} ms · {} tokens total ({}/task)\n",
            self.passed(),
            self.outcomes.len(),
            Self::pctl(&lat, 50.0),
            Self::pctl(&lat, 95.0),
            lat.iter().sum::<u128>() / n as u128,
            self.total_tokens(),
            self.total_tokens() / n as u64,
        ));
        let sc = self.scorecard();
        out.push_str(&format!(
            "\n**fitness** (0–1, higher better) · accuracy {:.2} · capability {:.2} · speed {:.2} · cost {:.2} → **composite {:.2}** · weakest: _{}_\n",
            sc.accuracy, sc.capability, sc.speed, sc.cost, sc.composite, sc.weakest_axis(),
        ));
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn res(text: &str, tools: &[&str]) -> TurnResult {
        TurnResult {
            final_text: text.into(),
            input_tokens: 10,
            output_tokens: 5,
            tools: tools.iter().map(|s| s.to_string()).collect(),
            latency_ms: 100,
            model: Some("fast".into()),
            failed: None,
        }
    }

    fn outcome(id: &str, tags: &[&str], passed: bool) -> TaskOutcome {
        TaskOutcome {
            id: id.into(),
            tags: tags.iter().map(|s| s.to_string()).collect(),
            passed,
            reason: if passed { "ok".into() } else { "fail".into() },
            result: res("x", &[]),
        }
    }

    #[test]
    fn scorecard_axes_and_weakest() {
        // All pass, one tag → accuracy & capability perfect; weakest is a
        // perf axis. A failing task must drop accuracy below capability.
        let perfect = Report { outcomes: vec![outcome("a", &["mem"], true), outcome("b", &["mem"], true)] };
        let sc = perfect.scorecard();
        assert!((sc.accuracy - 1.0).abs() < 1e-9);
        assert!((sc.capability - 1.0).abs() < 1e-9);
        assert!(sc.composite > 0.0 && sc.composite <= 1.0);

        let mixed = Report {
            outcomes: vec![outcome("a", &["mem"], true), outcome("b", &["speed"], false)],
        };
        let m = mixed.scorecard();
        assert!(m.accuracy < 1.0, "a failing task must lower accuracy");
        // One of two tags still has a pass → capability 0.5.
        assert!((m.capability - 0.5).abs() < 1e-9);

        // Empty report is all-zero, no panic.
        let empty = Report { outcomes: vec![] };
        assert_eq!(empty.scorecard().composite, 0.0);
    }

    #[test]
    fn embedded_suite_parses_and_is_nonempty() {
        let s = default_suite();
        assert!(!s.tasks.is_empty());
        // Every task must carry a graded id.
        assert!(s.tasks.iter().all(|t| !t.id.is_empty()));
    }

    #[test]
    fn grader_contains_is_case_insensitive() {
        let g = GradeSpec { contains: vec!["Nightjar".into()], ..Default::default() };
        assert!(g.check(&res("codename is nightjar", &[])).is_ok());
        assert!(g.check(&res("no match here", &[])).is_err());
    }

    #[test]
    fn grader_regex_and_tools() {
        let g = GradeSpec {
            regex: Some(r"2\s*,\s*3\s*,\s*5".into()),
            min_tools: Some(1),
            tool_used: Some("memory_save".into()),
            ..Default::default()
        };
        assert!(g.check(&res("2, 3, 5", &["memory_save"])).is_ok());
        assert!(g.check(&res("2, 3, 5", &[])).is_err()); // no tools
        assert!(g.check(&res("wrong", &["memory_save"])).is_err()); // regex
    }

    #[test]
    fn grader_flags_failed_turn() {
        let mut r = res("anything", &[]);
        r.failed = Some("boom".into());
        assert!(GradeSpec::default().check(&r).is_err());
    }

    #[test]
    fn report_aggregates() {
        let report = Report {
            outcomes: vec![
                TaskOutcome {
                    id: "a".into(),
                    tags: vec![],
                    passed: true,
                    reason: "ok".into(),
                    result: res("x", &[]),
                },
                TaskOutcome {
                    id: "b".into(),
                    tags: vec![],
                    passed: false,
                    reason: "nope".into(),
                    result: res("y", &[]),
                },
            ],
        };
        assert_eq!(report.passed(), 1);
        assert_eq!(report.total_tokens(), 30);
        assert!(report.markdown().contains("1/2 passed"));
        assert_eq!(report.json()["pass_rate"], 0.5);
    }
}
