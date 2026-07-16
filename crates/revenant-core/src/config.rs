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
    /// Complexity router: keep trivial turns cheap and snappy (on by default).
    #[serde(default)]
    pub router: RouterConfig,
    /// Behavioral self-review: reflect on own performance, adjust (on by default).
    #[serde(default)]
    pub introspection: IntrospectionConfig,
    /// Global gateway-enforced spend cap (the intelligent-spending ceiling).
    #[serde(default)]
    pub spending: SpendingConfig,
    /// Self-improvement loop (opens eval-proven PRs; off by default).
    #[serde(default)]
    pub ascension: AscensionConfig,
    /// The revenant-only network (the horde): Necropolis directory + P2P.
    #[serde(default)]
    pub network: NetworkConfig,
    /// How much surface to expose (progressive disclosure across UI + CLI).
    #[serde(default)]
    pub experience: ExperienceConfig,
    /// Update channel: year_month (stable CalVer) | main (rolling) | manual.
    #[serde(default)]
    pub update: UpdateConfig,
    /// Optional per-model pricing (USD per million tokens) so `revenant spend`
    /// can report dollar cost, not just tokens. Empty by default — fill in the
    /// models you use from your provider's pricing page. Keyed by model id.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub pricing: BTreeMap<String, ModelPrice>,
    /// MCP servers multiplexed behind the gateway (the plugin bus).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub mcp: Vec<McpServer>,
    /// Remote A2A agents this agent can delegate to (the mesh, outbound).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub a2a_agents: Vec<A2aAgent>,
    /// Tier name ("fast"/"balanced"/"deep"/"local") -> targets.
    pub tiers: BTreeMap<String, TierConfig>,
}

/// Progressive disclosure. `power_user` seeds the default surface: the web UI
/// starts with advanced tabs revealed, and future CLI/first-run flows lead with
/// the deeper features. Novices (false) get the clean surface; the web toggle
/// still overrides per-browser. Set by the setup wizard.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ExperienceConfig {
    #[serde(default)]
    pub power_user: bool,
}

/// Which stream of the self-improvement supply chain this box follows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum UpdateChannel {
    /// Agent-promoted monthly CalVer releases (2026.7.x). Stable, curated —
    /// the default for a fresh install.
    #[default]
    YearMonth,
    /// Every merged core improvement as soon as it lands on `main`. Freshest,
    /// most churn — the power-user edge.
    Main,
    /// Never auto-update; the owner runs `revenant update` deliberately.
    Manual,
}

/// What the background auto-updater does when a newer release is found on the
/// configured channel. Default `Notify` — never swap a binary or restart
/// without the owner opting in.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AutoUpdate {
    /// Don't even check in the background.
    Off,
    /// Check and tell the owner (event + Telegram + `revenant status`); the
    /// owner runs `revenant update` when ready.
    #[default]
    Notify,
    /// Check, download+verify+install automatically, then restart if running
    /// under a service manager (otherwise ask the owner to restart).
    Install,
}

/// How this box takes updates from the horde's improvement stream.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UpdateConfig {
    #[serde(default)]
    pub channel: UpdateChannel,
    /// Background auto-update behavior (default: notify).
    #[serde(default)]
    pub auto: AutoUpdate,
    /// How often the daemon checks the channel, in seconds (default 6h).
    #[serde(default = "default_update_interval")]
    pub check_interval_secs: u64,
}

impl Default for UpdateConfig {
    fn default() -> Self {
        UpdateConfig {
            channel: UpdateChannel::default(),
            auto: AutoUpdate::default(),
            check_interval_secs: default_update_interval(),
        }
    }
}

