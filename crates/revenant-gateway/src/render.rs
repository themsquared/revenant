//! Harness tiers → agentgateway YAML.
//!
//! Single-target tiers render as a plain model alias; multi-target tiers
//! render internal models plus a `virtualModels` entry with priority
//! failover (target order = priority order). API keys are always `$ENV`
//! references — agentgateway expands them from the child environment, so
//! no secret ever lands in the YAML.

use anyhow::{bail, Result};
use revenant_core::config::Config;
use serde_json::{json, Map, Value};
use std::collections::HashSet;

/// `available_env` holds the env var names that will actually be present in
/// the gateway's environment. Targets referencing a missing key are dropped
/// (with a loud warning) so a fresh install without cloud keys still serves
/// whatever tiers it can — e.g. `local` via Ollama.
pub fn render_gateway_yaml(cfg: &Config, available_env: &HashSet<String>) -> Result<String> {
    let mut models: Vec<Value> = Vec::new();
    let mut virtual_models: Vec<Value> = Vec::new();

    for (tier, tier_cfg) in &cfg.tiers {
        if tier_cfg.targets.is_empty() {
            bail!("tier '{tier}' has no targets");
        }
        let usable: Vec<_> = tier_cfg
            .targets
            .iter()
            .filter(|t| match &t.api_key_env {
                Some(env) if !available_env.contains(env) => {
                    tracing::warn!(
                        "tier '{tier}': dropping target {} — ${env} not set in secrets.env",
                        t.model
                    );
                    false
                }
                _ => true,
            })
            .collect();
        if usable.is_empty() {
            tracing::warn!("tier '{tier}' has no usable targets (missing API keys) — skipped");
            continue;
        }
        if usable.len() == 1 {
            models.push(render_model(tier, usable[0], false)?);
        } else {
            use revenant_core::config::RouteStrategy;
            let weighted = tier_cfg.strategy == RouteStrategy::Weighted;
            let mut targets = Vec::new();
            for (idx, target) in usable.iter().enumerate() {
                let internal_name = format!("{tier}/{idx}");
                let mut model = render_model(&internal_name, target, true)?;
                if weighted {
                    // Weighted split across providers for cost/quality balance;
                    // agentgateway distributes traffic by relative weight.
                    targets.push(
                        json!({ "model": internal_name, "weight": target.weight.unwrap_or(1) }),
                    );
                } else {
                    // Failover members get outlier detection: a target answering
                    // with "I am broken" codes (auth/quota/missing model/5xx) is
                    // evicted so the virtual model routes to the next priority.
                    // The harness retries the turn once to ride the eviction.
                    model.as_object_mut().unwrap().insert(
                        "health".into(),
                        json!({
                            "unhealthyExpression":
                                "response.code >= 500 || response.code == 429 || response.code == 401 || response.code == 403 || response.code == 404",
                            "eviction": { "duration": "60s" },
                        }),
                    );
                    targets.push(json!({ "model": internal_name, "priority": idx }));
                }
                models.push(model);
            }
            let routing = if weighted {
                json!({ "weighted": { "targets": targets } })
            } else {
                json!({ "failover": { "targets": targets } })
            };
            virtual_models.push(json!({ "name": tier, "routing": routing }));
        }
    }
    if models.is_empty() {
        bail!("no tier has a usable target — add API keys to secrets.env or configure a local tier");
    }

    let mut llm = Map::new();
    llm.insert("port".into(), json!(cfg.gateway.llm_port));
    llm.insert("models".into(), Value::Array(models));
    if !virtual_models.is_empty() {
        llm.insert("virtualModels".into(), Value::Array(virtual_models));
    }
    // Global spend cap: a token bucket the gateway enforces on the LLM
    // listener. Because it lives below the agent, the ceiling holds no matter
    // what the harness does — the moat property made literal. Bucket capacity
    // and refill are both `budget` per `interval` → a rolling cap.
    if cfg.spending.enabled {
        llm.insert(
            "policies".into(),
            json!({
                "localRateLimit": [{
                    "maxTokens": cfg.spending.budget,
                    "tokensPerFill": cfg.spending.budget,
                    "fillInterval": cfg.spending.interval,
                    "type": cfg.spending.count.gateway_type(),
                }]
            }),
        );
    }

    let mut doc = json!({
        "config": {
            "readinessAddr": format!("127.0.0.1:{}", cfg.gateway.readiness_port),
            "statsAddr": format!("127.0.0.1:{}", cfg.gateway.stats_port),
        },
        "llm": Value::Object(llm),
    });

    // MCP plugin bus: one gateway endpoint multiplexing every configured MCP
    // server (stdio-spawned or remote). Namespaced + governable by the gateway.
    if !cfg.mcp.is_empty() {
        let targets: Vec<Value> = cfg
            .mcp
            .iter()
            .map(|s| {
                let mut t = Map::new();
                t.insert("name".into(), json!(s.name));
                match (&s.cmd, &s.url) {
                    (Some(cmd), _) => {
                        t.insert(
                            "stdio".into(),
                            json!({ "cmd": cmd, "args": s.args }),
                        );
                    }
                    (None, Some(url)) => {
                        t.insert("mcp".into(), json!({ "host": url }));
                    }
                    (None, None) => {}
                }
                Value::Object(t)
            })
            .collect();
        doc.as_object_mut().unwrap().insert(
            "mcp".into(),
            json!({ "port": cfg.gateway.mcp_port, "targets": targets }),
        );
    }

    // Governed A2A egress: one gateway bind per gateway-routed remote agent.
    // Marking the route `a2a: {}` enables A2A processing, telemetry, and the
    // hook for authz/guardrails/rate-limits — the first law extended to
    // agent-to-agent traffic. `direct` agents are skipped (substrate governs).
    let mut binds: Vec<Value> = Vec::new();
    for (idx, agent) in cfg.a2a_agents.iter().enumerate() {
        if agent.direct {
            continue;
        }
        let Some((scheme, host, _path)) = revenant_core::config::parse_endpoint(&agent.url) else {
            bail!("a2a agent '{}' has an unparseable url: {}", agent.name, agent.url);
        };
        let mut backend = Map::new();
        backend.insert("host".into(), json!(host));
        if scheme == "https" {
            backend.insert("backendTLS".into(), json!({}));
        }
        binds.push(json!({
            "port": cfg.gateway.a2a_egress_base + idx as u16,
            "listeners": [{
                "routes": [{
                    "policies": { "a2a": {} },
                    "backends": [Value::Object(backend)]
                }]
            }]
        }));
    }
    if !binds.is_empty() {
        doc.as_object_mut().unwrap().insert("binds".into(), Value::Array(binds));
    }

    let yaml = serde_yaml::to_string(&doc)?;
    Ok(format!(
        "# Rendered by revenant — DO NOT EDIT. Source of truth: ~/.revenant/config.toml\n{yaml}"
    ))
}

