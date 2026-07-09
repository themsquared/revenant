//! Harness configuration (`~/.revenant/config.toml`).

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    pub gateway: GatewayConfig,
    #[serde(default)]
    pub agent: AgentConfig,
    /// Tier name ("fast"/"balanced"/"deep"/"local") -> targets.
    pub tiers: BTreeMap<String, TierConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GatewayConfig {
    #[serde(default)]
    pub mode: GatewayMode,
    /// Pinned agentgateway release version (without the `v` prefix).
    pub version: String,
    #[serde(default = "default_llm_port")]
    pub llm_port: u16,
    #[serde(default = "default_readiness_port")]
    pub readiness_port: u16,
    #[serde(default = "default_stats_port")]
    pub stats_port: u16,
    /// Explicit path to an agentgateway binary (dev override). When unset,
    /// the pinned release is downloaded into the home dir.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub binary: Option<PathBuf>,
    /// External-gateway mode: base URL of an already-running gateway's LLM
    /// endpoint. Supervision and config rendering are disabled.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub endpoint: Option<String>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum GatewayMode {
    #[default]
    Bundled,
    External,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentConfig {
    #[serde(default = "default_tier")]
    pub default_tier: String,
    #[serde(default = "default_max_history")]
    pub max_history_messages: usize,
    #[serde(default = "default_max_tokens")]
    pub max_tokens: u32,
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            default_tier: default_tier(),
            max_history_messages: default_max_history(),
            max_tokens: default_max_tokens(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TierConfig {
    pub targets: Vec<TierTarget>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TierTarget {
    pub provider: Provider,
    pub model: String,
    /// Name of the env var (from secrets.env) holding the provider API key.
    /// Rendered into gateway YAML as a `$VAR` reference — never a literal.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key_env: Option<String>,
    /// Base URL override (e.g. a remote Ollama).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
}

/// Providers we render into agentgateway config. Serialized here in
/// lowercase for config.toml; `gateway_name()` gives the agentgateway
/// provider key.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Provider {
    Anthropic,
    OpenAI,
    Gemini,
    Vertex,
    Bedrock,
    Azure,
    Ollama,
    OpenRouter,
    Groq,
}

impl Provider {
    pub fn gateway_name(&self) -> &'static str {
        match self {
            Provider::Anthropic => "anthropic",
            Provider::OpenAI => "openAI",
            Provider::Gemini => "gemini",
            Provider::Vertex => "vertex",
            Provider::Bedrock => "bedrock",
            Provider::Azure => "azure",
            Provider::Ollama => "ollama",
            Provider::OpenRouter => "openrouter",
            Provider::Groq => "groq",
        }
    }
}

fn default_llm_port() -> u16 {
    41001
}
fn default_readiness_port() -> u16 {
    19001
}
fn default_stats_port() -> u16 {
    19002
}
fn default_tier() -> String {
    "balanced".to_string()
}
fn default_max_history() -> usize {
    50
}
fn default_max_tokens() -> u32 {
    8192
}

impl Config {
    pub fn from_toml(s: &str) -> Result<Self, toml::de::Error> {
        toml::from_str(s)
    }

    pub fn to_toml(&self) -> String {
        toml::to_string_pretty(self).expect("config serializes")
    }

    /// The default config written by `revenant init`.
    pub fn default_config() -> Self {
        let anthropic = |model: &str| TierTarget {
            provider: Provider::Anthropic,
            model: model.to_string(),
            api_key_env: Some("ANTHROPIC_API_KEY".to_string()),
            base_url: None,
        };
        let mut tiers = BTreeMap::new();
        tiers.insert(
            "fast".into(),
            TierConfig { targets: vec![anthropic("claude-haiku-4-5-20251001")] },
        );
        tiers.insert(
            "balanced".into(),
            TierConfig {
                targets: vec![
                    anthropic("claude-sonnet-5"),
                    anthropic("claude-haiku-4-5-20251001"),
                ],
            },
        );
        tiers.insert(
            "deep".into(),
            TierConfig { targets: vec![anthropic("claude-opus-4-8")] },
        );
        Config {
            gateway: GatewayConfig {
                mode: GatewayMode::Bundled,
                version: "1.3.1".to_string(),
                llm_port: default_llm_port(),
                readiness_port: default_readiness_port(),
                stats_port: default_stats_port(),
                binary: None,
                endpoint: None,
            },
            agent: AgentConfig::default(),
            tiers,
        }
    }
}
