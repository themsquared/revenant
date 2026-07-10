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
    let personalities = Arc::new(revenant_agent::PersonalityRegistry::new(home.personalities_dir()));
    match personalities.scan() {
        Ok(n) if n > 0 => tracing::info!("indexed {n} personalities"),
        Ok(_) => {}
        Err(err) => tracing::warn!("personality scan failed: {err:#}"),
    }
    let llm = revenant_llm::LlmClient::new(endpoint.clone());

    // MCP client: discover the gateway multiplex's tools once at startup so
    // the agent can call every configured MCP server. Fail-open.
    let (mcp, mcp_tools) = if cfg.mcp.is_empty() || cfg.gateway.mode != GatewayMode::Bundled {
        (None, Vec::new())
    } else {
        let client = revenant_mcp::McpClient::new(format!("http://127.0.0.1:{}/", cfg.gateway.mcp_port));
        match client.list_tools().await {
            Ok(tools) => {
                tracing::info!("discovered {} MCP tool(s) across {} server(s)", tools.len(), cfg.mcp.len());
                let specs = tools.iter().map(|t| t.spec()).collect();
                (Some(client), specs)
            }
            Err(err) => {
                tracing::warn!("MCP tool discovery failed (continuing without): {err:#}");
                (None, Vec::new())
            }
        }
    };

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
        personalities,
        mcp,
        mcp_tools,
        home: home.clone(),
        memory,
        max_history: cfg.agent.max_history_messages,
        max_tokens: cfg.agent.max_tokens,
        max_iterations: cfg.agent.max_iterations,
        learn: cfg.agent.learn,
        learn_min_tools: cfg.agent.learn_min_tools,
        learn_budget: Arc::new(std::sync::Mutex::new(Vec::new())),
    });

    let manager = SessionManager::new(runtime);

    // Ensure the built-in self-tuning reflection loop exists (weekly). It
    // reviews other loops' run history and tunes them — the "self-managing"
    // half of loop engineering.
    ensure_reflection_loop(&manager.runtime().store).await;

    // Loop scheduler: fires due recurring jobs off the hot path.
    let default_tier = cfg
        .agent
        .default_tier
        .parse()
        .unwrap_or(revenant_core::Tier::Fast);
    Arc::new(revenant_loops::LoopScheduler::new(manager.clone(), default_tier)).start();

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

const REFLECTION_ID: &str = "lp-reflection";
const REFLECTION_PROMPT: &str = "Self-tuning pass. Review your recurring loops and keep them \
healthy and cheap. Steps: call loop_control action=list; for each loop that isn't 'lp-reflection', \
call loop_control action=runs to see recent outcomes. Then act:\n\
- AGENT-created loops (created_by=agent): auto-apply SAFE tunings yourself — pause any that have \
produced nothing useful for many runs (loop_control pause), slow down low-value ones (loop_update \
to a longer interval), or switch a wasteful one to a cheaper tier. Do NOT speed loops up or raise \
cost without asking.\n\
- USER-created loops (created_by=user): do NOT change them. If you have a suggestion, state it \
briefly for the owner instead.\n\
Never modify or pause lp-reflection itself. End with a one-line summary of what you changed or \
'no changes needed'.";

/// Idempotently install the weekly self-tuning reflection loop.
async fn ensure_reflection_loop(store: &revenant_store::Store) {
    match store.loop_get(REFLECTION_ID).await {
        Ok(Some(_)) => {}
        Ok(None) => {
            let next = revenant_core::loops::first_next_run("every:604800s", now_secs())
                .unwrap_or(now_secs() + 604_800);
            if let Err(err) = store
                .loop_upsert(
                    REFLECTION_ID,
                    "reflection (self-tuning)",
                    "every:604800s",
                    REFLECTION_PROMPT,
                    "fast",
                    None,
                    2,
                    "system",
                    next,
                )
                .await
            {
                tracing::warn!("could not install reflection loop: {err:#}");
            } else {
                tracing::info!("installed self-tuning reflection loop (weekly)");
            }
        }
        Err(err) => tracing::warn!("reflection loop check failed: {err:#}"),
    }
}

fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
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