fn render_model(
    name: &str,
    target: &revenant_core::config::TierTarget,
    internal: bool,
) -> Result<Value> {
    let mut params = Map::new();
    params.insert("model".into(), json!(target.model));
    if let Some(env) = &target.api_key_env {
        params.insert("apiKey".into(), json!(format!("${env}")));
    }
    if let Some(base) = &target.base_url {
        params.insert("baseUrl".into(), json!(base));
    }

    let mut model = Map::new();
    model.insert("name".into(), json!(name));
    model.insert("provider".into(), json!(target.provider.gateway_name()));
    model.insert("params".into(), Value::Object(params));
    if internal {
        model.insert("visibility".into(), json!("internal"));
    }
    // Provider prompt caching: the gateway inserts cache markers on the
    // system prompt / tools / message prefix. The harness keeps its prompt
    // layers byte-stable in stability order precisely so these hit.
    if matches!(
        target.provider,
        revenant_core::config::Provider::Anthropic | revenant_core::config::Provider::Bedrock
    ) {
        model.insert(
            "promptCaching".into(),
            json!({ "cacheSystem": true, "cacheTools": true, "cacheMessages": true }),
        );
    }
    Ok(Value::Object(model))
}

#[cfg(test)]
mod tests {
    use super::*;
    use revenant_core::config::Config;

