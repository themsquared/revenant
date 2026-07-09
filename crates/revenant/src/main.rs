//! revenant CLI: init / up / chat / render.

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use revenant_core::config::{Config, GatewayMode};
use revenant_core::home::Home;
use revenant_core::Tier;
use std::io::Write;

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
    /// Start the daemon (supervised gateway) in the foreground.
    Up,
    /// Interactive chat REPL (starts the gateway if needed).
    Chat {
        /// Model tier: fast | balanced | deep | local
        #[arg(long)]
        tier: Option<String>,
    },
    /// Print the rendered agentgateway config (debug).
    Render,
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
            Command::Up => cmd_up().await,
            Command::Chat { tier } => cmd_chat(tier).await,
            Command::Render => cmd_render(),
        }
    })
}

fn load_config(home: &Home) -> Result<Config> {
    let path = home.config_path();
    if !path.exists() {
        bail!("no config at {} — run `revenant init` first", path.display());
    }
    let raw = std::fs::read_to_string(&path)?;
    Config::from_toml(&raw).with_context(|| format!("parsing {}", path.display()))
}

async fn cmd_init() -> Result<()> {
    let home = Home::resolve();
    std::fs::create_dir_all(home.root())?;
    std::fs::create_dir_all(home.gateway_bin_dir())?;
    std::fs::create_dir_all(home.workspace_dir())?;
    std::fs::create_dir_all(home.skills_dir())?;
    std::fs::create_dir_all(home.logs_dir())?;

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
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&secrets_path, std::fs::Permissions::from_mode(0o600))?;
        }
        println!("wrote {} (0600)", secrets_path.display());
    }

    // Fetch the pinned gateway now so first `chat` is instant.
    let cfg = load_config(&home)?;
    if cfg.gateway.mode == GatewayMode::Bundled {
        let bin = revenant_gateway::ensure_binary(&home, &cfg).await?;
        println!("gateway binary ready: {}", bin.display());
    }
    println!("\nrevenant is initialized. Try: revenant chat");
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

/// Start the supervised gateway; returns (llm_base_url, Option<handle>).
async fn start_gateway(
    home: &Home,
    cfg: &Config,
) -> Result<(String, Option<revenant_gateway::SupervisorHandle>)> {
    match cfg.gateway.mode {
        GatewayMode::External => {
            let endpoint = cfg
                .gateway
                .endpoint
                .clone()
                .context("gateway.mode = external requires gateway.endpoint")?;
            Ok((endpoint, None))
        }
        GatewayMode::Bundled => {
            let binary = revenant_gateway::ensure_binary(home, cfg).await?;
            let env = revenant_gateway::load_secrets(home)?;
            revenant_gateway::write_gateway_config(home, cfg, &binary, &env).await?;
            let supervisor = revenant_gateway::GatewaySupervisor {
                binary,
                config_path: home.gateway_config_path(),
                env,
                llm_port: cfg.gateway.llm_port,
            };
            let handle = supervisor.start().await?;
            Ok((format!("http://127.0.0.1:{}", cfg.gateway.llm_port), Some(handle)))
        }
    }
}

async fn cmd_up() -> Result<()> {
    let home = Home::resolve();
    let cfg = load_config(&home)?;
    let (endpoint, handle) = start_gateway(&home, &cfg).await?;
    println!("revenant up — gateway at {endpoint} (ctrl-c to stop)");
    tokio::signal::ctrl_c().await?;
    if let Some(handle) = handle {
        handle.shutdown().await;
    }
    Ok(())
}

async fn cmd_chat(tier_arg: Option<String>) -> Result<()> {
    let home = Home::resolve();
    let cfg = load_config(&home)?;
    let tier: Tier = tier_arg
        .as_deref()
        .unwrap_or(&cfg.agent.default_tier)
        .parse()
        .map_err(|e: String| anyhow::anyhow!(e))?;

    let (endpoint, handle) = start_gateway(&home, &cfg).await?;
    let store = revenant_store::Store::open(&home.db_path())?;
    let llm = revenant_llm::LlmClient::new(endpoint);
    let agent = revenant_agent::Agent::new(
        store.clone(),
        llm,
        cfg.agent.max_history_messages,
        cfg.agent.max_tokens,
    );
    let session_id = store.ensure_session("cli", "local", "chat").await?;

    println!("revenant chat — tier: {tier} — /quit to exit, /tier <t> to switch");
    let stdin = std::io::stdin();
    let mut current_tier = tier;
    loop {
        print!("\x1b[1myou>\x1b[0m ");
        std::io::stdout().flush()?;
        let mut line = String::new();
        if stdin.read_line(&mut line)? == 0 {
            break; // EOF
        }
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if line == "/quit" || line == "/exit" {
            break;
        }
        if let Some(t) = line.strip_prefix("/tier ") {
            match t.trim().parse::<Tier>() {
                Ok(t) => {
                    current_tier = t;
                    println!("tier → {current_tier}");
                }
                Err(e) => println!("{e}"),
            }
            continue;
        }

        print!("\x1b[1mrev>\x1b[0m ");
        std::io::stdout().flush()?;
        let result = agent
            .run_turn(session_id, current_tier, line, |delta| {
                print!("{delta}");
                let _ = std::io::stdout().flush();
            })
            .await;
        println!();
        match result {
            Ok(stats) => {
                let (day_in, day_out) = store.spend_today().await.unwrap_or((0, 0));
                println!(
                    "\x1b[2m[{} · {}in/{}out tok · today {}in/{}out]\x1b[0m",
                    stats.routed_model.as_deref().unwrap_or(current_tier.as_str()),
                    stats.usage.input_tokens,
                    stats.usage.output_tokens,
                    day_in,
                    day_out
                );
            }
            Err(err) => println!("\x1b[31merror: {err:#}\x1b[0m"),
        }
    }

    if let Some(handle) = handle {
        handle.shutdown().await;
    }
    Ok(())
}
