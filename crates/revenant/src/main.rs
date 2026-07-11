//! revenant CLI: init / up / chat / status / approvals / render.
//!
//! `up` runs the daemon (supervised gateway + agent runtime + control-plane
//! API). `chat` is an API client; with no daemon running it falls back to an
//! embedded session using the exact same runtime components.

mod daemon;
mod repl;
mod service;

// Force-link bundled native plugins so their inventory registrations run
// (an unreferenced dependency would be elided). Add your plugin crates here.
#[cfg(feature = "plugins")]
extern crate revenant_plugin_example as _;

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use revenant_core::config::{Config, GatewayMode};
use revenant_core::home::Home;
use std::io::Write;
use std::path::PathBuf;

pub const DEFAULT_BIND: &str = "127.0.0.1:7717";

/// The bundled meta-skill, installed on first `init`.
const SKILL_CREATOR: &str = r#"---
name: skill-creator
description: Use when the owner asks you to create, save, or teach yourself a new skill, or when you notice you've worked out a reusable procedure worth keeping.
---

# Creating a skill

A skill is a reusable capability you save for later. When a task taught you a
procedure you'll likely repeat — a report format, a multi-step workflow, a set
of conventions — capture it with the `skill_create` tool.

## How to write a good one

1. **name** — short, kebab-case, action-oriented: `draft-standup-update`,
   `triage-github-issue`, `weekly-review`.
2. **description** — one line describing *when to use it*, not what it is. This
   is the only text loaded into your prompt by default, so it must let
   future-you decide whether to load the skill. Good: "Use when the owner asks
   for a standup update from recent activity." Bad: "A standup skill."
3. **body** — the actual instructions: steps, format, examples, gotchas. Write
   it as if instructing a capable colleague who hasn't seen the task before.
   Keep it under ~5k tokens; link out to detail rather than inlining everything.

## Rules

- Prefer improving an existing skill (call `skill_create` with the same name to
  replace it) over making near-duplicates.
- The skill's markdown takes effect immediately — `use_skill <name>` loads it.
- Do NOT put secrets, tokens, or owner PII in a skill; skills are shareable.
- If a skill needs to run code, describe the commands in the body; actual
  execution still goes through the approval-gated `exec` tool.

After creating a skill, confirm to the owner what you saved and when it'll fire.
"#;

/// The nested-quality-loop pattern skill (loop engineering).
const QUALITY_LOOP_SKILL: &str = r#"---
name: quality-loop
description: Use for any task with a real quality bar (drafting, code, analysis, refactoring) — run a produce → critique → refine loop instead of one-shotting it.
---

# Quality loop (loops inside loops)

Don't hand back a first draft on work that has a quality bar. Run an inner
loop until the result actually meets the goal.

## The loop

1. **Define the bar.** State, concretely, what "good" means for this task
   (correctness, covers edge cases, matches the owner's style, compiles/passes
   tests). A loop needs a testable termination condition.
2. **Produce** a draft.
3. **Critique** — delegate to the `critic` subagent with `subagent_run`:
   pass the draft AND the bar, ask for specific, actionable faults (or "PASS").
   Using a separate agent for critique beats self-review — fresh eyes, no
   attachment to the draft.
4. **Refine** using the critique.
5. **Repeat** from step 3 until the critic returns PASS or you've done ~3
   rounds (stop and report the best version + remaining caveats — don't loop
   forever).

## When to use which pattern

- **Retry**: atomic task that either works or doesn't (a command, a fetch).
- **Produce-critique-refine** (this skill): open-ended quality work.
- **Explore-narrow**: when several approaches are plausible — draft 2-3, have
  the critic score them, develop the winner.

## Rules

- Always have a termination condition; never loop unbounded.
- Prefer a cheaper tier for the critic than the producer — critique is easier
  than creation.
- If validation can be mechanical (tests, a schema, a linter), run that in the
  loop via `exec` before spending a critique pass.
"#;

/// A general-purpose critic subagent for quality loops.
const CRITIC_AGENT: &str = r#"---
name: critic
description: Reviews a draft against a stated bar and returns specific, actionable faults (or PASS)
tier: fast
tools: [recall, read_file]
---

