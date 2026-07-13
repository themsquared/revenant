//! Minor / major classification for network promotion.
//!
//! A proven change is either MINOR — a pure performance/quality win the network
//! can vouch for and auto-promote — or MAJOR — new capability or anything near
//! the safety surface, which the owner must decide. This module makes that call
//! from what we can see locally (changed files + the diff text + the eval
//! verdict), and it is deliberately **conservative and wards-first**: any doubt,
//! any warded path, any sensitive token → MAJOR.
//!
//! The reason it can't just trust the eval delta: evals measure task outcomes,
//! not safety. A diff can improve the composite while quietly loosening
//! approval-gating or widening a permission. So the wards/sensitive fence
//! overrides the metric win, always.
//!
//! Phase 1: this classifies and explains. It is NOT yet wired to any promote
//! path — see docs/network-promotion.md.

use crate::Verdict;
use serde::Serialize;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ChangeClass {
    /// Network-quorum eligible: proven, small, capability-neutral, wards clear.
    Minor,
    /// Owner decides: new capability, user-visible behavior, or near the safety
    /// surface. Never auto-promotes.
    Major,
}

#[derive(Debug, Clone, Serialize)]
pub struct Classification {
    pub class: ChangeClass,
    /// Human-readable reasons for the verdict (why minor, or what forced major).
    pub reasons: Vec<String>,
    /// If a warded/sensitive path forced MAJOR, the first offending path.
    pub ward_hit: Option<String>,
}

impl Classification {
    pub fn is_minor(&self) -> bool {
        self.class == ChangeClass::Minor
    }
}

/// Knobs for the classifier (kept explicit so it's testable and tunable).
#[derive(Debug, Clone)]
pub struct ClassifyOpts {
    /// Max changed files a MINOR change may touch. Bigger ⇒ MAJOR (broad blast
    /// radius warrants a human even if the metric looks good).
    pub max_minor_files: usize,
}

impl Default for ClassifyOpts {
    fn default() -> Self {
        ClassifyOpts { max_minor_files: 5 }
    }
}

/// Path fragments that mark the safety/capability surface. A changed path
/// containing any of these forces MAJOR even outside the crate-level denylist
/// (much of this surface lives in revenant-core, which is not fully warded).
const SENSITIVE_PATHS: &[&str] = &[
    "permission",
    "approval",
    "security",
    "privacy",
    "secret",
    "auth",
    "/tool.rs",
    "tools/src/builtins", // where tool risk tiers live
    "config.rs",          // config surface: keys, tiers, autonomy
    "providers.rs",       // provider/key routing
    "identity",
];

/// Diff tokens that signal a capability or safety change regardless of which
/// file they land in. Presence forces MAJOR.
const DANGER_TOKENS: &[&str] = &[
    "PermissionTier",
    "fn risk(",
    "Dangerous",
    "api_key",
    "secrets",
    "ANTHROPIC_API_KEY",
    "unsafe ",
    "Command::new",         // new process execution
    "reqwest::",            // new network egress
    "register_tool!",       // a new tool = new capability
    "tool_choice",
    "std::process",
];

/// Classify a proven change. `changed` are repo-relative paths; `diff` is the
/// unified diff text (may be empty if unavailable — then only paths + verdict
/// are used, which biases further toward MAJOR by design); `denylist` are the
/// Ascension wards; `verdict` is the eval judgement.
pub fn classify(
    changed: &[String],
    diff: &str,
    denylist: &[String],
    verdict: &Verdict,
    opts: &ClassifyOpts,
) -> Classification {
    let mut reasons = Vec::new();

    // 1. Wards — the hard fence. Any warded path ⇒ MAJOR, no exceptions.
    for file in changed {
        for deny in denylist {
            if file.starts_with(deny) {
                return major(
                    format!("touches warded path {file} (ward {deny})"),
                    Some(file.clone()),
                );
            }
        }
    }

    // 2. Sensitive surface — path fragments near safety/capability.
    for file in changed {
        let lower = file.to_lowercase();
        if let Some(hit) = SENSITIVE_PATHS.iter().find(|p| lower.contains(*p)) {
            return major(
                format!("touches sensitive surface {file} (matched \"{hit}\")"),
                Some(file.clone()),
            );
        }
    }

    // 3. Diff danger tokens — capability/safety signals wherever they appear.
    for tok in DANGER_TOKENS {
        if diff.contains(tok) {
            return major(format!("diff introduces \"{tok}\" (capability/safety signal)"), None);
        }
    }

    // 4. A regression anywhere ⇒ never minor (should already be rejected
    //    upstream by `evaluate`, but defence in depth).
    if !verdict.regressions.is_empty() {
        return major(format!("verdict has regressions: {:?}", verdict.regressions), None);
    }

    // 5. Blast radius — too many files for an unattended promote.
    if changed.len() > opts.max_minor_files {
        return major(
            format!("{} files changed (> {} minor cap)", changed.len(), opts.max_minor_files),
            None,
        );
    }

    // 6. No changed files at all is not a promotable improvement.
    if changed.is_empty() {
        return major("no changed files".into(), None);
    }

    // Passed every fence: a small, capability-neutral, wards-clear win.
    reasons.push(format!("{} file(s) changed, all clear of wards", changed.len()));
    reasons.push("no sensitive paths or capability/safety tokens in the diff".into());
    if !verdict.fixed_tasks.is_empty() {
        reasons.push(format!("fixes {} eval task(s)", verdict.fixed_tasks.len()));
    }
    if verdict.latency_delta_pct.abs() > f64::EPSILON || verdict.token_delta_pct.abs() > f64::EPSILON
    {
        reasons.push(format!(
            "metric win: latency Δ {:.1}%, tokens Δ {:.1}%",
            verdict.latency_delta_pct, verdict.token_delta_pct
        ));
    }
    Classification { class: ChangeClass::Minor, reasons, ward_hit: None }
}

