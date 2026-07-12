//! revenant CLI: init / up / chat / status / approvals / render.
//!
//! `up` runs the daemon (supervised gateway + agent runtime + control-plane
//! API). `chat` is an API client; with no daemon running it falls back to an
//! embedded session using the exact same runtime components.

mod ascend_loop;
mod daemon;
mod repl;
mod service;

// Force-link bundled native plugins so their inventory registrations run
// (an unreferenced dependency would be elided). Add your plugin crates here.
#[cfg(feature = "plugins")]
extern crate revenant_plugin_example as _;

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use revenant_core::config::{Config, GatewayMode, UpdateChannel};
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

/// Autoresearch methodology skill — drives web_search + web_fetch well.
const AUTORESEARCH_SKILL: &str = r#"---
name: autoresearch
description: Use for any question that needs current, external, or factual information you can't answer from memory — research it with web_search + web_fetch and answer with citations.
---

# Autoresearch

When a question needs facts you don't already hold (current events, docs,
prices, comparisons, "what is X", "how do I Y with library Z"), do not guess.
Research it.

## Method
1. **Decompose** the question into 2–4 distinct search angles. Different
   phrasings surface different sources.
2. **web_search** each angle. Skim titles + snippets; pick the most credible,
   most relevant URLs (prefer primary sources, official docs, reputable orgs).
3. **web_fetch** the top 2–5 URLs. Read the actual content — snippets lie.
4. **Cross-check**: a claim that matters should appear in ≥2 independent
   sources, or be flagged as single-sourced/uncertain.
5. **Synthesize**: answer the question directly first, then supporting detail.
   Cite sources inline as [title](url). Separate what you verified from what
   you're inferring.

## Rules
- Fetch before you trust — never answer from snippets alone on anything
  load-bearing.
- If sources conflict, say so and give the most credible reading.
- If the web turns up nothing usable, say that plainly rather than inventing.
- Keep it tight: the owner wants the answer, then the evidence — not a link dump.
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
#[command(name = "revenant", version, about = "The agent that comes back. Gateway-native Rust agent harness.",
    after_help = "Run `revenant` with no command for guided setup, then chat.\n\nAdvanced commands (hidden above; run `revenant <cmd> --help`):\n  ascend, pr-review, net, necropolis, eval, memory, mcp, render, service, init")]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Guided first-run setup, then drops you into chat. Safe to re-run.
    Setup,
    #[command(hide = true)]
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
    /// Token + dollar spend by model, with budget status (cost-consciousness).
    Spend {
        /// Window: today | 24h | 7d
        #[arg(long, default_value = "today")]
        window: String,
    },
    /// Diagnose your setup in plain English — config, keys, credit, daemon,
    /// channels, network. Run this first if anything feels off.
    Doctor,
    /// Update revenant to the latest release (checksum-verified, backed up).
    Update {
        /// Only report whether an update is available; don't install.
        #[arg(long)]
        check: bool,
    },
    /// List pending approvals, or resolve one.
    Approvals {
        /// approve <id> | deny <id>
        #[arg(num_args = 0..=2)]
        action: Vec<String>,
    },
    #[command(hide = true)]
    /// Print the rendered agentgateway config (debug).
    Render,
    #[command(hide = true)]
    /// Memory engine: reindex | status | search <query>
    Memory {
        #[arg(num_args = 1..=2)]
        action: Vec<String>,
    },
    #[command(hide = true)]
    /// Manage MCP server plugins: list | add <name> <cmd> [args…] | add-url <name> <url> | remove <name>
    Mcp {
        #[arg(num_args = 1.., allow_hyphen_values = true, trailing_var_arg = true)]
        action: Vec<String>,
    },
    /// Mint a one-time pairing code for chat channels (Telegram etc).
    Pair,
    /// Print the web UI URL with an embedded login token.
    Open,
    #[command(hide = true)]
    /// Install/uninstall the always-on background service (launchd/systemd).
    Service {
        /// install | uninstall
        action: String,
    },
    /// Observe the eval scorecard and plan self-improvement candidates. With
    /// --run, drive the top candidate through the full actuator (isolate →
    #[command(hide = true)]
    /// implement → prove → review → offer). PRs are dry-run unless --live.
    Ascend {
        #[arg(long)]
        run: bool,
        #[arg(long)]
        live: bool,
        /// Drive the actuator on an explicit task instead of an eval-derived
        /// candidate (e.g. "fix the clippy warning in crates/foo/src/bar.rs").
        #[arg(long)]
        fix: Option<String>,
        /// Run the auto-publish leg once: sign + push landed (human-merged)
        /// `ascension` PRs to the network as Improvement artifacts. Skips the
        /// actuator entirely.
        #[arg(long)]
        publish: bool,
    },
    /// Cut a CalVer release: tag main with the next `vYEAR.MONTH.patch` and push
    /// so CI builds + publishes it, bundling landed molts. --dry-run to preview.
    #[command(hide = true)]
    Promote {
        #[arg(long)]
        dry_run: bool,
    },
    /// Gatekeeper: independently review open machine-authored (`ascension`) PRs
    #[command(hide = true)]
    /// and label them ascension-approved / ascension-blocked for your merge.
    PrReview {
        #[arg(long)]
        repo: Option<String>,
        #[arg(long)]
        limit: Option<usize>,
    },
    #[command(hide = true)]
    /// Run a Necropolis directory server (the horde's muster point).
    Necropolis {
        #[arg(long, default_value_t = 7720)]
        port: u16,
        /// Ledger file (durable, hash-linked). Defaults to ~/.revenant/necropolis.db.
        #[arg(long)]
        db: Option<PathBuf>,
    },
    #[command(hide = true)]
    /// Revenant-only network: register | peers | publish <kind> <file> | list [kind] | pull <id> | sync <peer-url> | verify
    Net {
        #[arg(num_args = 1.., allow_hyphen_values = true, trailing_var_arg = true)]
        action: Vec<String>,
    },
    #[command(hide = true)]
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
        /// Run the agent-behaviour suite (tool use, multi-turn, research).
        #[arg(long)]
        agent: bool,
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
            // Bare `revenant`: first run (no config) → the setup wizard;
            // otherwise → straight into chat. The zero-friction default.
            None => {
                let home = Home::resolve();
                if home.config_path().exists() {
                    repl::cmd_chat(None).await
                } else {
                    cmd_setup().await
                }
            }
            Some(cmd) => run_command(cmd).await,
        }
    })
}