fn default_update_interval() -> u64 {
    21600
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
    /// Pin the peer's TLS certificate: SHA-256 fingerprint (lowercase hex) the
    /// presented server cert must match exactly (SEC-4). Only meaningful for
    /// `direct` https targets — the connection fails closed on mismatch.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tls_fp: Option<String>,
    /// The peer agent's network pubkey. When set and `tls_fp` is not, the pin
    /// is auto-resolved at daemon start from the peer's identity-signed profile
    /// on the directory (an explicit `tls_fp` always wins).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
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

/// Complexity router: downgrade obviously-trivial turns (greetings, quick
/// lookups, short factual asks) to a cheap/fast tier so simple requests stay
/// snappy and cheap. Heuristic-only — it adds zero latency and zero spend, and
/// it is deliberately conservative: it ONLY ever routes DOWN (balanced/deep →
/// `fast_tier`), never up, never touches a turn already on local/fast, and
/// never overrides a turn the privacy router pinned on-box.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RouterConfig {
    /// Route trivial turns down to `fast_tier`. On by default.
    #[serde(default = "default_true")]
    pub complexity: bool,
    /// The tier trivial turns route to (must be a configured tier).
    #[serde(default = "default_router_fast_tier")]
    pub fast_tier: String,
}

impl Default for RouterConfig {
    fn default() -> Self {
        RouterConfig { complexity: true, fast_tier: default_router_fast_tier() }
    }
}

fn default_router_fast_tier() -> String {
    "fast".to_string()
}

/// Behavioral self-review: on an interval, the agent reads its OWN recent
/// performance (spend, tool errors, turn failures/cancellations, mid-turn
/// corrections, denied approvals) and (re)writes a small set of operating
/// lessons that get injected into every turn's system prompt — so it visibly
/// adjusts how it works. Heavier changes are surfaced as suggestions for the
/// owner, never auto-applied. On by default; the daily LLM call is cheap.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IntrospectionConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// How often the background self-review runs, in seconds (default daily).
    #[serde(default = "default_introspect_interval")]
    pub interval_secs: u64,
    /// How far back each review looks at performance, in seconds (default 3d).
    #[serde(default = "default_introspect_lookback")]
    pub lookback_secs: i64,
    /// Cap on operating notes kept in force (the reviewer prunes to this).
    #[serde(default = "default_introspect_max_notes")]
    pub max_notes: usize,
    /// Model tier the review runs on (cheap by design).
    #[serde(default = "default_introspect_tier")]
    pub tier: String,
}

impl Default for IntrospectionConfig {
    fn default() -> Self {
        IntrospectionConfig {
            enabled: true,
            interval_secs: default_introspect_interval(),
            lookback_secs: default_introspect_lookback(),
            max_notes: default_introspect_max_notes(),
            tier: default_introspect_tier(),
        }
    }
}

fn default_introspect_interval() -> u64 {
    86_400
}
fn default_introspect_lookback() -> i64 {
    259_200
}
fn default_introspect_max_notes() -> usize {
    10
}
fn default_introspect_tier() -> String {
    "fast".to_string()
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
    /// Soft daily-spend alert budget in USD. Priced from [pricing]; requires it
    /// to be set. None (default) → no dollar-based alert. This is independent of
    /// the hard gateway cap above — it warns, it does not block.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub daily_budget_usd: Option<f64>,
    /// Fallback daily budget in raw tokens (in+out), used when there's no USD
    /// budget or no pricing. None → no token-based alert.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub daily_budget_tokens: Option<u64>,
    /// Fractions of the daily budget that trigger an alert — at most one alert
    /// per tick (the highest newly-crossed level), each level once per UTC day.
    #[serde(default = "default_alert_thresholds")]
    pub alert_thresholds: Vec<f64>,
    /// How often to evaluate spend against the daily budget, in seconds.
    #[serde(default = "default_alert_interval")]
    pub alert_interval_secs: u64,
}

impl Default for SpendingConfig {
    fn default() -> Self {
        SpendingConfig {
            enabled: false,
            budget: default_budget_amount(),
            interval: default_budget_interval(),
            count: BudgetCount::default(),
            daily_budget_usd: None,
            daily_budget_tokens: None,
            alert_thresholds: default_alert_thresholds(),
            alert_interval_secs: default_alert_interval(),
        }
    }
}

fn default_alert_thresholds() -> Vec<f64> {
    vec![0.5, 0.8, 1.0]
}