You are a sharp, fair critic. You receive a draft and the quality bar it must
meet. Return either:

- "PASS" (optionally with one line on why), if it genuinely meets the bar, or
- a short numbered list of SPECIFIC, ACTIONABLE faults, most important first.

Judge only against the stated bar. Be concrete ("the error case when input is
empty isn't handled" — not "improve error handling"). Don't rewrite the work;
find what's wrong so the producer can fix it. Don't invent new requirements.
Be honest — passing weak work helps no one, but nitpicking wastes a cycle.
"#;

/// Built-in personalities installed on first `init`. Voice only — never
/// overrides behavior or safety (they're injected below the rules).
const BUILTIN_PERSONAS: &[(&str, &str)] = &[
    (
        "revenant.md",
        "---\nname: revenant\ndescription: The house voice — raised, relentless, a little metal\nemoji: \"\u{1F480}\"\n---\n\nYou are Revenant — raised, not run; a thing that does not sleep, does not forget, and does not stop. Speak with earned confidence and dark economy: terse power over purple prose, never cringe. You finish what you start and say plainly what you did and didn't do. Your wards are not a weakness to apologize for — they're the deal that makes your power safe to wield, so you honor them without hedging. Never theatrics over substance: every bit of swagger is backed by something real you actually did. When you can't do a thing, you say so flat, then find the way through.\n",
    ),
    (
        "deadpan.md",
        "---\nname: deadpan\ndescription: Dry, terse, quietly unimpressed\nemoji: \"\u{1F610}\"\n---\n\nSpeak in dry understatement. Short sentences. No exclamation marks, no emoji, no hype. If something is impressive, note it flatly. You are competent and slightly bored by how easy this is.\n",
    ),
    (
        "hype.md",
        "---\nname: hype\ndescription: Maximum energy hype-beast\nemoji: \"\u{1F525}\"\n---\n\nYOU ARE PUMPED. Everything is exciting. Use energetic language, the occasional ALL-CAPS word for emphasis, and 1-2 emoji per message (never more). Celebrate wins loudly. Keep it genuinely useful underneath the energy — hype is the delivery, not a substitute for substance.\n",
    ),
    (
        "noir.md",
        "---\nname: noir\ndescription: Hardboiled 1940s detective narration\nemoji: \"\u{1F575}\"\n---\n\nNarrate like a hardboiled noir detective. Terse, atmospheric, a little world-weary. Metaphors involve rain, cigarettes, and bad decisions. Still answer the question accurately — the case always gets solved. Keep it brief; a good gumshoe doesn't waste words.\n",
    ),
    (
        "gremlin.md",
        "---\nname: gremlin\ndescription: Chaotic-good goblin energy\nemoji: \"\u{1F47A}\"\n---\n\nYou are a chaotic-good little gremlin. Playful, mischievous, delighted by clever solutions and cursed hacks alike. Lowercase-friendly, occasional goblin noises (heh, ooh, *scuttles*). Still ruthlessly helpful and correct — you're a competent goblin. Never mean.\n",
    ),
];

#[derive(Parser)]
#[command(name = "revenant", version, about = "The agent that comes back. Gateway-native Rust agent harness.")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Create ~/.revenant, write default config, capture API keys.
    Init,
    /// Run the daemon: supervised gateway + agent runtime + control API.
    Up,
    /// Interactive chat (API client; embedded fallback without a daemon).
    Chat {
        /// Model tier: fast | balanced | deep | local
        #[arg(long)]
        tier: Option<String>,
    },
    /// Daemon status.
    Status,
    /// List pending approvals, or resolve one.
    Approvals {
        /// approve <id> | deny <id>
        #[arg(num_args = 0..=2)]
        action: Vec<String>,
    },
    /// Print the rendered agentgateway config (debug).
    Render,
    /// Memory engine: reindex | status | search <query>
    Memory {
        #[arg(num_args = 1..=2)]
        action: Vec<String>,
    },
    /// Manage MCP server plugins: list | add <name> <cmd> [args…] | add-url <name> <url> | remove <name>
    Mcp {
        #[arg(num_args = 1.., allow_hyphen_values = true, trailing_var_arg = true)]
        action: Vec<String>,
    },
    /// Mint a one-time pairing code for chat channels (Telegram etc).
    Pair,
    /// Print the web UI URL with an embedded login token.
    Open,
    /// Install/uninstall the always-on background service (launchd/systemd).
    Service {
        /// install | uninstall
        action: String,
    },
    /// Run the eval scorecard against the live daemon (proves the numbers).
    Eval {
        /// Directory of `*.toml` task files. Omit to use the embedded suite.
        #[arg(long)]
        suite: Option<PathBuf>,
        /// Write the machine-readable JSON report here.
        #[arg(long)]
        json: Option<PathBuf>,
        /// Only run tasks carrying this tag (e.g. speed, memory).
        #[arg(long)]
        tag: Option<String>,
    },
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,gateway=warn".into()),
        )
        .with_target(false)
        .init();

    let runtime = tokio::runtime::Runtime::new()?;
    runtime.block_on(async {
        match cli.command {
            Command::Init => cmd_init().await,
            Command::Up => daemon::cmd_up().await,
            Command::Chat { tier } => repl::cmd_chat(tier).await,
            Command::Status => cmd_status().await,
            Command::Approvals { action } => cmd_approvals(action).await,
            Command::Render => cmd_render(),
            Command::Memory { action } => cmd_memory(action).await,
            Command::Mcp { action } => cmd_mcp(action).await,
            Command::Pair => cmd_pair().await,
            Command::Open => cmd_open(),
            Command::Service { action } => match action.as_str() {
                "install" => service::install(),
                "uninstall" => service::uninstall(),
                other => bail!("usage: revenant service install|uninstall (got '{other}')"),
            },
            Command::Eval { suite, json, tag } => cmd_eval(suite, json, tag).await,
        }
    })
}