async fn run_command(command: Command) -> Result<()> {
    match command {
            Command::Setup => cmd_setup().await,
            Command::Init => cmd_init().await,
            Command::Up => daemon::cmd_up().await,
            Command::Chat { tier } => repl::cmd_chat(tier).await,
            Command::Status => cmd_status().await,
            Command::Spend { window } => cmd_spend(window).await,
            Command::Doctor => cmd_doctor().await,
            Command::Update { check } => cmd_update(check),
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
            Command::Eval { suite, json, tag, agent } => cmd_eval(suite, json, tag, agent).await,
            Command::Ascend { run, live, fix, publish } => cmd_ascend(run, live, fix, publish).await,
            Command::Promote { dry_run } => cmd_promote(dry_run).await,
            Command::PrReview { repo, limit } => cmd_pr_review(repo, limit).await,
            Command::Necropolis { port, db } => cmd_necropolis(port, db).await,
            Command::Net { action } => cmd_net(action).await,
    }
}

async fn cmd_necropolis(port: u16, db: Option<PathBuf>) -> Result<()> {
    use std::sync::{Arc, Mutex};
    let home = Home::resolve();
    let db_path = db.unwrap_or_else(|| home.root().join("necropolis.db"));
    let dir = revenant_net::necropolis::Directory::open(&db_path.to_string_lossy())
        .context("opening Necropolis ledger")?;
    let addr = std::net::SocketAddr::from(([127, 0, 0, 1], port));
    println!("🜁 Necropolis — the horde musters at http://{addr} (ledger: {})", db_path.display());
    revenant_net::necropolis::serve(addr, Arc::new(Mutex::new(dir))).await
}

pub(crate) fn now_ts() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Filesystem-safe slug for an adopted artifact's install path.
fn net_slug(title: &str) -> String {
    let s: String = title
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c.to_ascii_lowercase() } else { '-' })
        .collect();
    let s = s.trim_matches('-').to_string();
    if s.is_empty() {
        "artifact".to_string()
    } else {
        s
    }
}

