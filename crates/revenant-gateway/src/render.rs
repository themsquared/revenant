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
            let mut targets = Vec::new();
            for (idx, target) in usable.iter().enumerate() {
                let internal_name = format!("{tier}/{idx}");
                models.push(render_model(&internal_name, target, true)?);
                targets.push(json!({ "model": internal_name, "priority": idx }));
            }
            virtual_models.push(json!({
                "name": tier,
                "routing": { "failover": { "targets": targets } },
            }));
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

    let doc = json!({
        "config": {
            "readinessAddr": format!("127.0.0.1:{}", cfg.gateway.readiness_port),
            "statsAddr": format!("127.0.0.1:{}", cfg.gateway.stats_port),
        },
        "llm": Value::Object(llm),
    });

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
                }],
            },
        );
        let yaml = render_gateway_yaml(&cfg, &env).unwrap();
        assert!(yaml.contains("name: local"));
        assert!(!yaml.contains("anthropic"));
    }
}