    #[test]
    fn default_config_renders() {
        let env: HashSet<String> = ["ANTHROPIC_API_KEY".to_string()].into();
        let yaml = render_gateway_yaml(&Config::default_config(), &env).unwrap();
        // Balanced is multi-target → virtual model with failover.
        assert!(yaml.contains("virtualModels"));
        assert!(yaml.contains("name: balanced"));
        assert!(yaml.contains("balanced/0"));
        assert!(yaml.contains("visibility: internal"));
        // Secrets are env refs, never literals.
        assert!(yaml.contains("apiKey: $ANTHROPIC_API_KEY"));
        // Single-target tiers are plain aliases.
        assert!(yaml.contains("name: fast"));
        assert!(yaml.contains("name: deep"));
    }

    #[test]
    fn missing_keys_drop_tiers_not_config() {
        // No keys at all → cloud tiers are skipped; config errors only when
        // NOTHING is usable.
        let env = HashSet::new();
        let err = render_gateway_yaml(&Config::default_config(), &env).unwrap_err();
        assert!(err.to_string().contains("no tier has a usable target"));

        let mut cfg = Config::default_config();
        cfg.tiers.insert(
            "local".into(),
            revenant_core::config::TierConfig {
                targets: vec![revenant_core::config::TierTarget {
                    provider: revenant_core::config::Provider::Ollama,
                    model: "qwen3:0.6b".into(),
                    api_key_env: None,
                    base_url: None,
                    weight: None,
                }],
                strategy: revenant_core::config::RouteStrategy::Failover,
            },
        );
        let yaml = render_gateway_yaml(&cfg, &env).unwrap();
        assert!(yaml.contains("name: local"));
        assert!(!yaml.contains("anthropic"));
    }

    #[test]
    fn weighted_tier_renders_weighted_routing() {
        use revenant_core::config::{Provider, RouteStrategy, TierConfig, TierTarget};
        let env: HashSet<String> =
            ["ANTHROPIC_API_KEY".to_string(), "OPENAI_API_KEY".to_string()].into();
        let mut cfg = Config::default_config();
        cfg.tiers.insert(
            "balanced".into(),
            TierConfig {
                strategy: RouteStrategy::Weighted,
                targets: vec![
                    TierTarget {
                        provider: Provider::OpenAI,
                        model: "gpt-4o-mini".into(),
                        api_key_env: Some("OPENAI_API_KEY".into()),
                        base_url: None,
                        weight: Some(70),
                    },
                    TierTarget {
                        provider: Provider::Anthropic,
                        model: "claude-sonnet-5".into(),
                        api_key_env: Some("ANTHROPIC_API_KEY".into()),
                        base_url: None,
                        weight: Some(30),
                    },
                ],
            },
        );
        let yaml = render_gateway_yaml(&cfg, &env).unwrap();
        assert!(yaml.contains("weighted"));
        assert!(yaml.contains("weight: 70"));
        assert!(yaml.contains("weight: 30"));
        // Weighted members carry no failover priority/health.
        assert!(!yaml.contains("priority:"));
    }

    #[test]
    fn spending_cap_renders_local_rate_limit() {
        use revenant_core::config::BudgetCount;
        let env: HashSet<String> = ["ANTHROPIC_API_KEY".to_string()].into();
        let mut cfg = Config::default_config();
        cfg.spending.enabled = true;
        cfg.spending.budget = 500_000;
        cfg.spending.interval = "24h".into();
        cfg.spending.count = BudgetCount::Tokens;
        let yaml = render_gateway_yaml(&cfg, &env).unwrap();
        assert!(yaml.contains("localRateLimit"));
        assert!(yaml.contains("maxTokens: 500000"));
        assert!(yaml.contains("fillInterval: 24h"));
        assert!(yaml.contains("type: tokens"));
        // Off by default → no policies block.
        let off = render_gateway_yaml(&Config::default_config(), &env).unwrap();
        assert!(!off.contains("localRateLimit"));
    }
}
