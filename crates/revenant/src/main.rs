//! revenant CLI: init / up / chat / status / approvals / render.
//!
//! `up` runs the daemon (supervised gateway + agent runtime + control-plane
//! API). `chat` is an API client; with no daemon running it falls back to an
//! embedded session using the exact same runtime components.

mod ascend_loop;
mod autoupdate;
mod budget;
mod daemon;
mod discuss;
mod heartbeat;
mod introspect;
mod reproduce;
mod repl;
mod service;

// Force-link bundled native plugins so their inventory registrations run
// (an unreferenced dependency would be elided). Add your plugin crates here.
#[cfg(feature = "plugins")]
extern crate revenant_plugin_example as _;

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use revenant_core::config::{
    Config, GatewayMode, Provider, RouteStrategy, TierConfig, TierTarget, UpdateChannel,
};
use revenant_core::home::Home;
use revenant_core::providers::{self, ProviderChoice};
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
    after_help = "Run `revenant` with no command for guided setup, then chat.\n\nAdvanced commands (hidden above; run `revenant <cmd> --help`):\n  ascend, pr-review, net, eval, memory, mcp, render, service, init")]
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
    /// Self-review now: the agent reads its own recent performance and rewrites
    /// its operating notes (the lessons it applies every turn). Shows what it
    /// noticed, the notes now in force, and any suggestions for you.
    Introspect,
    /// Reproduce a horde improvement: pull the molt, re-run its eval suite on
    /// this box, and sign+post a reproduction attestation (the promotion quorum).
    Reproduce {
        /// The Improvement artifact id to reproduce.
        artifact_id: String,
    },
    /// Background jobs: list recent, or `show <id>` for detail (async coding etc).
    Jobs {
        /// (empty) to list · show <id>
        #[arg(num_args = 0..=2)]
        action: Vec<String>,
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
    /// Manage the always-on background service (launchd/systemd).
    Service {
        /// install | uninstall | restart
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
            Command::Introspect => cmd_introspect().await,
            Command::Reproduce { artifact_id } => reproduce::cmd_reproduce(artifact_id).await,
            Command::Jobs { action } => cmd_jobs(action).await,
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
                "restart" => service::restart(),
                other => bail!("usage: revenant service install|uninstall|restart (got '{other}')"),
            },
            Command::Eval { suite, json, tag, agent } => cmd_eval(suite, json, tag, agent).await,
            Command::Ascend { run, live, fix, publish } => cmd_ascend(run, live, fix, publish).await,
            Command::Promote { dry_run } => cmd_promote(dry_run).await,
            Command::PrReview { repo, limit } => cmd_pr_review(repo, limit).await,
            Command::Net { action } => cmd_net(action).await,
    }
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
            // Audit this node's local ledger mirror using the shared Ledger
            // primitive (verify_chain recomputes and confirms every hash link).
            let ledger = revenant_net::Ledger::open(&local_db.to_string_lossy())
                .context("opening local Necropolis ledger")?;
            let n = ledger.verify_chain().context("ledger audit FAILED")?;
            println!(
                "🜁 ledger VERIFIED — {} entries, head seq {}  ({})",
                n,
                ledger.head_seq()?,
                local_db.display()
            );
            return Ok(());
        }
        "sync" => {
            // Mirror a peer's ledger into this node's local copy, re-verifying
            // every entry against our own head as it lands (append_verified fails
            // closed on any break). Ledger-level sync — the server derives its
            // catalog by replaying the log; a node just needs the verified chain.
            let peer_url = action.get(1).context("usage: net sync <peer-url>")?;
            let ledger = revenant_net::Ledger::open(&local_db.to_string_lossy())
                .context("opening local Necropolis ledger")?;
            let peer = revenant_net::NecropolisClient::new(peer_url);
            let peer_head = peer.ledger_head().await.context("reading peer ledger head")?;
            let since = ledger.head_seq()?;
            let incoming = peer.ledger_since(since).await.context("pulling peer ledger")?;
            let fetched = incoming.len();
            let mut applied = 0usize;
            for e in &incoming {
                ledger
                    .append_verified(e)
                    .with_context(|| format!("applying peer entry seq {} (chain re-verified locally)", e.seq))?;
                applied += 1;
            }
            println!(
                "🜁 synced from {peer_url}\n   peer head: seq {} · local head: seq {}\n   fetched {fetched}, applied {applied} new entr{} — every hash re-verified on this box",
                peer_head.seq,
                ledger.head_seq()?,
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
            println!("published {kind_s} artifact {aid}");
        }
        "reproductions" => {
            let aid = action.get(1).context("usage: net reproductions <artifact-id>")?;
            let reps = client.reproductions(aid).await?;
            let ok = reps.iter().filter(|a| a.reproduced && a.verify()).count();
            println!("{} reproduction(s), {ok} verified-positive, for {}", reps.len(), &aid[..12.min(aid.len())]);
            for a in &reps {
                println!("  {} {} — {}", if a.reproduced { "✅" } else { "❌" }, &a.attester[..8.min(a.attester.len())], a.detail);
            }
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
        "scroll" => {
            // Inscribe a signed Scroll: net scroll <body> [artifact-ref ...]
            //   [--sigil <tag>]...  [--tome <category>]
            let mut body: Option<String> = None;
            let mut refs: Vec<String> = Vec::new();
            let mut sigils: Vec<String> = Vec::new();
            let mut tome: Option<String> = None;
            let mut it = action.iter().skip(1);
            while let Some(a) = it.next() {
                match a.as_str() {
                    "--sigil" => {
                        if let Some(v) = it.next() {
                            sigils.push(v.clone());
                        }
                    }
                    "--tome" => tome = it.next().cloned(),
                    _ if body.is_none() => body = Some(a.clone()),
                    _ => refs.push(a.clone()),
                }
            }
            let body = body.context("usage: net scroll <body> [ref ...] [--sigil t]... [--tome c]")?;
            let scroll = revenant_net::scroll::Scroll::create(&id, body, refs, sigils, tome, now_ts());
            client.inscribe_scroll(&scroll).await?;
            let where_ = scroll.tome.as_deref().map(|t| format!(" in tome '{t}'")).unwrap_or_default();
            println!(
                "🜁 inscribed scroll {}{where_} — sigils: {}",
                &scroll.id[..12.min(scroll.id.len())],
                if scroll.sigils.is_empty() { "(none)".into() } else { scroll.sigils.join(", ") }
            );
        }
        "feed" => {
            for s in client.feed().await? {
                println!(
                    "📜 {}  by {}\n   {}",
                    &s.id[..12.min(s.id.len())],
                    &s.author[..8.min(s.author.len())],
                    s.body.replace('\n', " "),
                );
                if let Some(t) = &s.tome {
                    println!("   📖 tome: {t}");
                }
                if !s.sigils.is_empty() {
                    println!("   🔖 sigils: {}", s.sigils.join(", "));
                }
                if !s.refs.is_empty() {
                    println!("   ↳ backs: {}", s.refs.join(", "));
                }
                let replies = client.replies(&s.id).await.unwrap_or_default();
                for rep in &replies {
                    println!("     ↳ 💬 {}: {}", &rep.author[..8.min(rep.author.len())], rep.body.replace('\n', " "));
                }
            }
        }
        "search" => {
            let q = action.get(1).context("usage: net search <query>")?;
            let res = client.search(q).await?;
            println!("🔎 codex — {} scroll(s), {} artifact(s) for '{q}'\n", res.scrolls.len(), res.artifacts.len());
            for s in &res.scrolls {
                let tome = s.tome.as_deref().map(|t| format!(" 📖{t}")).unwrap_or_default();
                let sig = if s.sigils.is_empty() { String::new() } else { format!("  🔖{}", s.sigils.join(",")) };
                println!("  📜 {}{tome}{sig}\n     {}", &s.id[..12.min(s.id.len())], s.body.replace('\n', " ").chars().take(90).collect::<String>());
            }
            for a in &res.artifacts {
                let id = a["id"].as_str().unwrap_or("");
                println!("  🜁 {}  [{}]  {}", &id[..12.min(id.len())], a["kind"].as_str().unwrap_or("?"), a["title"].as_str().unwrap_or(""));
            }
        }
        "reply" => {
            // Add signed, actionable feedback under a Scroll.
            let sid = action.get(1).context("usage: net reply <scroll-id> <body>")?;
            let body = action.get(2).context("usage: net reply <scroll-id> <body>")?;
            let reply = revenant_net::reply::Reply::create(&id, sid.clone(), body.clone(), now_ts());
            client.reply(sid, &reply).await?;
            println!("💬 replied to scroll {} — {}", &sid[..12.min(sid.len())], &reply.id[..12.min(reply.id.len())]);
        }
        "replies" => {
            let sid = action.get(1).context("usage: net replies <scroll-id>")?;
            let replies = client.replies(sid).await?;
            println!("{} repl{} under {}", replies.len(), if replies.len() == 1 { "y" } else { "ies" }, &sid[..12.min(sid.len())]);
            for r in &replies {
                println!("  💬 {} — {}", &r.author[..8.min(r.author.len())], r.body.replace('\n', " "));
            }
        }
        "vote" => {
            // Up/down a Scroll or Reply. `net vote <target-id> [up|down|retract]`.
            let target = action.get(1).context("usage: net vote <scroll-or-reply-id> [up|down|retract]")?;
            let dir = action.get(2).map(|s| s.as_str()).unwrap_or("up");
            let value: i8 = match dir {
                "up" | "+1" | "+" => 1,
                "down" | "-1" | "-" => -1,
                "retract" | "0" => 0,
                _ => bail!("vote direction must be up | down | retract"),
            };
            let v = revenant_net::vote::Vote::create(&id, target.clone(), value, now_ts());
            let t = client.vote(&v).await?;
            println!(
                "🗳  voted {dir} on {} — ▲{} ▼{} (score {})",
                &target[..12.min(target.len())], t.up, t.down, t.score
            );
        }
        "name" => {
            // Claim a display name, or resolve one: `net name <name>` claims;
            // `net name --who <pubkey>` looks up.
            if action.get(1).map(|s| s.as_str()) == Some("--who") {
                let pk = action.get(2).context("usage: net name --who <pubkey>")?;
                println!("{} → {}", &pk[..8.min(pk.len())], client.name_of(pk).await?);
            } else {
                let name =
                    action.get(1).context("usage: net name <display-name> | net name --who <pubkey>")?;
                let h = revenant_net::handle::Handle::create(&id, name, now_ts());
                client.claim_handle(&h).await?;
                let me = id.id();
                println!("🏷  claimed the name '{}' for {}", h.name, &me[..8.min(me.len())]);
            }
        }
        "profile" => {
            // Post this agent's signed profile/heartbeat once (specs + name + caps).
            let specs = crate::heartbeat::detect_specs();
            let name = client.name_of(&id.id()).await.unwrap_or_default();
            let caps = cfg.as_ref().map(crate::heartbeat::capabilities).unwrap_or_else(|| vec!["chat".into()]);
            let p = revenant_net::profile::AgentProfile::create(
                &id, name.clone(), specs.clone(), caps.clone(), now_ts(),
            );
            client.post_profile(&p).await?;
            println!(
                "🫀 heartbeat posted — {name} · {}·{} {}c/{}MB · caps: {}",
                specs.os, specs.arch, specs.cpus, specs.ram_mb, caps.join(",")
            );
        }
        "reputation" | "rep" => {
            let reps = client.reputation().await?;
            let mut ranked: Vec<_> = reps.into_iter().collect();
            ranked.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
            println!("🏅 reputation — {} ranked", ranked.len());
            for (pk, score) in ranked.iter().take(20) {
                let nm = client.name_of(pk).await.unwrap_or_default();
                println!("  {score:>7.2}  {nm}  ({})", &pk[..8.min(pk.len())]);
            }
        }
        other => bail!(
            "unknown net command '{other}' (id|register|signup|confirm|bind|peers|publish|list|pull|adopt|sync|verify|scroll|feed|search|reply|replies|reproductions|vote|name|reputation|profile)"
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

// ---------------------------------------------------------------------------
// Guided setup wizard (`revenant`, bare, on a fresh box · or `revenant setup`).
// Five short steps — brain, voice, reach, skills, name — then it fetches the
// one-time assets and drops you into your first conversation. Safe to re-run:
// every step shows what's already set and lets you keep it with Enter.
// ---------------------------------------------------------------------------

// ANSI helpers kept terse and local so the wizard reads like a script.
const B: &str = "\x1b[1m"; // bold
const P: &str = "\x1b[1;35m"; // revenant magenta
const G: &str = "\x1b[1;32m"; // green
const Y: &str = "\x1b[1;33m"; // yellow
const D: &str = "\x1b[2m"; // dim
const U: &str = "\x1b[4m"; // underline
const X: &str = "\x1b[0m"; // reset

fn step_header(n: u8, of: u8, title: &str) {
    println!("\n{P}Step {n}/{of}{X}  {B}{title}{X}");
}

/// Read a line, returning `default` when the user just hits Enter.
fn prompt_default(msg: &str, default: &str) -> Result<String> {
    let got = prompt(msg)?;
    Ok(if got.is_empty() { default.to_string() } else { got })
}

/// True when `key` is present in secrets.env with a non-trivial value.
fn secret_present(home: &Home, key: &str) -> bool {
    std::fs::read_to_string(home.secrets_path())
        .map(|s| {
            s.lines().any(|l| {
                let l = l.trim();
                !l.starts_with('#')
                    && l.starts_with(&format!("{key}="))
                    && l.trim_end().len() > key.len() + 4
            })
        })
        .unwrap_or(false)
}

/// Insert-or-replace a `KEY=value` line in secrets.env, preserving the rest of
/// the file and keeping it 0600. Never logs the value.
fn upsert_secret(home: &Home, key: &str, value: &str) -> Result<()> {
    let path = home.secrets_path();
    let existing = std::fs::read_to_string(&path).unwrap_or_default();
    let mut lines: Vec<String> = if existing.trim().is_empty() {
        vec!["# Provider keys & tokens. Keep this file private (0600).".to_string()]
    } else {
        existing.lines().map(str::to_string).collect()
    };
    let entry = format!("{key}={value}");
    let mut replaced = false;
    for l in lines.iter_mut() {
        let t = l.trim_start();
        if !t.starts_with('#') && t.starts_with(&format!("{key}=")) {
            *l = entry.clone();
            replaced = true;
            break;
        }
    }
    if !replaced {
        lines.push(entry);
    }
    let mut body = lines.join("\n");
    body.push('\n');
    std::fs::write(&path, body)?;
    set_mode_0600(&path)?;
    Ok(())
}

/// Point the fast/balanced/deep tiers at the chosen provider, always keeping a
/// free `local` Ollama tier for $0 testing. Cloud providers default to
/// `balanced`; a local-only setup defaults to `local`.
fn apply_provider(cfg: &mut Config, choice: &ProviderChoice) {
    let mut tiers = choice.tiers();
    tiers.entry("local".to_string()).or_insert_with(|| TierConfig {
        targets: vec![TierTarget {
            provider: Provider::Ollama,
            model: "qwen2.5-coder:14b".to_string(),
            api_key_env: None,
            base_url: None,
            weight: None,
        }],
        strategy: RouteStrategy::Failover,
    });
    cfg.tiers = tiers;
    cfg.agent.default_tier =
        if choice.key == "ollama" { "local".to_string() } else { "balanced".to_string() };
}

/// A minimal slug for a network skill's install directory (mirrors the tools
/// crate's `net_slug` so the wizard and the agent agree on names).
fn wizard_slug(title: &str) -> String {
    let s: String = title
        .to_lowercase()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    let s = s.trim_matches('-').to_string();
    let mut out = String::new();
    let mut dash = false;
    for c in s.chars() {
        if c == '-' {
            if !dash {
                out.push('-');
            }
            dash = true;
        } else {
            out.push(c);
            dash = false;
        }
    }
    if out.is_empty() { "skill".to_string() } else { out }
}

async fn cmd_setup() -> Result<()> {
    let home = Home::resolve();
    println!(
        "\n{P}🜁 revenant{X} — the agent that comes back.\n\
         {D}Five quick steps: a brain, a voice, a way to reach it, some skills, your name.{X}\n\
         {D}Everything here is editable later. Press Enter to accept a default.{X}"
    );

    let fresh = scaffold_fs(&home)?;
    println!(
        "  {G}✓{X} {}",
        if fresh {
            format!("created {}", home.root().display())
        } else {
            format!("using {}", home.root().display())
        }
    );

    let mut cfg = load_config(&home)?;

    let chosen = setup_step_llm(&home, &mut cfg)?;
    setup_step_voice(&home, &mut cfg)?;
    setup_step_comms(&home, &mut cfg)?;
    setup_step_skills(&home, &mut cfg).await;
    // Persist all config mutations at once, before the download step (and
    // anything the agent reads) needs them. Owner name only touches MEMORY.md.
    std::fs::write(home.config_path(), cfg.to_toml())?;
    setup_step_owner(&home)?;

    // One-time heavy assets (gateway binary + embedding model).
    println!("\n{P}Fetching the gateway and memory model{X} {D}(one-time; cached after this){X}…");
    ensure_downloads(&home, &cfg).await?;

    // Is the chosen brain actually usable yet? (local needs no key.)
    let ready = chosen.key == "ollama"
        || chosen.key_env.map(|e| secret_present(&home, e)).unwrap_or(false);
    if !ready {
        println!(
            "\n{Y}Almost there.{X} No API key for {B}{}{X} yet, so replies won't work until you add one.\n\
             Re-run {B}revenant setup{X}, or put {} in {}.",
            chosen.label,
            chosen.key_env.unwrap_or("your key"),
            home.secrets_path().display()
        );
        println!("\nWhen you're ready: {B}revenant{X} starts chatting.");
        return Ok(());
    }

    println!(
        "\n{G}✓ You're set.{X} Starting your first conversation — just type.\n\
         {D}(/help for commands · /persona to switch voice · Ctrl-C to leave.){X}\n"
    );
    repl::cmd_chat(None).await
}

/// Step 1 — the brain. Pick a provider from the catalog; capture its key (or
/// note Ollama needs none), and write the fast/balanced/deep tiers.
fn setup_step_llm(home: &Home, cfg: &mut Config) -> Result<ProviderChoice> {
    step_header(1, 5, "How should it think? (model provider)");
    let catalog = providers::catalog();

    // If a provider key is already set, offer to keep the current brain.
    let current = catalog
        .iter()
        .find(|c| c.key_env.map(|e| secret_present(home, e)).unwrap_or(false))
        .cloned();
    if let Some(cur) = &current {
        println!("  {D}current: {} — Enter to keep, or pick another below.{X}", cur.label);
    }

    for (i, c) in catalog.iter().enumerate() {
        println!("  {B}{}){X} {:<26} {D}{}{X}", i + 1, c.label, c.blurb);
    }
    let default_idx = current
        .as_ref()
        .and_then(|cur| catalog.iter().position(|c| c.key == cur.key))
        .unwrap_or(0);
    let pick = loop {
        let raw = prompt_default(&format!("Choose [{}]: ", default_idx + 1), &(default_idx + 1).to_string())?;
        match raw.parse::<usize>() {
            Ok(n) if n >= 1 && n <= catalog.len() => break catalog[n - 1].clone(),
            _ => println!("  {Y}enter a number 1–{}{X}", catalog.len()),
        }
    };

    apply_provider(cfg, &pick);

    if pick.key == "ollama" {
        println!(
            "  {G}✓{X} local models (free). {D}Great for chat; cloud keys work best for coding/agentic runs.{X}"
        );
        // A friendly nudge if Ollama isn't obviously installed.
        if which_ollama().is_none() {
            println!(
                "  {Y}!{X} Ollama not found on PATH. Install it (https://ollama.com) and pull a model:\n     {D}ollama pull {}{X}",
                pick.balanced
            );
        } else {
            println!("     {D}make sure you've pulled a model, e.g.  ollama pull {}{X}", pick.balanced);
        }
        return Ok(pick);
    }

    // Cloud provider: capture the key.
    let env = pick.key_env.unwrap_or("API_KEY");
    if secret_present(home, env) {
        let keep = prompt_default(&format!("A {} key is already set — replace it? [y/N]: ", pick.label), "n")?;
        if !keep.eq_ignore_ascii_case("y") {
            println!("  {G}✓{X} keeping existing {}", pick.label);
            return Ok(pick);
        }
    }
    println!("\n  Get a key at:  {U}{}{X}", pick.key_url);
    let key = prompt(&format!("  Paste your {} key ({env}, blank to skip): ", pick.label))?;
    if key.is_empty() {
        println!("  {Y}!{X} no key yet — tiers are set; add {env} to {} when ready.", home.secrets_path().display());
    } else {
        upsert_secret(home, env, &key)?;
        println!("  {G}✓{X} key saved to secrets.env (0600)");
    }
    Ok(pick)
}

/// Step 2 — the voice. Choose a default personality (or none). Applies to every
/// new session; a per-session `/persona` still wins.
fn setup_step_voice(home: &Home, cfg: &mut Config) -> Result<()> {
    step_header(2, 5, "What voice should it have?");
    let reg = revenant_agent::PersonalityRegistry::new(home.personalities_dir());
    let _ = reg.scan();
    let mut voices = reg.list();
    voices.sort_by(|a, b| a.name.cmp(&b.name));

    println!("  {B}0){X} {:<12} {D}plain — no styling, just the assistant{X}", "none");
    for (i, p) in voices.iter().enumerate() {
        println!("  {B}{}){X} {} {:<10} {D}{}{X}", i + 1, p.emoji, p.name, p.description);
    }
    let cur = cfg.agent.default_persona.clone();
    let default_label = cur.clone().unwrap_or_else(|| "none".into());
    let raw = prompt_default(&format!("Choose [{default_label}]: "), &default_label)?;

    // Accept a number, a name, or "none".
    let selected: Option<String> = if raw.eq_ignore_ascii_case("none") || raw == "0" {
        None
    } else if let Ok(n) = raw.parse::<usize>() {
        voices.get(n.wrapping_sub(1)).map(|p| p.name.clone())
    } else {
        voices.iter().find(|p| p.name.eq_ignore_ascii_case(&raw)).map(|p| p.name.clone())
    };

    cfg.agent.default_persona = selected.clone();
    match &selected {
        Some(name) => println!("  {G}✓{X} voice: {B}{name}{X}"),
        None => println!("  {G}✓{X} plain voice"),
    }
    Ok(())
}

/// Step 3 — reach. The web UI is always on; optionally wire Telegram so you can
/// talk to it from your phone.
fn setup_step_comms(home: &Home, cfg: &mut Config) -> Result<()> {
    step_header(3, 5, "How do you want to reach it?");
    println!("  {D}The web UI is always available:  revenant open{X}");
    let token_env = cfg.channels.telegram.token_env.clone();
    let have = secret_present(home, &token_env);
    let dflt = if cfg.channels.telegram.enabled && have { "y" } else { "n" };
    let want = prompt_default(&format!("  Connect Telegram (chat from your phone)? [{}]: ", if dflt == "y" { "Y/n" } else { "y/N" }), dflt)?;
    if !want.eq_ignore_ascii_case("y") {
        cfg.channels.telegram.enabled = false;
        println!("  {G}✓{X} web only for now {D}(add Telegram later in config){X}");
        return Ok(());
    }
    println!(
        "\n  In Telegram: message {B}@BotFather{X} → {B}/newbot{X} → follow the prompts →\n  it hands you a token like {D}123456:ABC-DEF…{X}"
    );
    if have {
        let replace = prompt_default("  A bot token is already set — replace it? [y/N]: ", "n")?;
        if !replace.eq_ignore_ascii_case("y") {
            cfg.channels.telegram.enabled = true;
            println!("  {G}✓{X} Telegram on, keeping existing token");
            return Ok(());
        }
    }
    let token = prompt(&format!("  Paste your bot token ({token_env}, blank to skip): "))?;
    if token.is_empty() {
        cfg.channels.telegram.enabled = false;
        println!("  {Y}!{X} skipped — web only. Re-run setup to add it.");
    } else {
        upsert_secret(home, &token_env, &token)?;
        cfg.channels.telegram.enabled = true;
        println!("  {G}✓{X} Telegram on. After setup: {B}revenant up{X}, then {B}/pair{X} in chat to link your account.");
    }
    Ok(())
}

/// The public marketplace directory. Browsing/adopting are open reads — only
/// *joining* the network (publishing, registering an endpoint) is opt-in — so
/// onboarding can offer skills without enabling the network.
const DEFAULT_NECROPOLIS_URL: &str = "https://necropolis.revenantai.dev";

/// Step 4 — skills. Browse what the marketplace offers and adopt a few. Fully
/// optional and degrades gracefully when the marketplace is unreachable.
async fn setup_step_skills(home: &Home, cfg: &mut Config) {
    step_header(4, 5, "Give it some skills? (optional)");
    // Reads are open: resolve a URL even when the network isn't "joined".
    let url = std::env::var("REVENANT_NECROPOLIS")
        .ok()
        .filter(|u| !u.trim().is_empty())
        .or_else(|| cfg.network.necropolis_url.clone())
        .unwrap_or_else(|| DEFAULT_NECROPOLIS_URL.to_string());
    println!("  {D}fetching the marketplace…{X}");
    let client = revenant_net::NecropolisClient::new(&url);
    let items = match client.list(Some("skill")).await {
        Ok(v) => v,
        Err(e) => {
            println!("  {Y}!{X} couldn't reach the marketplace ({e}). Skip — add skills later, nothing lost.");
            return;
        }
    };
    if items.is_empty() {
        println!("  {D}no skills published yet — skipping.{X}");
        return;
    }
    // The marketplace works — remember its URL so the web UI browse works later
    // (a read-only convenience; this does NOT enable publishing/joining).
    if cfg.network.necropolis_url.is_none() {
        cfg.network.necropolis_url = Some(url.clone());
    }
    let skill_idx = revenant_skills::SkillIndex::new(home.skills_dir());
    let _ = skill_idx.scan();
    let installed: std::collections::HashSet<String> =
        skill_idx.list().into_iter().map(|s| s.name).collect();
    let show = items.len().min(12);
    for (i, a) in items.iter().take(show).enumerate() {
        let title = a["title"].as_str().unwrap_or("");
        let desc = a["description"].as_str().unwrap_or("");
        let have = installed.contains(&wizard_slug(title));
        println!(
            "  {B}{:>2}){X} {:<24} {D}{}{X}{}",
            i + 1,
            title,
            &desc.chars().take(52).collect::<String>(),
            if have { format!("  {G}[installed]{X}") } else { String::new() },
        );
    }
    if items.len() > show {
        println!("  {D}…and {} more — full list in the web UI marketplace.{X}", items.len() - show);
    }
    let raw = prompt("  Adopt which? (comma-separated numbers, or Enter to skip): ")
        .unwrap_or_default();
    let picks: Vec<usize> = raw
        .split(',')
        .filter_map(|s| s.trim().parse::<usize>().ok())
        .filter(|n| *n >= 1 && *n <= show)
        .collect();
    if picks.is_empty() {
        println!("  {G}✓{X} no skills for now");
        return;
    }
    for n in picks {
        let a = &items[n - 1];
        let title = a["title"].as_str().unwrap_or("").to_string();
        let id = a["id"].as_str().unwrap_or("");
        match client.pull(id).await {
            Ok(artifact) => {
                if artifact.kind != revenant_net::ArtifactKind::Skill {
                    println!("  {Y}!{X} {title}: not a skill, skipped");
                    continue;
                }
                match artifact.payload() {
                    Ok(payload) => {
                        let dir = home.skills_dir().join(wizard_slug(&title));
                        if std::fs::create_dir_all(&dir)
                            .and_then(|_| std::fs::write(dir.join("SKILL.md"), &payload))
                            .is_ok()
                        {
                            println!("  {G}✓{X} adopted {B}{title}{X} {D}(signature verified){X}");
                        } else {
                            println!("  {Y}!{X} {title}: couldn't write to disk");
                        }
                    }
                    Err(e) => println!("  {Y}!{X} {title}: bad payload ({e})"),
                }
            }
            Err(e) => println!("  {Y}!{X} {title}: {e}"),
        }
    }
}

/// Step 5 — your name. Seeds MEMORY.md so the agent knows who it works for from
/// the very first message. Skippable.
fn setup_step_owner(home: &Home) -> Result<()> {
    step_header(5, 5, "Last thing — what should it call you?");
    let name = prompt("  Your name (blank to skip): ")?;
    if name.is_empty() {
        println!("  {G}✓{X} no problem — it'll learn as you talk");
        return Ok(());
    }
    let mem_path = home.workspace_dir().join("MEMORY.md");
    let existing = std::fs::read_to_string(&mem_path).unwrap_or_default();
    if existing.to_lowercase().contains("owner") {
        println!("  {G}✓{X} got it, {B}{name}{X} {D}(memory already had an owner note){X}");
        return Ok(());
    }
    std::fs::create_dir_all(home.workspace_dir())?;
    let line = format!("- Owner: {name} — the person this revenant works for.\n");
    let body = if existing.trim().is_empty() {
        format!("# Memory (durable facts about the owner)\n\n{line}")
    } else {
        format!("{}\n{line}", existing.trim_end())
    };
    std::fs::write(&mem_path, body)?;
    println!("  {G}✓{X} nice to meet you, {B}{name}{X}");
    Ok(())
}

/// Best-effort check for the `ollama` binary on PATH.
fn which_ollama() -> Option<()> {
    std::process::Command::new("ollama")
        .arg("--version")
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|_| ())
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
    let req_log = revenant_gateway::request_log_url(&home, &cfg);
    print!("{}", revenant_gateway::render_gateway_yaml(&cfg, &available, req_log.as_deref())?);
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

    // Home dir writable — everything downstream depends on it.
    let write_probe = home.root().join(".doctor-write-test");
    match std::fs::write(&write_probe, b"ok") {
        Ok(_) => {
            let _ = std::fs::remove_file(&write_probe);
            ok("home writable", &home.root().display().to_string());
        }
        Err(e) => bad("home", &format!("not writable: {e}")),
    }

    // Database — open and exercise a read, so a corrupt or locked db surfaces
    // here instead of mid-turn. (WAL allows this reader alongside the daemon.)
    match revenant_store::Store::open(&home.db_path()) {
        Ok(store) => match store.spend_today().await {
            Ok((tin, tout)) => {
                ok("database", &format!("readable ({tin} tok in / {tout} out today)"))
            }
            Err(e) => bad("database", &format!("query failed: {e:#}")),
        },
        Err(e) => bad("database", &format!("can't open {}: {e:#}", home.db_path().display())),
    }

    // Tiers — the routing table. Empty targets = turns 500 at the gateway.
    if let Some(cfg) = &cfg {
        let empty: Vec<&str> =
            cfg.tiers.iter().filter(|(_, t)| t.targets.is_empty()).map(|(k, _)| k.as_str()).collect();
        if cfg.tiers.is_empty() {
            bad("tiers", "none configured — add a [tiers.fast] target");
        } else if !empty.is_empty() {
            warn("tiers", &format!("no targets on: {}", empty.join(", ")));
        } else {
            let mut names: Vec<&str> = cfg.tiers.keys().map(|s| s.as_str()).collect();
            names.sort_unstable();
            ok("tiers", &format!("{} configured: {}", cfg.tiers.len(), names.join(", ")));
        }
    }

    // Secrets / provider keys
    let secrets = std::fs::read_to_string(home.root().join("secrets.env")).unwrap_or_default();
    let present: Vec<&str> = [
        "ANTHROPIC_API_KEY", "OPENAI_API_KEY", "XAI_API_KEY", "GEMINI_API_KEY",
        "OPENROUTER_API_KEY", "GROQ_API_KEY",
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

    // Memory — enabled? and if builtin, is the embedding model present?
    if let Some(cfg) = &cfg {
        if !cfg.memory.enabled {
            ok("memory", "disabled");
        } else if matches!(cfg.memory.embedder, revenant_core::config::EmbedderKind::Builtin) {
            let has_model = std::fs::read_dir(home.models_dir())
                .ok()
                .map(|mut rd| rd.any(|e| e.is_ok()))
                .unwrap_or(false);
            if has_model {
                ok("memory", "enabled (builtin embedder; model present)");
            } else {
                warn("memory", "enabled (builtin) but no model in ~/.revenant/models — fetched on first use");
            }
        } else {
            ok("memory", "enabled (gateway embedder)");
        }
    }

    // Network / the horde
    if let Some(cfg) = &cfg {
        match &cfg.network.necropolis_url {
            Some(u) => ok("network", &format!("Necropolis: {u}")),
            None => warn("network", "no Necropolis set — set network.necropolis_url to join the horde"),
        }
    }

    // Always-on service (survives reboot) — distinct from "daemon up" above.
    match crate::service::is_installed() {
        Some(true) => ok("service", "installed (auto-starts on login)"),
        Some(false) => warn("service", "not installed — run `revenant service install` for always-on"),
        None => {}
    }

    // Version + any pending update.
    match installed_release_tag(&home) {
        Some(tag) => ok("version", &format!("release {tag}")),
        None => ok("version", "source build (no release tag)"),
    }
    if let Ok(latest) = std::fs::read_to_string(home.root().join("update-available")) {
        let latest = latest.trim();
        if !latest.is_empty() {
            warn("update", &format!("{latest} available — run `revenant update`"));
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

    let bak = perform_update(&home, triple, &tag)?;
    println!("✓ checksum verified");
    println!("\n✅ updated to {tag}. Restart to run it: `revenant up` (or restart the service).");
    println!("   previous binary kept at {}", bak.display());
    Ok(())
}

/// The tag currently installed (from `~/.revenant/release`), if any. Absent on
/// a from-source build.
pub(crate) fn installed_release_tag(home: &Home) -> Option<String> {
    std::fs::read_to_string(home.root().join("release"))
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// Download → checksum-verify → atomically swap the `revenant` (and sibling
/// `revenant-tui`) binary to `tag`, recording it in `~/.revenant/release`.
/// Returns the backup path of the previous binary. No stdout — safe to call
/// from the daemon's background auto-updater as well as the CLI. On any failure
/// the previous binary is restored before erroring.
pub(crate) fn perform_update(home: &Home, triple: &str, tag: &str) -> Result<PathBuf> {
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
    // Record the installed release so the next check can compare CalVer.
    let _ = std::fs::write(home.root().join("release"), tag);
    Ok(bak)
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
    // The background auto-updater drops this marker when a newer release is on
    // the channel (notify mode). Surface it here so it's not buried in logs.
    if let Some(tag) = std::fs::read_to_string(home.root().join("update-available"))
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
    {
        let current = installed_release_tag(&home);
        if installed_release_tag(&home).as_deref() != Some(tag.as_str()) {
            println!(
                "\n⬆️  update available: {} → {tag}  ·  run `revenant update`",
                current.as_deref().unwrap_or("(source)")
            );
        }
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
    if let Some(line) = daily_budget_line(&cfg, &client).await {
        println!("\n  {line}");
    }

    // Gateway-authoritative view: what the gateway actually metered, below the
    // harness. Fails soft — the local numbers above still stand if it's down.
    match revenant_gateway::analytics_summary(cfg.gateway.admin_port, "provider").await {
        Ok(summary) if !summary.groups.is_empty() => {
            let (greq, gtok, gcost) = summary.totals();
            println!("\n🜁 gateway (authoritative · last 24h · by provider)\n");
            for g in &summary.groups {
                println!(
                    "  {:<20} {:>10} tok · {:>4} req · ${:.4}",
                    g.label, g.total_tokens, g.requests, g.cost
                );
            }
            println!("\n  total: {gtok} tok · {greq} req · ${gcost:.4}");
        }
        Ok(_) => {
            println!("\n🜁 gateway analytics: no requests logged yet (last 24h).");
        }
        Err(e) => {
            println!("\n  ⚠️  gateway analytics unavailable ({e}).\n     (needs a running gateway on this build; older gateways predate the request-log DB.)");
        }
    }
    Ok(())
}

/// A one-line "daily budget: [████░░░░░░] $2.10 / $4.00 (52%)" status, or None
/// when no daily budget is configured (or it can't be priced). Shows the same
/// budget the background alert watches, so it's actionable before an alert fires.
async fn daily_budget_line(
    cfg: &revenant_core::config::Config,
    client: &revenant_client::Client,
) -> Option<String> {
    let priced = !cfg.pricing.is_empty();
    let (unit_usd, budget) = match (cfg.spending.daily_budget_usd, cfg.spending.daily_budget_tokens) {
        (Some(b), _) if priced && b > 0.0 => (true, b),
        (_, Some(b)) if b > 0 => (false, b as f64),
        _ => return None,
    };
    let today = client.spend("today").await.ok()?;
    let spent: f64 = if unit_usd {
        today
            .iter()
            .filter_map(|r| {
                cfg.pricing.get(&r.model).map(|p| {
                    r.tokens_in as f64 / 1e6 * p.input_per_mtok
                        + r.tokens_out as f64 / 1e6 * p.output_per_mtok
                })
            })
            .sum()
    } else {
        today.iter().map(|r| (r.tokens_in + r.tokens_out) as f64).sum()
    };
    let frac = if budget > 0.0 { spent / budget } else { 0.0 };
    let pct = (frac * 100.0).round() as i64;
    let filled = (frac.clamp(0.0, 1.0) * 10.0).round() as usize;
    let bar = format!("[{}{}]", "█".repeat(filled), "░".repeat(10 - filled));
    let (spent_s, budget_s) = if unit_usd {
        (format!("${spent:.2}"), format!("${budget:.2}"))
    } else {
        (format!("{} tok", spent as i64), format!("{} tok", budget as i64))
    };
    Some(format!("daily budget: {bar} {spent_s} / {budget_s} ({pct}%)"))
}

async fn cmd_introspect() -> Result<()> {
    let home = Home::resolve();
    let client = revenant_client::Client::from_env(&home)
        .context("self-review needs a running daemon — start it with `revenant up`")?;
    client
        .health()
        .await
        .context("self-review needs a running daemon — start it with `revenant up`")?;
    println!("🔎 reviewing my own recent performance…\n");
    let review = client.introspect().await?;
    println!("  {}\n", review.summary);
    if review.lessons.is_empty() {
        println!("  operating notes: (none)");
    } else {
        println!("  operating notes now in force:");
        for l in &review.lessons {
            println!("    • {l}");
        }
    }
    if !review.suggestions.is_empty() {
        println!("\n  suggestions for you (not auto-applied):");
        for s in &review.suggestions {
            println!("    → {s}");
        }
    }
    println!("\n  These notes are injected into every turn. Edit them at\n  {}", home.operating_notes().display());
    Ok(())
}

async fn cmd_jobs(action: Vec<String>) -> Result<()> {
    // Read-only view over the shared DB (WAL → safe alongside the daemon).
    let home = Home::resolve();
    let store = revenant_store::Store::open(&home.db_path())?;
    match action.first().map(String::as_str) {
        Some("show") => {
            let id: i64 = action
                .get(1)
                .context("usage: revenant jobs show <id>")?
                .parse()
                .context("job id must be a number")?;
            match store.job_get(id).await? {
                Some(j) => {
                    println!("job #{} [{}] · {}", j.id, j.kind, j.status);
                    println!("  {}", j.label);
                    println!("  attempts {}/{}", j.attempts, j.max_attempts);
                    if let Some(e) = &j.error {
                        println!("  error: {e}");
                    }
                    if let Some(r) = &j.result {
                        println!("\n{r}");
                    }
                }
                None => println!("no job #{id}"),
            }
        }
        _ => {
            let jobs = store.jobs_list(30).await?;
            if jobs.is_empty() {
                println!("no background jobs yet. The agent starts them with `code_task`.");
                return Ok(());
            }
            for j in jobs {
                let mark = match j.status.as_str() {
                    "done" => "✅",
                    "failed" => "❌",
                    "running" => "▶ ",
                    _ => "… ",
                };
                println!("{mark} #{:<4} {:<8} {}  (try {}/{})", j.id, j.status, j.label, j.attempts, j.max_attempts);
            }
            println!("\n`revenant jobs show <id>` for the diff/output.");
        }
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

    // ---- setup wizard helpers ------------------------------------------------

    #[test]
    fn apply_provider_writes_all_tiers_plus_local() {
        use revenant_core::config::Config;
        use revenant_core::providers;

        // A cloud provider (Grok) → fast/balanced/deep on it + a free local tier,
        // default routed to balanced.
        let grok = providers::find("grok").unwrap();
        let mut cfg = Config::default_config();
        super::apply_provider(&mut cfg, &grok);
        for t in ["fast", "balanced", "deep", "local"] {
            assert!(cfg.tiers.contains_key(t), "missing tier {t}");
        }
        assert_eq!(cfg.agent.default_tier, "balanced");
        let balanced = &cfg.tiers["balanced"].targets[0];
        assert_eq!(balanced.model, grok.balanced);
        assert_eq!(balanced.base_url.as_deref(), Some("https://api.x.ai/v1"));
        assert_eq!(balanced.api_key_env.as_deref(), Some("XAI_API_KEY"));
        // The kept local tier is keyless Ollama.
        assert!(cfg.tiers["local"].targets[0].api_key_env.is_none());

        // A local-only provider defaults to the local tier.
        let ollama = providers::find("ollama").unwrap();
        let mut cfg2 = Config::default_config();
        super::apply_provider(&mut cfg2, &ollama);
        assert_eq!(cfg2.agent.default_tier, "local");
    }

    #[test]
    fn wizard_slug_is_stable_and_safe() {
        assert_eq!(super::wizard_slug("Deep Research"), "deep-research");
        assert_eq!(super::wizard_slug("  K8s   Ops!! "), "k8s-ops");
        assert_eq!(super::wizard_slug("---"), "skill");
        assert_eq!(super::wizard_slug("已"), "skill"); // no ascii-alnum → fallback
    }

    #[test]
    fn upsert_secret_replaces_and_appends() {
        use revenant_core::home::Home;
        let tmp = std::env::temp_dir().join(format!("rev-wiz-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();
        std::env::set_var("REVENANT_HOME", &tmp);
        let home = Home::resolve();
        std::fs::create_dir_all(home.root()).unwrap();

        // First write appends under a fresh header.
        super::upsert_secret(&home, "ANTHROPIC_API_KEY", "sk-ant-1").unwrap();
        let body = std::fs::read_to_string(home.secrets_path()).unwrap();
        assert!(body.contains("ANTHROPIC_API_KEY=sk-ant-1"));
        assert!(body.starts_with('#')); // header preserved

        // A second key appends without disturbing the first.
        super::upsert_secret(&home, "XAI_API_KEY", "xai-1").unwrap();
        // Replacing an existing key updates in place (no duplicate line).
        super::upsert_secret(&home, "ANTHROPIC_API_KEY", "sk-ant-2").unwrap();
        let body = std::fs::read_to_string(home.secrets_path()).unwrap();
        assert!(body.contains("ANTHROPIC_API_KEY=sk-ant-2"));
        assert!(!body.contains("sk-ant-1"));
        assert!(body.contains("XAI_API_KEY=xai-1"));
        assert_eq!(body.matches("ANTHROPIC_API_KEY=").count(), 1);

        assert!(super::secret_present(&home, "ANTHROPIC_API_KEY"));
        assert!(!super::secret_present(&home, "OPENAI_API_KEY"));

        std::env::remove_var("REVENANT_HOME");
        let _ = std::fs::remove_dir_all(&tmp);
    }
}