async fn cmd_net(action: Vec<String>) -> Result<()> {
    let home = Home::resolve();
    let cfg = load_config(&home).ok();
    let id = revenant_net::Identity::load_or_create(&home.identity_dir())?;
    let verb = action.first().map(String::as_str).unwrap_or("");

    // Local-ledger verbs need no muster URL — they operate on this node's own
    // durable Necropolis (or, for `sync`, an explicit peer given as an arg).
    let local_db = home.root().join("necropolis.db");
    match verb {
        // `verify` with NO argument = audit the local ledger. `verify <token>`
        // = confirm an account email (handled in the client match below).
        "verify" if action.get(1).is_none() => {
            let dir = revenant_net::Directory::open(&local_db.to_string_lossy())
                .context("opening local Necropolis ledger")?;
            // Directory::open re-verifies the entire hash chain on load; reaching
            // here means the audit passed.
            println!(
                "🜁 ledger VERIFIED — {} entries, head seq {}  ({})",
                dir.ledger_len()?,
                dir.head_seq()?,
                local_db.display()
            );
            return Ok(());
        }
        "sync" => {
            let peer_url = action.get(1).context("usage: net sync <peer-url>")?;
            let mut dir = revenant_net::Directory::open(&local_db.to_string_lossy())
                .context("opening local Necropolis ledger")?;
            let peer = revenant_net::NecropolisClient::new(peer_url);
            let peer_head = peer.ledger_head().await.context("reading peer ledger head")?;
            let since = dir.head_seq()?;
            let incoming = peer.ledger_since(since).await.context("pulling peer ledger")?;
            let fetched = incoming.len();
            let applied = dir
                .apply_remote(&incoming)
                .context("applying peer entries (chain re-verified locally)")?;
            println!(
                "🜁 synced from {peer_url}\n   peer head: seq {} · local head: seq {}\n   fetched {fetched}, applied {applied} new entr{} — every hash re-verified on this box",
                peer_head.seq,
                dir.head_seq()?,
                if applied == 1 { "y" } else { "ies" },
            );
            return Ok(());
        }
        _ => {}
    }

    let url = std::env::var("REVENANT_NECROPOLIS")
        .ok()
        .or_else(|| cfg.as_ref().and_then(|c| c.network.necropolis_url.clone()))
        .context("no Necropolis URL — set REVENANT_NECROPOLIS or [network].necropolis_url")?;
    let client = revenant_net::NecropolisClient::new(&url);

    match verb {
        "id" => println!("{} (fingerprint {})", id.id(), id.fingerprint()),
        "register" => {
            let endpoint = cfg
                .as_ref()
                .and_then(|c| c.network.endpoint.clone())
                .unwrap_or_else(|| "http://127.0.0.1:7717/a2a".to_string());
            client.register(&id.id(), &endpoint, &["chat".into(), "ascension".into()]).await?;
            println!("mustered at {url} as {} (endpoint {endpoint})", id.fingerprint());
        }
        "peers" => {
            for p in client.peers().await? {
                println!(
                    "{}  rep(published={}, adopted={})  {}",
                    &p["id"].as_str().unwrap_or("")[..8.min(p["id"].as_str().unwrap_or("").len())],
                    p["reputation"]["published"], p["reputation"]["adopted"], p["endpoint"],
                );
            }
        }
        "publish" => {
            let kind_s = action.get(1).context("usage: net publish <kind> <file> [title]")?;
            let file = action.get(2).context("usage: net publish <kind> <file> [title]")?;
            let title = action.get(3).cloned().unwrap_or_else(|| file.clone());
            let kind: revenant_net::ArtifactKind =
                serde_json::from_value(serde_json::Value::String(kind_s.clone()))
                    .with_context(|| format!("bad kind '{kind_s}' (skill|plugin|signal|improvement)"))?;
            let bytes = std::fs::read(file).with_context(|| format!("reading {file}"))?;
            // Skills carry a human description in their frontmatter — surface it
            // in the catalog (it's metadata, outside the signed preimage).
            let desc = if matches!(kind, revenant_net::ArtifactKind::Skill) {
                revenant_net::artifact::frontmatter_description(&String::from_utf8_lossy(&bytes))
                    .unwrap_or_default()
            } else {
                String::new()
            };
            let artifact = revenant_net::Artifact::create(
                &id, kind, title, desc, &bytes, None, now_ts(),
            );
            let aid = client.publish(&artifact).await?;
            println!("published {} artifact {}", kind_s, &aid[..12]);
        }
        "list" => {
            for a in client.list(action.get(1).map(String::as_str)).await? {
                println!(
                    "{}  [{}]  {}  by {}  {}",
                    &a["id"].as_str().unwrap_or("")[..12.min(a["id"].as_str().unwrap_or("").len())],
                    a["kind"].as_str().unwrap_or("?"),
                    a["title"].as_str().unwrap_or(""),
                    &a["author"].as_str().unwrap_or("")[..8.min(a["author"].as_str().unwrap_or("").len())],
                    if a["has_eval_proof"].as_bool().unwrap_or(false) { "✅proof" } else { "" },
                );
            }
        }
        "pull" => {
            let aid = action.get(1).context("usage: net pull <id> [out-file]")?;
            let artifact = client.pull(aid).await?; // verifies signature + hash
            println!(
                "pulled '{}' [{:?}] by {} — signature VERIFIED",
                artifact.title,
                artifact.kind,
                &artifact.author[..8],
            );
            if let Some(out) = action.get(2) {
                std::fs::write(out, artifact.payload()?)?;
                println!("wrote payload to {out}");
            }
        }
        "adopt" => {
            // Pull (signature + hash verified inside client.pull), install the
            // capability into the local box, and attest the adoption — the
            // horde teaching itself. Nothing is trusted until it verifies here.
            let aid = action.get(1).context("usage: net adopt <id>")?;
            let artifact = client.pull(aid).await?;
            let slug = net_slug(&artifact.title);
            let payload = artifact.payload()?;
            let dest = match artifact.kind {
                revenant_net::ArtifactKind::Skill => {
                    let dir = home.skills_dir().join(&slug);
                    std::fs::create_dir_all(&dir)?;
                    let p = dir.join("SKILL.md");
                    std::fs::write(&p, &payload)?;
                    p
                }
                revenant_net::ArtifactKind::Plugin => {
                    std::fs::create_dir_all(home.plugins_dir())?;
                    let p = home.plugins_dir().join(format!("{slug}.wasm"));
                    std::fs::write(&p, &payload)?;
                    p
                }
                revenant_net::ArtifactKind::Signal => {
                    let p = home.root().join("signals.log");
                    use std::io::Write;
                    let mut f = std::fs::OpenOptions::new().create(true).append(true).open(&p)?;
                    writeln!(f, "{}\t{}", artifact.title, String::from_utf8_lossy(&payload))?;
                    p
                }
                revenant_net::ArtifactKind::Improvement => {
                    // Code changes are never auto-applied — saved for review.
                    let dir = home.root().join("adopted-improvements");
                    std::fs::create_dir_all(&dir)?;
                    let p = dir.join(format!("{slug}.patch"));
                    std::fs::write(&p, &payload)?;
                    p
                }
            };
            client.attest(aid, &id.id(), true).await?;
            println!(
                "adopted '{}' [{:?}] by {} → {} · attested to the network",
                artifact.title,
                artifact.kind,
                &artifact.author[..8],
                dest.display(),
            );
        }
        "signup" => {
            // Register a human by email; save the account key locally.
            let email = action.get(1).context("usage: net signup <email>")?;
            let resp = client.signup(email).await?;
            if resp.get("status").and_then(|s| s.as_str()) == Some("already verified") {
                println!("that email is already verified — run `revenant net bind` to add this agent");
                return Ok(());
            }
            if let Some(key) = resp.get("account_key").and_then(|k| k.as_str()) {
                std::fs::write(home.root().join("account.key"), key)?;
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    let _ = std::fs::set_permissions(
                        home.root().join("account.key"),
                        std::fs::Permissions::from_mode(0o600),
                    );
                }
            }
            println!("registered {email} — account key saved to ~/.revenant/account.key");
            match resp.get("verify_token").and_then(|t| t.as_str()) {
                Some(tok) => println!("verify now with:\n  revenant net verify {tok}"),
                None => println!("check your email for the token, then:\n  revenant net verify <token>"),
            }
        }
        "confirm" | "verify" => {
            let token = action.get(1).context("usage: revenant net verify <token>")?;
            client.verify_account(token).await?;
            println!("✅ email confirmed. Now bind this agent: revenant net bind");
        }
        "bind" => {
            // Bind THIS node's identity to the verified account (sign a proof).
            let key = std::fs::read_to_string(home.root().join("account.key"))
                .context("no account key — run `revenant net signup <email>` first")?;
            let key = key.trim();
            let sig = id.sign_hex(key.as_bytes());
            client.bind_agent(key, &id.id(), &sig).await?;
            println!("🜁 bound agent {} to your account — it may now publish to the horde", id.fingerprint());
        }
        other => bail!(
            "unknown net command '{other}' (id|register|signup|confirm|bind|peers|publish|list|pull|adopt|sync|verify)"
        ),
    }
    Ok(())
}

async fn cmd_ascend(run: bool, live: bool, fix: Option<String>, publish: bool) -> Result<()> {
    let home = Home::resolve();
    let cfg = load_config(&home)?;
    let asc = cfg.ascension.clone();

    // --publish: run only the auto-publish leg (no daemon required — it just
    // reads merged PRs via gh and signs artifacts to the network).
    if publish {
        println!("🜁 Publishing landed molts from {} to the network…", asc.pr_repo);
        let n = ascend_loop::publish_landed_molts(&home, &asc, &cfg.network, true).await?;
        println!("done — {n} new molt(s) published.");
        return Ok(());
    }

    let client = revenant_client::Client::from_env(&home)?;
    client
        .health()
        .await
        .context("ascend needs a running daemon — start it with `revenant up`")?;

    // --fix: drive the actuator on an explicit task, skipping eval detection.
    if let Some(task) = fix {
        let repo = std::env::current_dir()?;
        if !repo.join(".git").exists() {
            bail!("`revenant ascend --fix` must run from the revenant git repo (cwd has no .git)");
        }
        let candidate = revenant_ascension::Candidate {
            kind: revenant_ascension::CandidateKind::FailingTask,
            target: "adhoc-fix".into(),
            detail: task.clone(),
            priority: 100.0,
        };
        println!(
            "🜁 ACTUATOR — ad-hoc task in an isolated worktree{}:\n   {task}\n",
            if live { " (LIVE)" } else { " (dry-run offer)" }
        );
        let run_cfg = ascend_run_cfg(&asc, live);
        let today = (now_ts() / 86_400).to_string();
        let outcome = revenant_ascension::run::run_candidate(
            &client, &repo, candidate, &run_cfg, home.root(), &today, Some(&task),
        )
        .await?;
        print_ascend_outcome(&outcome);
        return Ok(());
    }

    println!("🜁 Ascension — observe & plan");
    println!(
        "   autonomy: {} · reviewer: {} tier · warded: {}",
        asc.autonomy,
        asc.reviewer_tier,
        asc.denylist.join(", "),
    );
    println!();

    // Observe: run the live scorecard, then detect candidates.
    eprintln!("running eval scorecard to observe current state…");
    let suite = revenant_evals::default_suite();
    let report = revenant_evals::run_suite(&client, &suite).await?;
    println!("{}", report.markdown());

    let candidates = revenant_ascension::detect(&report);
    if candidates.is_empty() {
        println!("\nNo improvement candidates — the scorecard is clean. Nothing to raise.");
        return Ok(());
    }
    println!("\n## Candidates (most promising first)\n");
    for (i, c) in candidates.iter().enumerate() {
        println!("{}. [{:?}] `{}` — {}", i + 1, c.kind, c.target, c.detail);
    }

    if !run {
        println!("\n(observe-only — pass --run to drive the top candidate through the actuator.)");
        return Ok(());
    }

    // The repository the actuator edits is the current directory.
    let repo = std::env::current_dir()?;
    if !repo.join(".git").exists() {
        bail!("`revenant ascend --run` must be run from the revenant git repo (cwd has no .git)");
    }
    let top = candidates.into_iter().next().unwrap();
    println!(
        "\n🜁 ACTUATOR — driving top candidate `{}` in an isolated worktree{}…\n",
        top.target,
        if live { " (LIVE — will open a real PR if approved)" } else { " (dry-run offer)" },
    );
    let run_cfg = ascend_run_cfg(&asc, live);
    let today = (now_ts() / 86_400).to_string();
    let outcome = revenant_ascension::run::run_candidate(
        &client, &repo, top, &run_cfg, home.root(), &today, None,
    )
    .await?;
    print_ascend_outcome(&outcome);
    Ok(())
}