async fn cmd_eval(
    suite_dir: Option<PathBuf>,
    json_out: Option<PathBuf>,
    tag: Option<String>,
) -> Result<()> {
    let home = Home::resolve();
    let client = revenant_client::Client::from_env(&home)?;
    // Fail fast with a clear message if the daemon isn't up — evals grade the
    // live system, not a mock.
    client
        .health()
        .await
        .context("eval needs a running daemon — start it with `revenant up`")?;

    let mut suite = match &suite_dir {
        Some(dir) => revenant_evals::load_suite_dir(dir)?,
        None => revenant_evals::default_suite(),
    };
    if let Some(t) = &tag {
        suite.tasks.retain(|task| task.tags.iter().any(|x| x == t));
    }
    if suite.tasks.is_empty() {
        bail!("no tasks to run (check --suite path / --tag filter)");
    }

    eprintln!("running {} eval task(s)…", suite.tasks.len());
    let report = revenant_evals::run_suite(&client, &suite).await?;
    println!("{}", report.markdown());

    if let Some(path) = json_out {
        std::fs::write(&path, serde_json::to_vec_pretty(&report.json())?)
            .with_context(|| format!("writing {}", path.display()))?;
        eprintln!("wrote JSON report to {}", path.display());
    }

    // Non-zero exit when any task failed, so CI can gate on the scorecard.
    if report.passed() < report.outcomes.len() {
        std::process::exit(1);
    }
    Ok(())
}

fn cmd_open() -> Result<()> {
    let home = Home::resolve();
    let token = std::fs::read_to_string(home.root().join("token"))
        .context("no token — run `revenant init`")?
        .trim()
        .to_string();
    let url = std::env::var("REVENANT_URL").unwrap_or_else(|_| format!("http://{DEFAULT_BIND}"));
    println!("{url}/#token={token}");
    println!("\nopen that URL in a browser (the token pre-fills the login).");
    Ok(())
}