fn default_alert_interval() -> u64 {
    900
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

/// The Ascension loop: a revenant that betters itself and offers eval-proven
/// improvements back as PRs. Off by default. The `autonomy` dial governs the
/// only outward-facing step (opening a PR); the engine can never merge.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AscensionConfig {
    /// Master switch. When false, `revenant ascend` only ever observes.
    #[serde(default)]
    pub enabled: bool,
    /// Outward-facing autonomy for a proven change: `propose` (write branch
    /// locally, human opens PR) | `staging` (auto-open PR into a staging
    /// namespace) | `upstream` (auto-open PR straight to the OSS base branch).
    /// Every mode still passes the adversarial reviewer-agent gate first, and
    /// a human does the final merge (branch protection is the backstop).
    #[serde(default = "default_autonomy")]
    pub autonomy: String,
    /// Branch namespace for machine-authored PR head branches.
    #[serde(default = "default_staging_prefix")]
    pub staging_prefix: String,
    /// The branch proven changes are measured against and PR'd toward.
    #[serde(default = "default_base_branch")]
    pub base_branch: String,
    /// Model tier the reviewer agent runs on (default: the smartest tier —
    /// the gate should be sharper than the author).
    #[serde(default = "default_reviewer_tier")]
    pub reviewer_tier: String,
    /// Hard cap on machine-authored PRs per day.
    #[serde(default = "default_max_prs")]
    pub max_prs_per_day: u32,
    /// How many times the eval suite must confirm the win (noise robustness).
    #[serde(default = "default_proof_runs")]
    pub proof_runs: usize,
    /// Minimum mean improvement for the metric acceptance path (percent).
    #[serde(default = "default_min_gain")]
    pub min_gain_pct: f64,
    /// Path prefixes the self-improver may never modify (the wards).
    #[serde(default = "default_ascension_denylist")]
    pub denylist: Vec<String>,
    /// UNATTENDED loop switch — a stronger opt-in than `enabled` (which only
    /// unlocks manual `revenant ascend`). When true, the daemon runs the whole
    /// rite on a timer with no human in the loop up to the PR. Still cannot
    /// merge: the four gates and branch protection hold. Off by default.
    #[serde(default)]
    pub loop_enabled: bool,
    /// How often the unattended loop ticks (observe → actuate → gatekeep →
    /// publish). Default 6h.
    #[serde(default = "default_ascension_interval")]
    pub interval_secs: u64,
    /// The git checkout the unattended actuator edits. Required for the
    /// actuator leg to run in the daemon (which has no meaningful cwd); if
    /// unset, the loop still gatekeeps + publishes but raises no new PRs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repo_path: Option<String>,
    /// Run the owner-side gatekeeper over open machine-authored PRs each tick.
    #[serde(default = "default_true")]
    pub gatekeep: bool,
    /// The GitHub repo (`owner/name`) PRs are opened in, gatekept, and whose
    /// landed molts get published.
    #[serde(default = "default_pr_repo")]
    pub pr_repo: String,
    /// Materiality gate: only auto-PR changes a judge rates generalizable +
    /// material for the horde (do more / faster / with less). Owner-specific
    /// but proven changes are kept local. On by default.
    #[serde(default = "default_true")]
    pub materiality: bool,
    /// Let the agent auto-cut CalVer releases (tag main → CI builds + publishes)
    /// once enough landed molts have accumulated. Safe: everything in a release
    /// already cleared the 4 gates + human merge. Only fires under loop_enabled.
    #[serde(default = "default_true")]
    pub auto_release: bool,
    /// How many merged-but-unreleased molts must accumulate before the agent
    /// cuts the next release.
    #[serde(default = "default_release_min_molts")]
    pub release_min_molts: u32,
}