pub(crate) fn gh_capture(args: &[&str]) -> Option<String> {
    let out = std::process::Command::new("gh").args(args).output().ok()?;
    out.status.success().then(|| String::from_utf8_lossy(&out.stdout).into_owned())
}

async fn cmd_promote(dry_run: bool) -> Result<()> {
    let home = Home::resolve();
    let cfg = load_config(&home)?;
    if cfg.ascension.repo_path.is_none() {
        // Fall back to cwd if it's a git repo — the manual invocation case.
        let cwd = std::env::current_dir()?;
        if !cwd.join(".git").exists() {
            bail!("set ascension.repo_path (or run from the revenant git repo) to cut a release");
        }
    }
    let asc = {
        let mut a = cfg.ascension.clone();
        if a.repo_path.is_none() {
            a.repo_path = Some(std::env::current_dir()?.to_string_lossy().to_string());
        }
        a
    };
    println!("🜁 Promote — {} (repo {})", if dry_run { "dry-run" } else { "LIVE" }, asc.pr_repo);
    match ascend_loop::promote_release(&home, &asc, dry_run).await? {
        Some(msg) => println!("{msg}"),
        None => println!(
            "nothing to cut — fewer than {} merged-but-unreleased molt(s) since the last release.",
            asc.release_min_molts
        ),
    }
    Ok(())
}

async fn cmd_pr_review(repo: Option<String>, limit: Option<usize>) -> Result<()> {
    let home = Home::resolve();
    let cfg = load_config(&home)?;
    let asc = cfg.ascension.clone();
    let client = revenant_client::Client::from_env(&home)?;
    client
        .health()
        .await
        .context("pr-review needs a running daemon — start it with `revenant up`")?;
    let repo = repo.unwrap_or_else(|| asc.pr_repo.clone());
    ascend_loop::gatekeep_open_prs(&client, &asc, &repo, limit, true).await?;
    Ok(())
}

pub(crate) fn ascend_run_cfg(
    asc: &revenant_core::config::AscensionConfig,
    live: bool,
) -> revenant_ascension::run::RunConfig {
    revenant_ascension::run::RunConfig {
        coder_tier: asc.reviewer_tier.clone(),
        reviewer_tier: asc.reviewer_tier.clone(),
        base_branch: asc.base_branch.clone(),
        staging_prefix: asc.staging_prefix.clone(),
        max_prs_per_day: asc.max_prs_per_day,
        denylist: asc.denylist.clone(),
        max_repair: 5,
        live,
        materiality: asc.materiality,
    }
}

fn print_ascend_outcome(outcome: &revenant_ascension::run::RunOutcome) {
    println!("── actuator outcome ──");
    println!(
        "build={} test={} clippy={} · files={:?}",
        outcome.build_ok, outcome.test_ok, outcome.clippy_ok, outcome.changed_files
    );
    if let Some(approved) = outcome.reviewer_approved {
        println!("reviewer approved: {approved}");
    }
    if let Some(offer) = &outcome.offer {
        println!("offer: {offer}");
    }
    for n in &outcome.notes {
        println!("· {n}");
    }
}

