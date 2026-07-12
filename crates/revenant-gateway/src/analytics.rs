//! Thin client for the gateway's request-log analytics API (admin port).
//!
//! Phase 1 of "gateway as control plane": the gateway persists every LLM
//! request (tokens, cost, latency, model, provider, identity) into its
//! request-log DB and serves aggregates at `/api/logs/analytics/summary`. This
//! is the authoritative, un-fakeable view of spend — computed below the harness
//! — so `revenant spend` reads it back here rather than trusting only the
//! harness's own bookkeeping. Fails soft: if the gateway or DB isn't up, the
//! caller degrades to local numbers.

use anyhow::{bail, Context, Result};
use serde_json::json;

/// One aggregation bucket (e.g. per provider or per model).
pub struct GroupStat {
    pub label: String,
    pub requests: i64,
    pub total_tokens: i64,
    pub cost: f64,
}

pub struct AnalyticsSummary {
    pub groups: Vec<GroupStat>,
    pub window_from: String,
    pub window_to: String,
}

impl AnalyticsSummary {
    /// (requests, tokens, cost) summed across groups.
    pub fn totals(&self) -> (i64, i64, f64) {
        self.groups.iter().fold((0, 0, 0.0), |(r, t, c), g| {
            (r + g.requests, t + g.total_tokens, c + g.cost)
        })
    }
}

/// Query the gateway analytics summary grouped by `field` ("provider",
/// "requestModel", "responseModel", "httpStatus"), over the default window
/// (the gateway defaults to the last 24h when no timeRange is given).
///
/// Returns a clear error when the admin API is unreachable or the request-log
/// DB isn't configured — callers surface that as "gateway analytics
/// unavailable" rather than failing the whole command.
pub async fn analytics_summary(admin_port: u16, field: &str) -> Result<AnalyticsSummary> {
    let url = format!("http://127.0.0.1:{admin_port}/api/logs/analytics/summary");
    let body = json!({ "groupBy": [ { "field": field } ] });
    let resp = reqwest::Client::new()
        .post(&url)
        .json(&body)
        .send()
        .await
        .context("reaching the gateway analytics API (is `revenant up` running?)")?;
    if !resp.status().is_success() {
        let code = resp.status();
        let msg = resp.text().await.unwrap_or_default();
        bail!("gateway analytics returned {code}: {}", msg.trim().trim_matches('"'));
    }
    let v: serde_json::Value = resp.json().await.context("parsing analytics response")?;
    let mut groups = Vec::new();
    if let Some(arr) = v["groups"].as_array() {
        for g in arr {
            // `group` is a single-key map like {"provider":"anthropic"}; the
            // value is our label (stringify non-string values defensively).
            let label = g["group"]
                .as_object()
                .and_then(|m| m.values().next())
                .map(|x| x.as_str().map(String::from).unwrap_or_else(|| x.to_string()))
                .unwrap_or_else(|| "?".to_string());
            groups.push(GroupStat {
                label,
                requests: g["requests"].as_i64().unwrap_or(0),
                total_tokens: g["totalTokens"].as_i64().unwrap_or(0),
                cost: g["cost"].as_f64().unwrap_or(0.0),
            });
        }
    }
    // Highest spend first.
    groups.sort_by(|a, b| b.cost.total_cmp(&a.cost).then(b.total_tokens.cmp(&a.total_tokens)));
    Ok(AnalyticsSummary {
        groups,
        window_from: v["timeRange"]["from"].as_str().unwrap_or("").to_string(),
        window_to: v["timeRange"]["to"].as_str().unwrap_or("").to_string(),
    })
}