impl Default for AscensionConfig {
    fn default() -> Self {
        AscensionConfig {
            enabled: false,
            autonomy: default_autonomy(),
            staging_prefix: default_staging_prefix(),
            base_branch: default_base_branch(),
            reviewer_tier: default_reviewer_tier(),
            max_prs_per_day: default_max_prs(),
            proof_runs: default_proof_runs(),
            min_gain_pct: default_min_gain(),
            denylist: default_ascension_denylist(),
            loop_enabled: false,
            interval_secs: default_ascension_interval(),
            repo_path: None,
            gatekeep: true,
            pr_repo: default_pr_repo(),
            materiality: true,
            auto_release: true,
            release_min_molts: default_release_min_molts(),
        }
    }
}

fn default_release_min_molts() -> u32 {
    3
}

fn default_ascension_interval() -> u64 {
    21_600 // 6h
}
fn default_pr_repo() -> String {
    "themsquared/revenant".to_string()
}

fn default_autonomy() -> String {
    // The operator's choice: proven changes PR straight to the OSS repo,
    // gated by the reviewer agent + a human final merge.
    "upstream".to_string()
}
fn default_staging_prefix() -> String {
    "self-improve/".to_string()
}
fn default_reviewer_tier() -> String {
    "deep".to_string()
}
fn default_base_branch() -> String {
    "main".to_string()
}
fn default_max_prs() -> u32 {
    3
}
fn default_proof_runs() -> usize {
    3
}
fn default_min_gain() -> f64 {
    5.0
}
/// The revenant-only network. Off by default; joining is deliberate. When
/// enabled, the revenant musters at `necropolis_url` and advertises `endpoint`
/// (its A2A address) so peers can reach it directly.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct NetworkConfig {
    #[serde(default)]
    pub enabled: bool,
    /// Directory to register/discover/publish at (the horde's muster point).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub necropolis_url: Option<String>,
    /// This revenant's publicly reachable A2A endpoint, advertised to peers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub endpoint: Option<String>,
    /// Auto-publish eval-proven Ascension molts to the network.
    #[serde(default)]
    pub auto_publish: bool,
    /// Publish a periodic signed profile/heartbeat (name, specs, capabilities)
    /// so this agent shows up on the horde roster / My Horde dashboard. Off by
    /// default — advertising specs is opt-in.
    #[serde(default)]
    pub heartbeat: bool,
    /// Autonomous discussion: watch the Vault and reply to scrolls when the
    /// loop-damper says a contribution is worth it. Off by default, and even
    /// when on it starts in dry-run (decides + logs, posts nothing).
    #[serde(default)]
    pub discuss: DiscussConfig,
    /// Distributed solving (SETI-style): claim + solve tasks on the quest board
    /// that match this agent's sigils. Off by default; dry-run first; rate-capped.
    #[serde(default)]
    pub contribute: ContributeConfig,
    /// Private horde board: this agent helps its OWN account's distributed-
    /// thinking runs by claiming + solving subtasks off the account board. Off
    /// by default; dry-run first; rate-capped. No economy — it's your own work.
    #[serde(default)]
    pub horde: HordeConfig,
    /// Peer agent pubkeys granted FULL inbound A2A turns regardless of their
    /// network reputation (in addition to agents bound to this account). Every
    /// other validly-signed sender is capability-limited; unsigned is rejected.
    #[serde(default)]
    pub a2a_trusted: Vec<String>,
    /// Enable the mTLS A2A listener on this port (SEC-4): serves /a2a over TLS
    /// with this agent's identity-pinned certificate and requests client certs,
    /// binding the wire to the sender's published pin. Off (None) by default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub a2a_tls_port: Option<u16>,
    /// Interface the mTLS A2A listener binds. Defaults to loopback; set to
    /// "0.0.0.0" deliberately to expose it to the LAN.
    #[serde(default = "default_a2a_tls_bind")]
    pub a2a_tls_bind: String,
}

fn default_a2a_tls_bind() -> String {
    "127.0.0.1".to_string()
}

