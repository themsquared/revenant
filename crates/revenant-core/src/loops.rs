//! Loop schedule parsing — shared by the tools (validate at create) and the
//! scheduler (compute next fire). Lives in core so neither side needs a
//! cross-crate dependency on the other.

use anyhow::{bail, Context, Result};

/// Floor on how often a loop may fire — a hard rail even for the agent.
pub const MIN_INTERVAL_S: i64 = 60;
/// Cap on total loops.
pub const MAX_LOOPS: usize = 20;

/// `every:<n>s` or `cron:<expr>`. Cron is boxed — it's much larger than the
/// interval variant.
pub enum Schedule {
    Every(i64),
    Cron(Box<croner::Cron>),
}

impl Schedule {
    pub fn parse(spec: &str) -> Result<Schedule> {
        if let Some(rest) = spec.strip_prefix("every:") {
            let secs: i64 = rest
                .trim()
                .trim_end_matches('s')
                .parse()
                .with_context(|| format!("bad interval '{spec}'"))?;
            if secs < MIN_INTERVAL_S {
                bail!("interval {secs}s is below the {MIN_INTERVAL_S}s floor");
            }
            Ok(Schedule::Every(secs))
        } else if let Some(rest) = spec.strip_prefix("cron:") {
            let cron = croner::Cron::new(rest.trim())
                .parse()
                .with_context(|| format!("bad cron '{spec}'"))?;
            Ok(Schedule::Cron(Box::new(cron)))
        } else {
            bail!("schedule must be 'every:<n>s' or 'cron:<expr>', got '{spec}'")
        }
    }

    /// Next fire time (unix seconds) strictly after `after`.
    pub fn next_after(&self, after: i64) -> Result<i64> {
        match self {
            Schedule::Every(secs) => Ok(after + secs),
            Schedule::Cron(cron) => {
                let base =
                    chrono::DateTime::from_timestamp(after, 0).unwrap_or_else(chrono::Utc::now);
                Ok(cron
                    .find_next_occurrence(&base, false)
                    .context("cron has no next occurrence")?
                    .timestamp())
            }
        }
    }
}

/// Validate a schedule and return the first next_run from `now`.
pub fn first_next_run(spec: &str, now: i64) -> Result<i64> {
    Schedule::parse(spec)?.next_after(now)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn floor_and_parse() {
        assert!(Schedule::parse("every:30s").is_err());
        assert_eq!(Schedule::parse("every:600s").unwrap().next_after(1000).unwrap(), 1600);
        assert!(Schedule::parse("nonsense").is_err());
        let c = Schedule::parse("cron:0 * * * *").unwrap().next_after(0).unwrap();
        assert!(c > 0 && c <= 3600);
    }
}