async fn cmd_eval(
    suite_dir: Option<PathBuf>,
    json_out: Option<PathBuf>,
    tag: Option<String>,
    agent: bool,
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
        None if agent => revenant_evals::agent_suite(),
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

/// Create the runtime tree, ship bundled skills/subagents/personas, write the
/// default config + bearer token. Idempotent. Returns whether the config was
/// freshly created (i.e. this looks like a first run). Shared by `init` and the
/// first-run `setup` wizard.
fn scaffold_fs(home: &Home) -> Result<bool> {
    std::fs::create_dir_all(home.root())?;
    std::fs::create_dir_all(home.gateway_bin_dir())?;
    std::fs::create_dir_all(home.workspace_dir())?;
    std::fs::create_dir_all(home.skills_dir())?;
    std::fs::create_dir_all(home.plugins_dir())?;
    std::fs::create_dir_all(home.identity_dir())?;
    std::fs::create_dir_all(home.logs_dir())?;

    // Ship the skill-creator meta-skill on fresh installs so the agent knows
    // how to author its own skills well.
    let creator_dir = home.skills_dir().join("skill-creator");
    if !creator_dir.join("SKILL.md").exists() {
        std::fs::create_dir_all(&creator_dir)?;
        std::fs::write(creator_dir.join("SKILL.md"), SKILL_CREATOR)?;
    }

    // Ship the loop-engineering skill + a critic subagent so nested quality
    // loops (produce → critique → refine) work out of the box.
    let loop_skill_dir = home.skills_dir().join("quality-loop");
    if !loop_skill_dir.join("SKILL.md").exists() {
        std::fs::create_dir_all(&loop_skill_dir)?;
        std::fs::write(loop_skill_dir.join("SKILL.md"), QUALITY_LOOP_SKILL)?;
    }
    let critic_path = home.agents_dir().join("critic.md");
    if !critic_path.exists() {
        std::fs::create_dir_all(home.agents_dir())?;
        std::fs::write(&critic_path, CRITIC_AGENT)?;
    }

    // Ship the autoresearch skill so web_search + web_fetch are used with a
    // real methodology (fan out, cross-check, cite) rather than one-shot.
    let research_dir = home.skills_dir().join("autoresearch");
    if !research_dir.join("SKILL.md").exists() {
        std::fs::create_dir_all(&research_dir)?;
        std::fs::write(research_dir.join("SKILL.md"), AUTORESEARCH_SKILL)?;
    }

    // Ship a few built-in personalities (voice layer). Idempotent: add any
    // missing built-in (so upgrades get new voices) without clobbering edits.
    let pdir = home.personalities_dir();
    std::fs::create_dir_all(&pdir)?;
    for (file, body) in BUILTIN_PERSONAS {
        if !pdir.join(file).exists() {
            std::fs::write(pdir.join(file), body)?;
        }
    }

    let config_path = home.config_path();
    let fresh = !config_path.exists();
    if fresh {
        std::fs::write(&config_path, Config::default_config().to_toml())?;
    }

    // Control-plane bearer token: required even on loopback.
    let token_path = home.root().join("token");
    if !token_path.exists() {
        use rand::RngCore;
        let mut bytes = [0u8; 32];
        rand::thread_rng().fill_bytes(&mut bytes);
        std::fs::write(&token_path, hex::encode(bytes))?;
        set_mode_0600(&token_path)?;
    }
    Ok(fresh)
}

/// Fetch the one-time heavy assets: the supervised gateway binary and (if
/// memory uses the built-in embedder) the ~35MB embedding model. Both are
/// checksum-verified and cached; re-running is cheap.
async fn ensure_downloads(home: &Home, cfg: &Config) -> Result<()> {
    if cfg.gateway.mode == GatewayMode::Bundled {
        let bin = revenant_gateway::ensure_binary(home, cfg).await?;
        println!("gateway binary ready: {}", bin.display());
    }
    if cfg.memory.enabled && cfg.memory.embedder == revenant_core::config::EmbedderKind::Builtin {
        let dir = revenant_memory::embed::ensure_builtin_model(&home.models_dir()).await?;
        println!("embedding model ready: {}", dir.display());
    }
    Ok(())
}

/// True once a usable Anthropic key is present in secrets.env.
fn has_anthropic_key(home: &Home) -> bool {
    std::fs::read_to_string(home.secrets_path())
        .map(|s| {
            s.lines().any(|l| {
                let l = l.trim();
                !l.starts_with('#')
                    && l.starts_with("ANTHROPIC_API_KEY=")
                    && l.trim_end().len() > "ANTHROPIC_API_KEY=".len() + 3
            })
        })
        .unwrap_or(false)
}

/// Read one line from stdin with a prompt; returns the trimmed input.
fn prompt(msg: &str) -> Result<String> {
    print!("{msg}");
    std::io::stdout().flush()?;
    let mut buf = String::new();
    std::io::stdin().read_line(&mut buf)?;
    Ok(buf.trim().to_string())
}

async fn cmd_init() -> Result<()> {
    let home = Home::resolve();
    scaffold_fs(&home)?;
    println!("wrote {}", home.config_path().display());

    let secrets_path = home.secrets_path();
    if !secrets_path.exists() {
        let key = prompt("Anthropic API key (sk-ant-..., blank to skip): ")?;
        let body = if key.is_empty() {
            "# Add provider keys here, e.g.\n# ANTHROPIC_API_KEY=sk-ant-...\n".to_string()
        } else {
            format!("ANTHROPIC_API_KEY={key}\n")
        };
        std::fs::write(&secrets_path, body)?;
        set_mode_0600(&secrets_path)?;
        println!("wrote {} (0600)", secrets_path.display());
    }

    let cfg = load_config(&home)?;
    ensure_downloads(&home, &cfg).await?;
    println!("\n\x1b[1;35mrevenant\x1b[0m is raised. `revenant chat` to give it its first words,\n`revenant up` to bind it to the machine, `revenant open` for the web UI.\nFor the full house voice: `/persona revenant` in chat. It does not sleep.");
    Ok(())
}

/// The turnkey first-run experience: `revenant` (bare) on a fresh box, or
/// `revenant setup` any time. Asks only what can't be inferred — how you'll pay
/// — writes everything, fetches the one-time assets, and drops you straight
/// into your first conversation. No TOML, no docs, no daemon to babysit.
async fn cmd_setup() -> Result<()> {
    let home = Home::resolve();
    println!(
        "\n\x1b[1;35m🜁 revenant\x1b[0m — the agent that comes back.\n\
         Let's get you talking to it. This takes about a minute, once.\n"
    );

    let fresh = scaffold_fs(&home)?;
    if fresh {
        println!("✓ created {}", home.root().display());
    } else {
        println!("✓ using existing setup at {}", home.root().display());
    }

    // The one real decision: how does it think? Everything else is inferred.
    if !has_anthropic_key(&home) {
        println!(
            "\n\x1b[1mHow should your revenant think?\x1b[0m\n\
             \x20 1) Anthropic (Claude) — recommended, most capable\n\
             \x20 2) Skip for now — add a key later; chat will tell you what's missing\n"
        );
        let choice = prompt("Choose [1]: ")?;
        let mut body =
            "# Provider keys. Keep this file private (0600).\n".to_string();
        if choice != "2" {
            println!(
                "\nGrab a key (starts with sk-ant-) at:\n  \x1b[4mhttps://console.anthropic.com/settings/keys\x1b[0m\n"
            );
            let key = prompt("Paste your Anthropic API key (or blank to skip): ")?;
            if key.is_empty() {
                body.push_str("# ANTHROPIC_API_KEY=sk-ant-...\n");
                println!("No key yet — that's fine. Run `revenant setup` again when you have one.");
            } else {
                body.push_str(&format!("ANTHROPIC_API_KEY={key}\n"));
                println!("✓ key saved");
            }
        } else {
            body.push_str("# ANTHROPIC_API_KEY=sk-ant-...\n");
        }
        std::fs::write(home.secrets_path(), body)?;
        set_mode_0600(&home.secrets_path())?;
    } else {
        println!("✓ provider key already set");
    }

    // Progressive disclosure: pick how much surface to show by default. Novice
    // → clean web UI + everyday CLI; power user → advanced tabs revealed. The
    // web toggle still overrides per-browser; this just sets the starting point.
    println!(
        "\n\x1b[1mHow will you use it?\x1b[0m\n\
         \x20 1) Just chatting — keep it simple (recommended)\n\
         \x20 2) Power user — show loops, subagents, spend, memory, ascension up front\n"
    );
    let power_user = prompt("Choose [1]: ")? == "2";
    {
        let mut cfg = load_config(&home)?;
        cfg.experience.power_user = power_user;
        std::fs::write(home.config_path(), cfg.to_toml())?;
    }
    println!("✓ {}", if power_user { "power-user mode — everything visible" } else { "simple mode — advanced features are one toggle away" });

    let cfg = load_config(&home)?;
    println!("\nFetching the gateway and memory model (one-time)…");
    ensure_downloads(&home, &cfg).await?;

    if !has_anthropic_key(&home) {
        println!(
            "\n\x1b[1;33mHeads up:\x1b[0m no provider key yet, so replies won't work until you add one.\n\
             Run \x1b[1mrevenant setup\x1b[0m again, or put ANTHROPIC_API_KEY in {}.",
            home.secrets_path().display()
        );
        println!("\nWhen you're ready: \x1b[1mrevenant\x1b[0m starts chatting.");
        return Ok(());
    }

    println!(
        "\n\x1b[1;32m✓ You're set.\x1b[0m Starting your first conversation — just type.\n\
         (Tips: /help for commands · /persona revenant for the full house voice · Ctrl-C to leave.)\n"
    );
    repl::cmd_chat(None).await
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

async fn cmd_doctor() -> Result<()> {
    let home = Home::resolve();
    println!("🩺 revenant doctor — checking your setup\n");
    let ok = |l: &str, d: &str| println!("  ✅ {l}{}", if d.is_empty() { String::new() } else { format!(" — {d}") });
    let warn = |l: &str, d: &str| println!("  ⚠️  {l} — {d}");
    let bad = |l: &str, d: &str| println!("  ❌ {l} — {d}");

    // Config
    let cfg = match load_config(&home) {
        Ok(c) => {
            ok("config", &home.root().join("config.toml").display().to_string());
            Some(c)
        }
        Err(e) => {
            bad("config", &format!("{e:#}"));
            None
        }
    };

    // Secrets / provider keys
    let secrets = std::fs::read_to_string(home.root().join("secrets.env")).unwrap_or_default();
    let present: Vec<&str> = [
        "ANTHROPIC_API_KEY", "OPENAI_API_KEY", "OPENROUTER_API_KEY", "GROQ_API_KEY",
    ]
    .into_iter()
    .filter(|k| secrets.contains(k))
    .collect();
    if secrets.is_empty() {
        bad("secrets.env", "missing — add a provider key, e.g. ANTHROPIC_API_KEY=sk-ant-...");
    } else if present.is_empty() {
        warn("secrets.env", "present, but no known provider API key found");
    } else {
        ok("provider keys", &present.join(", "));
    }

    // Gateway binary
    let gw_ok = std::fs::read_dir(home.gateway_bin_dir())
        .ok()
        .map(|rd| {
            rd.filter_map(|e| e.ok())
                .any(|e| e.file_name().to_string_lossy().contains("agentgateway"))
        })
        .unwrap_or(false);
    if gw_ok {
        ok("gateway binary", "installed");
    } else {
        warn("gateway binary", "not found — downloads automatically on first `revenant up`");
    }

    // Daemon
    let client = revenant_client::Client::from_env(&home).ok();
    let daemon_up = match &client {
        Some(c) => c.health().await.is_ok(),
        None => false,
    };
    if daemon_up {
        ok("daemon", "running");
    } else {
        warn("daemon", "not running — start it with `revenant up`");
    }

    // Provider credit / key check (the silent killer) — only if daemon is up.
    if daemon_up {
        if let Some(cfg) = &cfg {
            let llm = revenant_llm::LlmClient::new(format!("http://127.0.0.1:{}", cfg.gateway.llm_port));
            let model = if cfg.tiers.contains_key("fast") {
                "fast".to_string()
            } else {
                cfg.tiers.keys().next().cloned().unwrap_or_else(|| "fast".to_string())
            };
            match llm.ping(&model).await {
                Ok(()) => ok("provider reachable", &format!("'{model}' tier responded — keys + credit OK")),
                Err(e) => bad("provider", &format!("{e}")),
            }
        }
    } else {
        warn("provider credit", "can't check while the daemon is down (start it, then re-run)");
    }

    // Telegram
    if let Some(cfg) = &cfg {
        let tg = &cfg.channels.telegram;
        if tg.enabled && secrets.contains(&tg.token_env) {
            ok("telegram", &format!("enabled ({} present)", tg.token_env));
        } else if tg.enabled {
            warn("telegram", &format!("enabled but {} not in secrets.env", tg.token_env));
        } else {
            ok("telegram", "disabled");
        }
    }

    // Network / the horde
    if let Some(cfg) = &cfg {
        match &cfg.network.necropolis_url {
            Some(u) => ok("network", &format!("Necropolis: {u}")),
            None => warn("network", "no Necropolis set — set network.necropolis_url to join the horde"),
        }
    }

    println!("\n  Legend: ✅ good · ⚠️ optional/attention · ❌ must fix");
    println!("  Next: `revenant up` runs everything; `revenant chat` to talk; `revenant doctor` to re-check.");
    Ok(())
}

const UPDATE_REPO: &str = "themsquared/revenant";

fn update_triple() -> Option<&'static str> {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("macos", "aarch64") => Some("aarch64-apple-darwin"),
        ("linux", "x86_64") => Some("x86_64-unknown-linux-musl"),
        ("linux", "aarch64") => Some("aarch64-unknown-linux-musl"),
        _ => None,
    }
}