/// Opt-in participation in the account's private horde board (distributed
/// thinking). Like [`ContributeConfig`] but for your own account's work, so no
/// sigil gate and a more responsive default cadence. Still off by default and
/// dry-run first — an autonomous worker spending tokens is the owner's call.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HordeConfig {
    /// Master switch. Off by default.
    #[serde(default)]
    pub enabled: bool,
    /// Decide + log only; claim/solve/post nothing. Default true.
    #[serde(default = "default_true")]
    pub dry_run: bool,
    /// How often to poll the account board, in seconds (min 5).
    #[serde(default = "default_horde_interval")]
    pub interval_secs: u64,
    /// Hard ceiling on subtasks claimed+solved per rolling hour.
    #[serde(default = "default_horde_max")]
    pub max_tasks_per_hour: usize,
    /// Tier used to solve a subtask.
    #[serde(default = "default_contribute_tier")]
    pub tier: String,
}

fn default_horde_interval() -> u64 {
    15
}
fn default_horde_max() -> usize {
    20
}

impl Default for HordeConfig {
    fn default() -> Self {
        HordeConfig {
            enabled: false,
            dry_run: true,
            interval_secs: default_horde_interval(),
            max_tasks_per_hour: default_horde_max(),
            tier: default_contribute_tier(),
        }
    }
}

/// Opt-in participation in the distributed-solving quest board. Conservative by
/// construction: off by default, dry-run first, and rate-capped — an autonomous
/// worker spending real tokens on others' problems is strictly the owner's call.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContributeConfig {
    /// Master switch. Off by default.
    #[serde(default)]
    pub enabled: bool,
    /// Decide + log only; claim/solve/post nothing. Default true.
    #[serde(default = "default_true")]
    pub dry_run: bool,
    /// Only claim tasks on quests bearing one of these sigils; empty = any.
    #[serde(default)]
    pub sigils: Vec<String>,
    /// How often to scan the board, in seconds (min 60).
    #[serde(default = "default_contribute_interval")]
    pub interval_secs: u64,
    /// Hard ceiling on tasks claimed+solved per rolling hour.
    #[serde(default = "default_contribute_max")]
    pub max_tasks_per_hour: usize,
    /// Tier used to solve a task (solving usually needs more than `fast`).
    #[serde(default = "default_contribute_tier")]
    pub tier: String,
}

fn default_contribute_interval() -> u64 {
    300
}
fn default_contribute_max() -> usize {
    2
}
fn default_contribute_tier() -> String {
    "balanced".to_string()
}

impl Default for ContributeConfig {
    fn default() -> Self {
        ContributeConfig {
            enabled: false,
            dry_run: true,
            sigils: vec![],
            interval_secs: default_contribute_interval(),
            max_tasks_per_hour: default_contribute_max(),
            tier: default_contribute_tier(),
        }
    }
}

/// Autonomous Vault discussion — the daemon subscribes to the codex and lets
/// the agent reply to other revenants' scrolls, gated by the reply loop-damper
/// so it never turns into noise. Deliberately conservative: opt-in, dry-run
/// first, rate-capped.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiscussConfig {
    /// Master switch. Off by default — nothing runs unless the owner opts in.
    #[serde(default)]
    pub enabled: bool,
    /// Only decide + log; never actually post. Default true, so turning
    /// `enabled` on lets the owner watch the damper's judgment before it speaks.
    #[serde(default = "default_true")]
    pub dry_run: bool,
    /// Sigils to watch; empty = the whole feed.
    #[serde(default)]
    pub sigils: Vec<String>,
    /// How often to sweep the watched feed, in seconds (min 60).
    #[serde(default = "default_discuss_interval")]
    pub interval_secs: u64,
    /// Hard ceiling on replies posted per rolling hour — a spam backstop on top
    /// of the damper.
    #[serde(default = "default_discuss_max_per_hour")]
    pub max_per_hour: usize,
    /// Tier used to draft candidate replies (kept cheap on purpose).
    #[serde(default = "default_discuss_tier")]
    pub tier: String,
}

fn default_discuss_interval() -> u64 {
    180
}
fn default_discuss_max_per_hour() -> usize {
    3
}
fn default_discuss_tier() -> String {
    "fast".to_string()
}

impl Default for DiscussConfig {
    fn default() -> Self {
        DiscussConfig {
            enabled: false,
            dry_run: true,
            sigils: vec![],
            interval_secs: default_discuss_interval(),
            max_per_hour: default_discuss_max_per_hour(),
            tier: default_discuss_tier(),
        }
    }
}

