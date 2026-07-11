//! Privacy router: detect sensitive data so a turn can be forced onto the
//! local tier and never leave the box. High-signal patterns only — the point
//! is to catch real secrets/PII without dragging every email to a local model
//! and tanking quality. Add stricter patterns via config.

use regex::RegexSet;

/// Compiled sensitive-data matcher. Cheap to run per turn.
pub struct Detector {
    set: RegexSet,
    labels: Vec<String>,
}

/// (label, pattern). High-severity by default: keys, tokens, private keys,
/// SSN, credit-card-ish. Email/phone are intentionally NOT default (too noisy
/// to route on) — add them via `[privacy].extra_patterns` for stricter modes.
const BUILTIN: &[(&str, &str)] = &[
    ("private key", r"-----BEGIN [A-Z ]*PRIVATE KEY-----"),
    ("AWS access key", r"AKIA[0-9A-Z]{16}"),
    ("provider API key", r"sk-[A-Za-z0-9_\-]{20,}"),
    ("GitHub token", r"gh[pousr]_[A-Za-z0-9]{20,}"),
    ("Slack token", r"xox[baprs]-[A-Za-z0-9\-]{10,}"),
    ("SSN", r"\b\d{3}-\d{2}-\d{4}\b"),
    ("credit card", r"\b(?:\d[ -]?){13,16}\b"),
    ("secret assignment", r#"(?i)(password|passwd|secret|api[_-]?key|token)\s*[:=]\s*\S+"#),
];

impl Detector {
    pub fn new(extra_patterns: &[String]) -> Self {
        let mut patterns: Vec<String> = Vec::new();
        let mut labels: Vec<String> = Vec::new();
        for (label, pat) in BUILTIN {
            patterns.push((*pat).to_string());
            labels.push((*label).to_string());
        }
        for (i, pat) in extra_patterns.iter().enumerate() {
            // Skip a bad custom regex rather than failing the whole detector.
            if regex::Regex::new(pat).is_ok() {
                patterns.push(pat.clone());
                labels.push(format!("custom pattern {}", i + 1));
            } else {
                tracing::warn!("privacy: ignoring invalid extra_pattern: {pat}");
            }
        }
        let set = RegexSet::new(&patterns).unwrap_or_else(|_| RegexSet::empty());
        Detector { set, labels }
    }

    /// The first sensitive category found in `text`, if any.
    pub fn scan(&self, text: &str) -> Option<String> {
        self.set
            .matches(text)
            .iter()
            .next()
            .and_then(|i| self.labels.get(i).cloned())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catches_real_secrets_not_prose() {
        let d = Detector::new(&[]);
        assert!(d.scan("my key is sk-abcdefghij0123456789XYZ").is_some());
        assert_eq!(d.scan("my SSN is 123-45-6789").as_deref(), Some("SSN"));
        assert!(d.scan("password = hunter2fortnite").is_some());
        assert!(d.scan("AKIAIOSFODNN7EXAMPLE is the key").is_some());
        // Ordinary prose must not trip it.
        assert!(d.scan("what's the weather in Portland today?").is_none());
        assert!(d.scan("summarize my notes from yesterday").is_none());
    }

    #[test]
    fn extra_patterns_and_bad_regex() {
        let d = Detector::new(&["PROJECT-[0-9]+".into(), "(".into()]);
        assert!(d.scan("ticket PROJECT-4210 is blocked").is_some());
    }
}