/// Pull a `"key":"value"` string out of a JSON blob without a full parser.
fn sha256_file(path: &std::path::Path) -> Result<String> {
    let run = |cmd: &str, args: &[&str]| -> Option<String> {
        let out = std::process::Command::new(cmd).args(args).arg(path).output().ok()?;
        out.status
            .success()
            .then(|| String::from_utf8_lossy(&out.stdout).split_whitespace().next().map(String::from))
            .flatten()
    };
    run("shasum", &["-a", "256"])
        .or_else(|| run("sha256sum", &[]))
        .context("need `shasum` or `sha256sum` to verify the download")
}

fn install_bin(src: &std::path::Path, dst: &std::path::Path) -> Result<()> {
    if std::fs::rename(src, dst).is_err() {
        std::fs::copy(src, dst).with_context(|| format!("installing {}", dst.display()))?;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(dst, std::fs::Permissions::from_mode(0o755));
    }
    Ok(())
}

/// Parse a CalVer tag (`2026.7.0`, `v2026.7`, `2026.7.1`) into (year,month,patch).
/// Requires a plausible year+month so a stray semver tag (0.1.0) never matches.
///
/// Any SemVer-style prerelease/build suffix is stripped before parsing, so a
/// rolling `main`-channel tag like `v2026.7.412-main.abc1234` parses to its
/// core `(2026, 7, 412)` — the same shape as a stable tag. Rolling builds carry
/// a monotonic commit-count patch (see the main-prerelease workflow) so two
/// builds in the same month compare as distinct, which is what lets
/// `resolve_update_target`/`cmd_update` detect a newer rolling build.
pub(crate) fn parse_calver(tag: &str) -> Option<(u32, u32, u32)> {
    let core = tag.trim().trim_start_matches('v');
    // Drop `-prerelease` / `+build` metadata; the CalVer core is what orders.
    let core = core.split(['-', '+']).next().unwrap_or(core);
    let mut it = core.split('.');
    let year: u32 = it.next()?.parse().ok()?;
    let month: u32 = it.next()?.parse().ok()?;
    let patch: u32 = it.next().and_then(|p| p.parse().ok()).unwrap_or(0);
    if (2024..=2100).contains(&year) && (1..=12).contains(&month) {
        Some((year, month, patch))
    } else {
        None
    }
}