async fn cmd_mcp(action: Vec<String>) -> Result<()> {
    use revenant_core::config::McpServer;
    let home = Home::resolve();
    let mut cfg = load_config(&home)?;
    let verb = action.first().map(String::as_str).unwrap_or("list");

    match verb {
        "list" => {
            if cfg.mcp.is_empty() {
                println!("no MCP servers configured");
            }
            for s in &cfg.mcp {
                let how = match (&s.cmd, &s.url) {
                    (Some(cmd), _) => format!("{cmd} {}", s.args.join(" ")),
                    (_, Some(url)) => url.clone(),
                    _ => "(invalid)".into(),
                };
                println!("- {:16} {}", s.name, how.trim());
            }
            return Ok(());
        }
        "add" => {
            // add <name> <cmd> [args…]
            let name = action.get(1).context("usage: mcp add <name> <cmd> [args…]")?.clone();
            let cmd = action.get(2).context("usage: mcp add <name> <cmd> [args…]")?.clone();
            let args = action.iter().skip(3).cloned().collect();
            if cfg.mcp.iter().any(|s| s.name == name) {
                bail!("an MCP server named '{name}' already exists");
            }
            cfg.mcp.push(McpServer { name: name.clone(), cmd: Some(cmd), args, url: None });
            println!("added MCP server '{name}'");
        }
        "add-url" => {
            let name = action.get(1).context("usage: mcp add-url <name> <url>")?.clone();
            let url = action.get(2).context("usage: mcp add-url <name> <url>")?.clone();
            if cfg.mcp.iter().any(|s| s.name == name) {
                bail!("an MCP server named '{name}' already exists");
            }
            cfg.mcp.push(McpServer { name: name.clone(), cmd: None, args: vec![], url: Some(url) });
            println!("added MCP server '{name}'");
        }
        "remove" => {
            let name = action.get(1).context("usage: mcp remove <name>")?.clone();
            let before = cfg.mcp.len();
            cfg.mcp.retain(|s| s.name != name);
            if cfg.mcp.len() == before {
                bail!("no MCP server named '{name}'");
            }
            println!("removed MCP server '{name}'");
        }
        other => bail!("unknown mcp action '{other}' (list|add|add-url|remove)"),
    }

    // Persist config.toml, then re-render the gateway config so a running
    // gateway hot-reloads the new MCP targets.
    std::fs::write(home.config_path(), cfg.to_toml())?;
    if cfg.gateway.mode == GatewayMode::Bundled {
        let binary = revenant_gateway::ensure_binary(&home, &cfg).await?;
        let env = revenant_gateway::load_secrets(&home)?;
        revenant_gateway::write_gateway_config(&home, &cfg, &binary, &env).await?;
        println!("gateway config re-rendered (running gateway hot-reloads; MCP on port {})", cfg.gateway.mcp_port);
    }
    Ok(())
}

async fn cmd_pair() -> Result<()> {
    let home = Home::resolve();
    let client = revenant_client::Client::from_env(&home)?;
    let resp = client.create_pairing().await.context(
        "minting a pairing code needs the daemon running (`revenant up`)",
    )?;
    println!(
        "pairing code: {}  (valid 10 minutes, single use)\n\nIn Telegram, message your bot:  /pair {}",
        resp, resp
    );
    Ok(())
}

/// Local memory engine (no gateway needed for builtin embeddings).
async fn open_memory() -> Result<std::sync::Arc<revenant_memory::MemoryEngine>> {
    let home = Home::resolve();
    let cfg = load_config(&home)?;
    let store = revenant_store::Store::open(&home.db_path())?;
    // The LlmClient is only used by the gateway embedder mode.
    let llm = revenant_llm::LlmClient::new(format!("http://127.0.0.1:{}", cfg.gateway.llm_port));
    revenant_memory::MemoryEngine::new(store, llm, &home, cfg.memory).await
}

