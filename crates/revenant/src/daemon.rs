//! Daemon assembly: gateway + store + runtime + control plane.

use anyhow::{Context, Result};
use revenant_agent::{AgentRuntime, SessionManager};
use revenant_core::config::{Config, GatewayMode};
use revenant_core::home::Home;
use revenant_core::EventBus;
use revenant_security::ApprovalBroker;
use revenant_skills::SkillIndex;
use revenant_store::Store;
use revenant_tools::ToolRegistry;
use std::sync::Arc;
use std::time::Duration;

pub struct Daemon {
    pub manager: SessionManager,
    pub gateway_handle: Option<revenant_gateway::SupervisorHandle>,
    pub llm_endpoint: String,
}

/// Start the supervised gateway; returns (llm_base_url, Option<handle>).
pub async fn start_gateway(
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

/// Assemble the full runtime (used by the daemon AND embedded chat — one
/// code path, two frontings).
pub async fn build(home: &Home, cfg: &Config) -> Result<Daemon> {
    let (endpoint, gateway_handle) = start_gateway(home, cfg).await?;

    let store = Store::open(&home.db_path())?;
    // Generous buffer: the telegram mirror blocks on HTTP edits while
    // high-frequency turn deltas queue behind it.
    let events = EventBus::new(8192);
    let approvals =
        ApprovalBroker::new(store.clone(), events.clone(), Duration::from_secs(900));
    let skills = Arc::new(SkillIndex::new(home.skills_dir()));
    match skills.scan() {
        Ok(n) if n > 0 => tracing::info!("indexed {n} skills"),
        Ok(_) => {}
        Err(err) => tracing::warn!("skill scan failed: {err:#}"),
    }
    let tools = ToolRegistry::builtin(home, skills.clone());
    let agents = Arc::new(revenant_agent::AgentRegistry::new(home.agents_dir()));
    match agents.scan() {
        Ok(n) if n > 0 => tracing::info!("indexed {n} subagent definitions"),
        Ok(_) => {}
        Err(err) => tracing::warn!("subagent scan failed: {err:#}"),
    }
    let llm = revenant_llm::LlmClient::new(endpoint.clone());

    // Memory engine: fail-open. A missing model or broken vault must not
    // keep the agent from coming up.
    let memory = if cfg.memory.enabled {
        match revenant_memory::MemoryEngine::new(
            store.clone(),
            llm.clone(),
            home,
            cfg.memory.clone(),
        )
        .await
        {
            Ok(engine) => {
                engine.start_background();
                Some(engine)
            }
            Err(err) => {
                tracing::warn!("memory engine disabled: {err:#} (run `revenant init`?)");
                None
            }
        }
    } else {
        None
    };

    let runtime = Arc::new(AgentRuntime {
        store,
        llm,
        tools,
        approvals,
        events,
        skills,
        agents,
        home: home.clone(),
        memory,
        max_history: cfg.agent.max_history_messages,
        max_tokens: cfg.agent.max_tokens,
        max_iterations: cfg.agent.max_iterations,
    });

    let manager = SessionManager::new(runtime);

    // Telegram channel: starts only when enabled AND the bot token exists.
    if cfg.channels.telegram.enabled {
        let token = revenant_gateway::load_secrets(home)?
            .into_iter()
            .find(|(k, _)| k == &cfg.channels.telegram.token_env)
            .map(|(_, v)| v);
        match token {
            Some(token) if !token.is_empty() => {
                let default_tier = cfg
                    .agent
                    .default_tier
                    .parse()
                    .unwrap_or(revenant_core::Tier::Balanced);
                let channel = revenant_channels::telegram::TelegramChannel {
                    client: revenant_channels::telegram::TelegramClient::new(&token),
                    manager: manager.clone(),
                    default_tier,
                };
                tokio::spawn(async move {
                    if let Err(err) = channel.run().await {
                        tracing::error!("telegram channel exited: {err:#}");
                    }
                });
            }
            _ => tracing::info!(
                "telegram disabled: {} not set in secrets.env",
                cfg.channels.telegram.token_env
            ),
        }
    }

    Ok(Daemon {
        manager,
        gateway_handle,
        llm_endpoint: endpoint,
    })
}

pub async fn cmd_up() -> Result<()> {
    let home = Home::resolve();
    let cfg = crate::load_config(&home)?;
    let daemon = build(&home, &cfg).await?;

    let token = std::fs::read_to_string(home.root().join("token"))
        .context("reading ~/.revenant/token — run `revenant init`")?
        .trim()
        .to_string();
    let default_tier = cfg
        .agent
        .default_tier
        .parse()
        .map_err(|e: String| anyhow::anyhow!(e))?;
    let state = revenant_control::AppState::new(
        daemon.manager.clone(),
        token,
        default_tier,
        revenant_llm::LlmClient::new(daemon.llm_endpoint.clone()),
        home.clone(),
    );

    let listener = tokio::net::TcpListener::bind(crate::DEFAULT_BIND)
        .await
        .with_context(|| format!("binding control plane on {}", crate::DEFAULT_BIND))?;
    println!(
        "revenant up — gateway at {} · control API at http://{} (ctrl-c to stop)",
        daemon.llm_endpoint,
        crate::DEFAULT_BIND
    );

    let server = axum::serve(listener, revenant_control::router(state));
    tokio::select! {
        result = server => { result?; }
        _ = crate::shutdown_signal() => {}
    }
    if let Some(handle) = daemon.gateway_handle {
        handle.shutdown().await;
    }
    Ok(())
}
