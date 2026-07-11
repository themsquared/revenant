//! Harness configuration (`~/.revenant/config.toml`).

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    pub gateway: GatewayConfig,
    #[serde(default)]
    pub agent: AgentConfig,
    #[serde(default)]
    pub memory: MemoryConfig,
    #[serde(default)]
    pub channels: ChannelsConfig,
    #[serde(default)]
    pub privacy: PrivacyConfig,
    /// Global gateway-enforced spend cap (the intelligent-spending ceiling).
    #[serde(default)]
    pub spending: SpendingConfig,
    /// MCP servers multiplexed behind the gateway (the plugin bus).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub mcp: Vec<McpServer>,
    /// Remote A2A agents this agent can delegate to (the mesh, outbound).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub a2a_agents: Vec<A2aAgent>,
    /// Tier name ("fast"/"balanced"/"deep"/"local") -> targets.
    pub tiers: BTreeMap<String, TierConfig>,
}

/// An MCP server the gateway spawns/proxies. `cmd` = stdio server (spawned by
/// the gateway), or `url` = remote streamable-HTTP MCP endpoint.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpServer {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cmd: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub args: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
}

/// A remote A2A agent revenant can call via the mesh. `token_env` names an
/// env var holding a bearer token, if the peer requires one.
///
/// By default the call is proxied THROUGH the gateway (governed egress — the
/// first law extended to agents: authz, guardrails, telemetry, audit apply).
/// Set `direct = true` ONLY when revenant runs inside a substrate that already
/// governs the mesh (e.g. kagent), where a clean direct call is appropriate.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct A2aAgent {
    pub name: String,
    pub url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token_env: Option<String>,
    #[serde(default)]
    pub direct: bool,
}

/// Privacy router: when enabled, a turn whose input contains sensitive data
/// is forced onto `tier` (a local, on-box model) so it never reaches a cloud
/// provider. Off by default.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PrivacyConfig {
    #[serde(default)]
    pub enabled: bool,
    /// The safe tier sensitive turns route to (must be a local/on-box tier).
    #[serde(default = "default_privacy_tier")]
    pub tier: String,
    /// Extra regexes counted as sensitive, on top of the built-in set.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub extra_patterns: Vec<String>,
}

impl Default for PrivacyConfig {
    fn default() -> Self {
        PrivacyConfig { enabled: false, tier: default_privacy_tier(), extra_patterns: Vec::new() }
    }
}

fn default_privacy_tier() -> String {
    "local".to_string()
}

/// Global spend cap enforced by the gateway on the LLM listener — a token
/// bucket BELOW the agent, so the ceiling cannot be reasoned or coded around
/// from inside the harness (the moat, made literal). Renders to
/// `llm.policies.localRateLimit`. Off by default (no cap).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SpendingConfig {
    #[serde(default)]
    pub enabled: bool,
    /// Bucket size = refill amount per `interval`, i.e. the rolling cap.
    #[serde(default = "default_budget_amount")]
    pub budget: u64,
    /// Refill window, as an agentgateway duration ("24h", "1h", "60s").
    #[serde(default = "default_budget_interval")]
    pub interval: String,
    /// What the budget counts: LLM `tokens` (input+output) or `requests`.
    #[serde(default)]
    pub count: BudgetCount,
}

impl Default for SpendingConfig {
    fn default() -> Self {
        SpendingConfig {
            enabled: false,
            budget: default_budget_amount(),
            interval: default_budget_interval(),
            count: BudgetCount::default(),
        }
    }
}

/// Unit the spend cap counts — matches agentgateway's localRateLimit `type`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum BudgetCount {
    #[default]
    Tokens,
    Requests,
}

impl BudgetCount {
    pub fn gateway_type(&self) -> &'static str {
        match self {
            BudgetCount::Tokens => "tokens",
            BudgetCount::Requests => "requests",
        }
    }
}

fn default_budget_amount() -> u64 {
    1_000_000
}
fn default_budget_interval() -> String {
    "24h".to_string()
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ChannelsConfig {
    #[serde(default)]
    pub telegram: TelegramConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TelegramConfig {
    /// Starts when enabled AND the token env var is present in secrets.env.
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default = "default_telegram_token_env")]
    pub token_env: String,
}

impl Default for TelegramConfig {
    fn default() -> Self {
        TelegramConfig { enabled: true, token_env: default_telegram_token_env() }
    }
}

fn default_telegram_token_env() -> String {
    "TELEGRAM_BOT_TOKEN".to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// None => ~/.revenant/workspace/memory. Point at a folder inside an
    /// existing Obsidian vault to get its graph view over agent memory.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vault_path: Option<PathBuf>,
    #[serde(default)]
    pub embedder: EmbedderKind,
    /// Gateway mode: model name for POST /v1/embeddings.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub embed_model: Option<String>,
    #[serde(default = "default_consolidate_batch")]
    pub consolidate_batch: usize,
    #[serde(default = "default_consolidate_debounce")]
    pub consolidate_debounce_s: u64,
    #[serde(default = "default_sweep_interval")]
    pub sweep_interval_s: u64,
    #[serde(default = "default_injection_budget")]
    pub injection_budget_tokens: usize,
    #[serde(default = "default_retrieval_limit")]
    pub retrieval_limit: usize,
    #[serde(default = "default_true")]
    pub watch_vault: bool,
}