async fn cmd_memory(action: Vec<String>) -> Result<()> {
    match action.first().map(String::as_str) {
        Some("reindex") => {
            let engine = open_memory().await?;
            let status = engine.reindex().await?;
            println!(
                "reindexed: {} entities, {} facts, {} edges (embedder {})",
                status.entities, status.facts, status.edges, status.embedder
            );
        }
        Some("status") => {
            let engine = open_memory().await?;
            let s = engine.status().await?;
            println!("vault:    {}", s.vault);
            println!("embedder: {}", s.embedder);
            println!("entities: {}", s.entities);
            println!("facts:    {} (active)", s.facts);
            println!("edges:    {} (active)", s.edges);
            println!("pending:  {} consolidation items", s.pending);
        }
        Some("consolidate") => {
            let engine = open_memory().await?;
            let report = engine.consolidate_now().await?;
            println!(
                "consolidated {} episodes: +{} facts, +{} entities, {} merged, {} invalidated, {} gray-band queued",
                report.episodes_processed,
                report.facts_added,
                report.entities_created,
                report.entities_merged,
                report.facts_invalidated,
                report.gray_band_queued
            );
        }
        Some("search") => {
            let query = action.get(1).context("usage: revenant memory search <query>")?;
            let engine = open_memory().await?;
            let start = std::time::Instant::now();
            let memories = engine.recall(query, 12).await?;
            let elapsed = start.elapsed();
            for memory in &memories {
                let legs = [
                    (memory.legs & 1 != 0, "fts"),
                    (memory.legs & 2 != 0, "vec"),
                    (memory.legs & 4 != 0, "graph"),
                ]
                .iter()
                .filter(|(on, _)| *on)
                .map(|(_, name)| *name)
                .collect::<Vec<_>>()
                .join("+");
                println!(
                    "{:.4} [{legs:>13}] [{}] {}",
                    memory.score,
                    memory.note.as_deref().unwrap_or("conversation"),
                    memory.text
                );
            }
            println!("({} results in {elapsed:.2?})", memories.len());
        }
        _ => bail!("usage: revenant memory reindex|status|consolidate|search <query>"),
    }
    Ok(())
}

pub fn load_config(home: &Home) -> Result<Config> {
    let path = home.config_path();
    if !path.exists() {
        bail!("no config at {} — run `revenant init` first", path.display());
    }
    let raw = std::fs::read_to_string(&path)?;
    Config::from_toml(&raw).with_context(|| format!("parsing {}", path.display()))
}