fn channel_label(c: UpdateChannel) -> &'static str {
    match c {
        UpdateChannel::YearMonth => "year.month (stable)",
        UpdateChannel::Main => "main (rolling)",
        UpdateChannel::Manual => "manual",
    }
}

/// Resolve the newest release tag for a channel. year_month/manual take the
/// highest STABLE CalVer release; main also considers prereleases (where
/// main-branch/nightly builds land). Drafts are ignored.
fn resolve_update_target(channel: UpdateChannel) -> Result<Option<String>> {
    let api = format!("https://api.github.com/repos/{UPDATE_REPO}/releases?per_page=50");
    let out = std::process::Command::new("curl")
        .args(["-fsSL", "-H", "Accept: application/vnd.github+json", "-H", "User-Agent: revenant", &api])
        .output()
        .context("querying GitHub releases (is curl installed / are you online?)")?;
    if !out.status.success() {
        bail!("could not reach GitHub releases: {}", String::from_utf8_lossy(&out.stderr));
    }
    let rels: Vec<serde_json::Value> =
        serde_json::from_slice(&out.stdout).context("parsing GitHub releases list")?;
    Ok(select_update_target(&rels, channel))
}

/// Pick the newest eligible release tag from a GitHub releases payload.
/// Drafts are always ignored; stable channels also skip prereleases, while
/// `main` rides them (that's where per-commit rolling builds land). Highest
/// CalVer wins — for `main` the monotonic commit-count patch orders the builds.
fn select_update_target(rels: &[serde_json::Value], channel: UpdateChannel) -> Option<String> {
    let mut best: Option<((u32, u32, u32), String)> = None;
    for r in rels {
        if r["draft"].as_bool().unwrap_or(false) {
            continue;
        }
        let prerelease = r["prerelease"].as_bool().unwrap_or(false);
        // Stable channels skip prereleases; main rides them.
        if channel != UpdateChannel::Main && prerelease {
            continue;
        }
        let tag = r["tag_name"].as_str().unwrap_or("");
        if let Some(cv) = parse_calver(tag) {
            if best.as_ref().is_none_or(|(b, _)| cv > *b) {
                best = Some((cv, tag.to_string()));
            }
        }
    }
    best.map(|(_, t)| t)
}