impl Default for MemoryConfig {
    fn default() -> Self {
        MemoryConfig {
            enabled: true,
            vault_path: None,
            embedder: EmbedderKind::default(),
            embed_model: None,
            consolidate_batch: default_consolidate_batch(),
            consolidate_debounce_s: default_consolidate_debounce(),
            sweep_interval_s: default_sweep_interval(),
            injection_budget_tokens: default_injection_budget(),
            retrieval_limit: default_retrieval_limit(),
            watch_vault: true,
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EmbedderKind {
    #[default]
    Builtin,
    Gateway,
}

fn default_true() -> bool {
    true
}
fn default_consolidate_batch() -> usize {
    8
}
fn default_consolidate_debounce() -> u64 {
    20
}
fn default_sweep_interval() -> u64 {
    900
}
fn default_injection_budget() -> usize {
    800
}
fn default_retrieval_limit() -> usize {
    12
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
    #[serde(default = "default_mcp_port")]
    pub mcp_port: u16,
    /// Base port for governed A2A egress; each gateway-routed remote agent
    /// gets `a2a_egress_base + its index`.
    #[serde(default = "default_a2a_egress_base")]
    pub a2a_egress_base: u16,
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
    #[serde(default = "default_max_iterations")]
    pub max_iterations: u32,
    /// Closed learning loop: after a successful multi-tool turn, distill a
    /// reusable skill from the trajectory (Hermes-style self-improvement).
    #[serde(default = "default_true")]
    pub learn: bool,
    /// Minimum tool calls in a turn before it's considered worth distilling.
    #[serde(default = "default_learn_min_tools")]
    pub learn_min_tools: usize,
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            default_tier: default_tier(),
            max_history_messages: default_max_history(),
            max_tokens: default_max_tokens(),
            max_iterations: default_max_iterations(),
            learn: true,
            learn_min_tools: default_learn_min_tools(),
        }
    }
}

fn default_learn_min_tools() -> usize {
    2
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TierConfig {
    pub targets: Vec<TierTarget>,
    /// How a multi-target tier routes across its targets. `failover` (default)
    /// tries targets in priority order with health-based eviction — resilience.
    /// `weighted` splits traffic across targets by `weight` — cross-provider
    /// cost/quality balancing (the intelligent-spending knob). Single-target
    /// tiers ignore this and render as a plain alias.
    #[serde(default)]
    pub strategy: RouteStrategy,
}

/// Multi-target routing strategy, matching agentgateway's `virtualModels`
/// routing enum (verified against v1.3.1: `failover` | `weighted`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RouteStrategy {
    #[default]
    Failover,
    Weighted,
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
    /// Relative weight for `weighted` routing (ignored under `failover`, where
    /// target order is priority order). Defaults to 1 when omitted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub weight: Option<u32>,
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
fn default_mcp_port() -> u16 {
    41002
}
fn default_a2a_egress_base() -> u16 {
    41010
}

/// Parse an endpoint URL into (scheme, host:port, path). Ports default by
/// scheme. Used to render gateway A2A backends and to build egress URLs.
pub fn parse_endpoint(url: &str) -> Option<(String, String, String)> {
    let (scheme, rest) = url.split_once("://")?;
    let (hostport, path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, "/"),
    };
    let host_with_port = if hostport.contains(':') {
        hostport.to_string()
    } else {
        let port = if scheme == "https" { 443 } else { 80 };
        format!("{hostport}:{port}")
    };
    Some((scheme.to_string(), host_with_port, path.to_string()))
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
fn default_max_iterations() -> u32 {
    25
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
            weight: None,
        };
        let mut tiers = BTreeMap::new();
        tiers.insert(
            "fast".into(),
            TierConfig {
                targets: vec![anthropic("claude-haiku-4-5-20251001")],
                strategy: RouteStrategy::Failover,
            },
        );
        tiers.insert(
            "balanced".into(),
            TierConfig {
                targets: vec![
                    anthropic("claude-sonnet-5"),
                    anthropic("claude-haiku-4-5-20251001"),
                ],
                strategy: RouteStrategy::Failover,
            },
        );
        tiers.insert(
            "deep".into(),
            TierConfig {
                targets: vec![anthropic("claude-opus-4-8")],
                strategy: RouteStrategy::Failover,
            },
        );
        Config {
            gateway: GatewayConfig {
                mode: GatewayMode::Bundled,
                version: "1.3.1".to_string(),
                llm_port: default_llm_port(),
                readiness_port: default_readiness_port(),
                stats_port: default_stats_port(),
                mcp_port: default_mcp_port(),
                a2a_egress_base: default_a2a_egress_base(),
                binary: None,
                endpoint: None,
            },
            agent: AgentConfig::default(),
            memory: MemoryConfig::default(),
            channels: ChannelsConfig::default(),
            privacy: PrivacyConfig::default(),
            spending: SpendingConfig::default(),
            mcp: Vec::new(),
            a2a_agents: Vec::new(),
            tiers,
        }
    }
}