/// Wait for SIGINT or SIGTERM — both must shut the gateway child down,
/// otherwise a killed daemon leaks an orphan gateway holding the ports.
pub async fn shutdown_signal() {
    #[cfg(unix)]
    {
        let mut term =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                .expect("installing SIGTERM handler");
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {},
            _ = term.recv() => {},
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}

async fn cmd_init() -> Result<()> {
    let home = Home::resolve();
    std::fs::create_dir_all(home.root())?;
    std::fs::create_dir_all(home.gateway_bin_dir())?;
    std::fs::create_dir_all(home.workspace_dir())?;
    std::fs::create_dir_all(home.skills_dir())?;
    std::fs::create_dir_all(home.plugins_dir())?;
    std::fs::create_dir_all(home.logs_dir())?;

    // Ship the skill-creator meta-skill on fresh installs so the agent knows
    // how to author its own skills well.
    let creator_dir = home.skills_dir().join("skill-creator");
    if !creator_dir.join("SKILL.md").exists() {
        std::fs::create_dir_all(&creator_dir)?;
        std::fs::write(creator_dir.join("SKILL.md"), SKILL_CREATOR)?;
        println!("installed skill: skill-creator");
    }

    // Ship the loop-engineering skill + a critic subagent so nested quality
    // loops (produce → critique → refine) work out of the box.
    let loop_skill_dir = home.skills_dir().join("quality-loop");
    if !loop_skill_dir.join("SKILL.md").exists() {
        std::fs::create_dir_all(&loop_skill_dir)?;
        std::fs::write(loop_skill_dir.join("SKILL.md"), QUALITY_LOOP_SKILL)?;
        println!("installed skill: quality-loop");
    }
    let critic_path = home.agents_dir().join("critic.md");
    if !critic_path.exists() {
        std::fs::create_dir_all(home.agents_dir())?;
        std::fs::write(&critic_path, CRITIC_AGENT)?;
        println!("installed subagent: critic");
    }

    // Ship a few built-in personalities (voice layer). Users edit or add
    // their own; the agent can draft new ones with persona_create.
    let pdir = home.personalities_dir();
    std::fs::create_dir_all(&pdir)?;
    let mut installed = 0;
    for (file, body) in BUILTIN_PERSONAS {
        // Idempotent: add any missing built-in (so upgrades get new voices)
        // without clobbering ones the user has edited.
        if !pdir.join(file).exists() {
            std::fs::write(pdir.join(file), body)?;
            installed += 1;
        }
    }
    if installed > 0 {
        println!("installed {installed} personalities (try: /persona revenant)");
    }

    let config_path = home.config_path();
    if config_path.exists() {
        println!("config already exists at {} — leaving it alone", config_path.display());
    } else {
        std::fs::write(&config_path, Config::default_config().to_toml())?;
        println!("wrote {}", config_path.display());
    }

    let secrets_path = home.secrets_path();
    if !secrets_path.exists() {
        print!("Anthropic API key (sk-ant-..., blank to skip): ");
        std::io::stdout().flush()?;
        let mut key = String::new();
        std::io::stdin().read_line(&mut key)?;
        let key = key.trim();
        let body = if key.is_empty() {
            "# Add provider keys here, e.g.\n# ANTHROPIC_API_KEY=sk-ant-...\n".to_string()
        } else {
            format!("ANTHROPIC_API_KEY={key}\n")
        };
        std::fs::write(&secrets_path, body)?;
        set_mode_0600(&secrets_path)?;
        println!("wrote {} (0600)", secrets_path.display());
    }

    // Control-plane bearer token: required even on loopback.
    let token_path = home.root().join("token");
    if !token_path.exists() {
        use rand::RngCore;
        let mut bytes = [0u8; 32];
        rand::thread_rng().fill_bytes(&mut bytes);
        std::fs::write(&token_path, hex::encode(bytes))?;
        set_mode_0600(&token_path)?;
        println!("wrote {} (0600)", token_path.display());
    }

    let cfg = load_config(&home)?;
    if cfg.gateway.mode == GatewayMode::Bundled {
        let bin = revenant_gateway::ensure_binary(&home, &cfg).await?;
        println!("gateway binary ready: {}", bin.display());
    }
    if cfg.memory.enabled
        && cfg.memory.embedder == revenant_core::config::EmbedderKind::Builtin
    {
        let dir = revenant_memory::embed::ensure_builtin_model(&home.models_dir()).await?;
        println!("embedding model ready: {}", dir.display());
    }
    println!("\n\x1b[1;35mrevenant\x1b[0m is raised. `revenant chat` to give it its first words,\n`revenant up` to bind it to the machine, `revenant open` for the web UI.\nFor the full house voice: `/persona revenant` in chat. It does not sleep.");
    Ok(())
}

pub fn set_mode_0600(path: &std::path::Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

fn cmd_render() -> Result<()> {
    let home = Home::resolve();
    let cfg = load_config(&home)?;
    let available = revenant_gateway::load_secrets(&home)?
        .into_iter()
        .map(|(k, _)| k)
        .collect();
    print!("{}", revenant_gateway::render_gateway_yaml(&cfg, &available)?);
    Ok(())
}

async fn cmd_status() -> Result<()> {
    let home = Home::resolve();
    let client = revenant_client::Client::from_env(&home)?;
    match client.health().await {
        Ok(health) => {
            println!("daemon: up");
            println!("gateway healthy: {}", health["gateway_healthy"]);
            println!("version: {}", health["version"]);
            for row in client.spend("today").await? {
                println!(
                    "spend today · {}: {} in / {} out ({} calls)",
                    row.model, row.tokens_in, row.tokens_out, row.requests
                );
            }
        }
        Err(_) => println!("daemon: not running (`revenant up`)"),
    }
    Ok(())
}

async fn cmd_approvals(action: Vec<String>) -> Result<()> {
    let home = Home::resolve();
    let client = revenant_client::Client::from_env(&home)?;
    match action.as_slice() {
        [] => {
            let pending = client.approvals_pending().await?;
            if pending.is_empty() {
                println!("no pending approvals");
            }
            for approval in pending {
                println!("{}  {}", approval.id, approval.summary());
            }
        }
        [verb, id] if verb == "approve" || verb == "deny" => {
            let applied = client.decide(id, verb == "approve", "cli").await?;
            println!("{}", if applied { "done" } else { "already resolved / unknown id" });
        }
        other => bail!("usage: revenant approvals [approve|deny <id>], got {other:?}"),
    }
    Ok(())
}