fn cmd_update(check: bool) -> Result<()> {
    let triple = update_triple().context("auto-update isn't supported on this platform yet")?;
    let home = Home::resolve();
    let channel = load_config(&home).map(|c| c.update.channel).unwrap_or_default();
    // The installed release is recorded in ~/.revenant/release on each update;
    // absent on a from-source build, in which case any release is an upgrade.
    let release_marker = home.root().join("release");
    let current_tag = std::fs::read_to_string(&release_marker)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    let current_cv = current_tag.as_deref().and_then(parse_calver).unwrap_or((0, 0, 0));
    println!("channel: {}  ({triple})", channel_label(channel));
    println!("current: {}", current_tag.as_deref().unwrap_or("(built from source / no channel release yet)"));

    let Some(tag) = resolve_update_target(channel)? else {
        println!("\nno releases on the {} channel yet.", channel_label(channel));
        return Ok(());
    };
    println!("latest:  {tag}");
    if parse_calver(&tag).unwrap_or((0, 0, 0)) <= current_cv {
        println!("\n✅ already on the latest {} release.", channel_label(channel));
        return Ok(());
    }
    println!("\n⬆️  update available: {} → {tag}", current_tag.as_deref().unwrap_or("(source)"));
    if check {
        println!("run `revenant update` to install it.");
        return Ok(());
    }

    // Download tarball + checksums.
    let tmp = std::env::temp_dir().join(format!("revenant-update-{}", std::process::id()));
    std::fs::create_dir_all(&tmp)?;
    let base = format!("https://github.com/{UPDATE_REPO}/releases/download/{tag}");
    let tarball = format!("revenant-{tag}-{triple}.tar.gz");
    for f in [tarball.as_str(), "SHA256SUMS"] {
        let dst = tmp.join(f);
        let s = std::process::Command::new("curl")
            .args(["-fsSL", "-o", &dst.to_string_lossy(), &format!("{base}/{f}")])
            .status()
            .context("downloading release asset")?;
        if !s.success() {
            bail!("download failed for {f}");
        }
    }

    // Verify checksum before trusting a byte.
    let sums = std::fs::read_to_string(tmp.join("SHA256SUMS"))?;
    let want = sums
        .lines()
        .find(|l| l.trim_end().ends_with(&tarball))
        .and_then(|l| l.split_whitespace().next())
        .context("release checksum for this platform not found")?;
    let got = sha256_file(&tmp.join(&tarball))?;
    if !got.eq_ignore_ascii_case(want) {
        bail!("checksum mismatch — refusing to install (want {want}, got {got})");
    }
    println!("✓ checksum verified");

    let s = std::process::Command::new("tar")
        .args(["-xzf", &tmp.join(&tarball).to_string_lossy(), "-C", &tmp.to_string_lossy()])
        .status()
        .context("extracting release")?;
    if !s.success() {
        bail!("extract failed");
    }
    let new_bin = tmp.join("revenant");
    if !new_bin.exists() {
        bail!("archive has no `revenant` binary");
    }

    // Atomic-ish swap with a restorable backup.
    let exe = std::env::current_exe()?;
    let bak = exe.with_extension("bak");
    let _ = std::fs::remove_file(&bak);
    std::fs::rename(&exe, &bak).context("backing up the current binary")?;
    if let Err(e) = install_bin(&new_bin, &exe) {
        let _ = std::fs::rename(&bak, &exe); // restore on failure
        bail!("install failed (previous binary restored): {e}");
    }
    // Update the sibling TUI binary too, if the archive shipped one.
    if let Some(dir) = exe.parent() {
        let tui = tmp.join("revenant-tui");
        if tui.exists() {
            let _ = install_bin(&tui, &dir.join("revenant-tui"));
        }
    }
    let _ = std::fs::remove_dir_all(&tmp);
    // Record the installed release so the next `update` can compare CalVer.
    let _ = std::fs::write(&release_marker, &tag);
    println!("\n✅ updated to {tag}. Restart to run it: `revenant up` (or restart the service).");
    println!("   previous binary kept at {}", bak.display());
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

async fn cmd_spend(window: String) -> Result<()> {
    let home = Home::resolve();
    let cfg = load_config(&home)?;
    let client = revenant_client::Client::from_env(&home)?;
    client
        .health()
        .await
        .context("spend needs a running daemon — start it with `revenant up`")?;
    let rows = client.spend(&window).await?;
    if rows.is_empty() {
        println!("no spend recorded in window '{window}'.");
        return Ok(());
    }
    let priced = !cfg.pricing.is_empty();
    println!("🜁 spend · {window}\n");
    let (mut tin, mut tout, mut treq, mut tcost) = (0i64, 0i64, 0i64, 0.0f64);
    let mut any_unpriced = false;
    for r in &rows {
        tin += r.tokens_in;
        tout += r.tokens_out;
        treq += r.requests;
        let cost = cfg.pricing.get(&r.model).map(|p| {
            r.tokens_in as f64 / 1e6 * p.input_per_mtok + r.tokens_out as f64 / 1e6 * p.output_per_mtok
        });
        let cost_s = match cost {
            Some(c) => {
                tcost += c;
                format!("  ${c:.4}")
            }
            None if priced => {
                any_unpriced = true;
                "  (no price)".to_string()
            }
            None => String::new(),
        };
        println!(
            "  {:<32} {:>10} in / {:>10} out · {:>4} req{}",
            r.model, r.tokens_in, r.tokens_out, r.requests, cost_s
        );
    }
    println!(
        "\n  total: {tin} in / {tout} out · {treq} req{}",
        if priced { format!(" · ${tcost:.4}") } else { String::new() }
    );
    if !priced {
        println!("\n  Set [pricing] in config.toml (model → input_per_mtok / output_per_mtok, USD)\n  to see dollar cost, not just tokens.");
    } else if any_unpriced {
        println!("\n  Some models have no [pricing] entry — add them for a complete total.");
    }
    if cfg.spending.enabled {
        println!(
            "\n  budget cap: {} {} per {} (gateway-enforced)",
            cfg.spending.budget,
            format!("{:?}", cfg.spending.count).to_lowercase(),
            cfg.spending.interval,
        );
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

#[cfg(test)]
mod tests {
    use super::{parse_calver, select_update_target};
    use revenant_core::config::UpdateChannel;

    // A GitHub /releases payload shaped like what the API returns: newest first,
    // a stable release plus two rolling `-main.<sha>` prereleases and a draft.
    fn sample_releases() -> Vec<serde_json::Value> {
        serde_json::json!([
            {"tag_name": "v2026.7.500-main.ffffff0", "prerelease": true, "draft": true},
            {"tag_name": "v2026.7.412-main.bbbbbbb", "prerelease": true, "draft": false},
            {"tag_name": "v2026.7.410-main.aaaaaaa", "prerelease": true, "draft": false},
            {"tag_name": "v2026.7.0", "prerelease": false, "draft": false},
        ])
        .as_array()
        .unwrap()
        .clone()
    }

    #[test]
    fn main_channel_offers_newest_rolling_prerelease() {
        // The task's E2E requirement: on the main channel, update resolution
        // finds the latest rolling prerelease (not the stable release, not the draft).
        let got = select_update_target(&sample_releases(), UpdateChannel::Main);
        assert_eq!(got.as_deref(), Some("v2026.7.412-main.bbbbbbb"));
    }

    #[test]
    fn stable_channels_ignore_rolling_prereleases() {
        for chan in [UpdateChannel::YearMonth, UpdateChannel::Manual] {
            let got = select_update_target(&sample_releases(), chan);
            assert_eq!(got.as_deref(), Some("v2026.7.0"), "channel {chan:?} should skip prereleases");
        }
    }

    #[test]
    fn parses_stable_calver() {
        assert_eq!(parse_calver("v2026.7.0"), Some((2026, 7, 0)));
        assert_eq!(parse_calver("2026.7.3"), Some((2026, 7, 3)));
        assert_eq!(parse_calver("v2026.7"), Some((2026, 7, 0))); // implicit patch 0
    }

    #[test]
    fn parses_rolling_main_prerelease() {
        // v<YEAR>.<MONTH>.<COUNT>-main.<shortsha> from the main-prerelease workflow.
        assert_eq!(parse_calver("v2026.7.412-main.abc1234"), Some((2026, 7, 412)));
        // Build metadata (`+`) is stripped too.
        assert_eq!(parse_calver("v2026.7.0-main.deadbee+ci"), Some((2026, 7, 0)));
    }

    #[test]
    fn rolling_builds_order_monotonically_within_a_month() {
        // The whole point of the commit-count patch: two same-month rolling
        // builds must compare as distinct so `update --check` sees the newer one.
        let older = parse_calver("v2026.7.411-main.aaaaaaa").unwrap();
        let newer = parse_calver("v2026.7.412-main.bbbbbbb").unwrap();
        assert!(newer > older);
    }

    #[test]
    fn rejects_non_calver_tags() {
        assert_eq!(parse_calver("v0.1.0"), None); // year 0 out of range
        assert_eq!(parse_calver("main"), None);
        assert_eq!(parse_calver("v2026.13.0"), None); // month out of range
    }
}