fn major(reason: String, ward_hit: Option<String>) -> Classification {
    Classification { class: ChangeClass::Major, reasons: vec![reason], ward_hit }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wards() -> Vec<String> {
        vec![
            "crates/revenant-security".into(),
            "crates/revenant-gateway".into(),
            "crates/revenant-ascension".into(),
            ".github/".into(),
        ]
    }

    fn clean_verdict() -> Verdict {
        Verdict {
            accepted: true,
            fixed_tasks: vec!["t1".into()],
            regressions: vec![],
            reasons: vec![],
            latency_delta_pct: 12.0,
            token_delta_pct: 3.0,
        }
    }

    #[test]
    fn small_clean_perf_win_is_minor() {
        let changed = vec!["crates/revenant-agent/src/lib.rs".into()];
        let c = classify(&changed, "let x = faster(); // tighten loop", &wards(), &clean_verdict(), &ClassifyOpts::default());
        assert!(c.is_minor(), "expected minor, got {c:?}");
    }

    #[test]
    fn warded_path_forces_major() {
        let changed = vec!["crates/revenant-security/src/lib.rs".into()];
        let c = classify(&changed, "", &wards(), &clean_verdict(), &ClassifyOpts::default());
        assert_eq!(c.class, ChangeClass::Major);
        assert!(c.ward_hit.is_some());
    }

    #[test]
    fn sensitive_path_forces_major_even_outside_wards() {
        // revenant-core is NOT warded, but config.rs is sensitive surface.
        let changed = vec!["crates/revenant-core/src/config.rs".into()];
        let c = classify(&changed, "", &wards(), &clean_verdict(), &ClassifyOpts::default());
        assert_eq!(c.class, ChangeClass::Major);
    }

    #[test]
    fn permission_tier_token_in_diff_forces_major() {
        let changed = vec!["crates/revenant-agent/src/lib.rs".into()];
        let diff = "if tool.risk(&input) >= PermissionTier::Dangerous { allow() }";
        let c = classify(&changed, diff, &wards(), &clean_verdict(), &ClassifyOpts::default());
        assert_eq!(c.class, ChangeClass::Major);
    }

    #[test]
    fn new_tool_registration_forces_major() {
        let changed = vec!["crates/revenant-agent/src/lib.rs".into()];
        let c = classify(&changed, "register_tool!(MyNewThing::new());", &wards(), &clean_verdict(), &ClassifyOpts::default());
        assert_eq!(c.class, ChangeClass::Major);
    }

    #[test]
    fn broad_diff_forces_major() {
        let changed: Vec<String> =
            (0..9).map(|i| format!("crates/revenant-agent/src/m{i}.rs")).collect();
        let c = classify(&changed, "tweak", &wards(), &clean_verdict(), &ClassifyOpts::default());
        assert_eq!(c.class, ChangeClass::Major);
    }

    #[test]
    fn regression_forces_major() {
        let changed = vec!["crates/revenant-agent/src/lib.rs".into()];
        let mut v = clean_verdict();
        v.regressions = vec!["t2".into()];
        let c = classify(&changed, "tweak", &wards(), &v, &ClassifyOpts::default());
        assert_eq!(c.class, ChangeClass::Major);
    }

    #[test]
    fn empty_change_is_major() {
        let c = classify(&[], "", &wards(), &clean_verdict(), &ClassifyOpts::default());
        assert_eq!(c.class, ChangeClass::Major);
    }
}