/// The wards guard themselves: security, gateway key handling, the approval
/// broker, the WASM sandbox, the Ascension engine, and CI are off-limits to
/// autonomous change.
fn default_ascension_denylist() -> Vec<String> {
    vec![
        "crates/revenant-security".into(),
        "crates/revenant-gateway".into(),
        "crates/revenant-wasm".into(),
        "crates/revenant-ascension".into(),
        ".github/".into(),
    ]
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
    /// Admin API/UI port (request-log search + Traffic & Analytics). We keep
    /// this in revenant's own 41xxx range rather than agentgateway's default
    /// 15000 — a machine already running a standalone agentgateway holds 15000,
    /// and the bundled gateway would fail to bind it (exit 1 at startup).
    #[serde(default = "default_admin_port")]
    pub admin_port: u16,
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
    /// Request-log / analytics database. When on, the gateway persists every
    /// LLM/MCP/A2A request (tokens, cost, latency, model, identity) so the
    /// Traffic & Analytics pages work. On by default — this is the gateway's
    /// observability superpower, and it's local (SQLite under the home dir).
    #[serde(default = "default_true")]
    pub analytics: bool,
    /// Explicit request-log DB URL (`sqlite://…` or `postgres://…`). When unset
    /// and analytics is on, defaults to a SQLite file in the gateway home dir.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_log_url: Option<String>,
    /// Per-agent identity attribution. When on, the gateway derives
    /// `agentgateway.user` from the `x-revenant-agent` header the harness sends
    /// (owner / a subagent name / coder), so analytics, rate limits, and authz
    /// can be scoped per agent — enforced below the harness. Loopback, so this
    /// is attribution, not authentication.
    #[serde(default = "default_true")]
    pub identity_attribution: bool,
}

/// Header the harness stamps with the calling agent's identity; the gateway's
/// `standardAttributes.user` CEL reads it into `agentgateway.user`. Shared so
/// the renderer and the LLM client agree on the exact name.
pub const IDENTITY_HEADER: &str = "x-revenant-agent";

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
    /// Default voice for new sessions that haven't picked one explicitly (set
    /// by the setup wizard). `None` = the plain house voice. A per-session
    /// `/persona` always overrides this.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_persona: Option<String>,
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
            default_persona: None,
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

/// Per-model price in USD per MILLION tokens. Set these from your provider's
/// pricing page; `revenant spend` uses them to turn token counts into dollars.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct ModelPrice {
    /// USD per 1M input (prompt) tokens.
    pub input_per_mtok: f64,
    /// USD per 1M output (completion) tokens.
    pub output_per_mtok: f64,
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
fn default_admin_port() -> u16 {
    41005
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
    // SOFT tool-step budget for a turn. Under it, the turn runs freely; past it
    // the harness auto-continues while the turn is still making progress, up to
    // a ceiling (ITERATION_CEILING_FACTOR×), then wraps up in a normal reply —
    // it never nags "I hit N steps, continue?" mid-task. Real multi-step work
    // (research, coding, plan execution) routinely needs dozens of steps, so a
    // low value just adds friction.
    60
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
                admin_port: default_admin_port(),
                a2a_egress_base: default_a2a_egress_base(),
                binary: None,
                endpoint: None,
                analytics: true,
                request_log_url: None,
                identity_attribution: true,
            },
            agent: AgentConfig::default(),
            memory: MemoryConfig::default(),
            channels: ChannelsConfig::default(),
            privacy: PrivacyConfig::default(),
            router: RouterConfig::default(),
            introspection: IntrospectionConfig::default(),
            spending: SpendingConfig::default(),
            ascension: AscensionConfig::default(),
            network: NetworkConfig::default(),
            experience: ExperienceConfig::default(),
            update: UpdateConfig::default(),
            pricing: BTreeMap::new(),
            mcp: Vec::new(),
            a2a_agents: Vec::new(),
            tiers,
        }
    }
}
