//! Chat REPL over two interchangeable backends: the daemon API, or an
//! embedded runtime when no daemon is running. Both feed the same
//! event-driven loop, including interactive approval prompts.

use anyhow::Result;
use futures::StreamExt;
use revenant_core::home::Home;
use revenant_core::{Event, Tier};
use std::io::Write;
use tokio::sync::mpsc;

enum Backend {
    Api {
        client: revenant_client::Client,
        session_id: i64,
    },
    Embedded {
        manager: revenant_agent::SessionManager,
        session_id: i64,
    },
}

impl Backend {
    async fn send(&self, text: &str, tier: Tier) -> Result<()> {
        match self {
            Backend::Api { client, session_id } => {
                client.send_message(*session_id, text, Some(tier.as_str())).await
            }
            Backend::Embedded { manager, session_id } => {
                manager
                    .submit(
                        *session_id,
                        revenant_agent::SessionMsg::UserInput { content: text.to_string(), tier },
                    )
                    .await
            }
        }
    }

    async fn decide(&self, approval_id: &str, approve: bool) -> Result<()> {
        match self {
            Backend::Api { client, .. } => {
                client.decide(approval_id, approve, "cli").await.map(|_| ())
            }
            Backend::Embedded { manager, .. } => manager
                .runtime()
                .approvals
                .resolve(approval_id, approve, "cli")
                .await
                .map(|_| ()),
        }
    }

    fn session_id(&self) -> i64 {
        match self {
            Backend::Api { session_id, .. } | Backend::Embedded { session_id, .. } => *session_id,
        }
    }
}

pub async fn cmd_chat(tier_arg: Option<String>) -> Result<()> {
    let home = Home::resolve();
    let cfg = crate::load_config(&home)?;
    let mut tier: Tier = tier_arg
        .as_deref()
        .unwrap_or(&cfg.agent.default_tier)
        .parse()
        .map_err(|e: String| anyhow::anyhow!(e))?;

    // Events from either backend arrive on one channel.
    let (event_tx, mut event_rx) = mpsc::channel::<Event>(256);

    let (backend, _gateway_handle) = match revenant_client::Client::from_env(&home) {
        Ok(client) if client.health().await.is_ok() => {
            let session_id = client.create_session("cli").await?;
            let stream_client = client.clone();
            let tx = event_tx.clone();
            tokio::spawn(async move {
                if let Ok(mut stream) = stream_client.events().await {
                    while let Some(Ok(event)) = stream.next().await {
                        if tx.send(event).await.is_err() {
                            break;
                        }
                    }
                }
            });
            println!("connected to daemon");
            (Backend::Api { client, session_id }, None)
        }
        _ => {
            println!("daemon not reachable — running embedded session");
            let daemon = crate::daemon::build(&home, &cfg).await?;
            let session_id = daemon
                .manager
                .runtime()
                .store
                .ensure_session("cli", "local", "chat")
                .await?;
            let mut rx = daemon.manager.runtime().events.subscribe();
            let tx = event_tx.clone();
            tokio::spawn(async move {
                while let Ok(event) = rx.recv().await {
                    if tx.send(event).await.is_err() {
                        break;
                    }
                }
            });
            (
                Backend::Embedded { manager: daemon.manager.clone(), session_id },
                daemon.gateway_handle,
            )
        }
    };

    // stdin on a blocking thread feeding a channel, so the loop can react
    // to events and signals while idle.
    let (line_tx, mut line_rx) = mpsc::channel::<String>(4);
    std::thread::spawn(move || {
        let stdin = std::io::stdin();
        loop {
            let mut line = String::new();
            match stdin.read_line(&mut line) {
                Ok(0) | Err(_) => break,
                Ok(_) => {
                    if line_tx.blocking_send(line).is_err() {
                        break;
                    }
                }
            }
        }
    });

    let my_session = backend.session_id();
    println!("revenant chat — tier: {tier} — /quit to exit, /tier <t> to switch");

    'repl: loop {
        print!("\x1b[1myou>\x1b[0m ");
        std::io::stdout().flush()?;
        let line = tokio::select! {
            line = line_rx.recv() => match line { Some(l) => l, None => break },
            _ = crate::shutdown_signal() => { println!("\nshutting down"); break; }
        };
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
                    tier = t;
                    println!("tier → {tier}");
                }
                Err(e) => println!("{e}"),
            }
            continue;
        }

        backend.send(line, tier).await?;
        print!("\x1b[1mrev>\x1b[0m ");
        std::io::stdout().flush()?;

        // Consume events until this session's turn ends.
        loop {
            let event = tokio::select! {
                event = event_rx.recv() => match event { Some(e) => e, None => break 'repl },
                _ = crate::shutdown_signal() => { println!("\nshutting down"); break 'repl; }
            };
            if event.session_id().is_some_and(|id| id != my_session) {
                continue;
            }
            match event {
                Event::TurnDelta { text, .. } => {
                    print!("{text}");
                    std::io::stdout().flush()?;
                }
                Event::ToolStarted { summary, .. } => {
                    println!("\n\x1b[2m[tool] {summary}\x1b[0m");
                }
                Event::ApprovalCreated { id, summary, .. } => {
                    print!("\x1b[33m⚠ approve? {summary} [y/N]:\x1b[0m ");
                    std::io::stdout().flush()?;
                    let answer = tokio::select! {
                        line = line_rx.recv() => line.unwrap_or_default(),
                        _ = tokio::time::sleep(std::time::Duration::from_secs(890)) => String::new(),
                    };
                    let approve = matches!(answer.trim(), "y" | "Y" | "yes");
                    backend.decide(&id, approve).await?;
                    if approve {
                        print!("\x1b[2mapproved — continuing\x1b[0m\n\x1b[1mrev>\x1b[0m ");
                    } else {
                        print!("\x1b[2mdenied\x1b[0m\n\x1b[1mrev>\x1b[0m ");
                    }
                    std::io::stdout().flush()?;
                }
                Event::TurnCompleted { input_tokens, output_tokens, routed_model, .. } => {
                    println!(
                        "\n\x1b[2m[{} · {}in/{}out tok]\x1b[0m",
                        routed_model.as_deref().unwrap_or(tier.as_str()),
                        input_tokens,
                        output_tokens
                    );
                    break;
                }
                Event::TurnFailed { error, .. } => {
                    println!("\n\x1b[31merror: {error}\x1b[0m");
                    break;
                }
                _ => {}
            }
        }
    }

    if let Some(handle) = _gateway_handle {
        handle.shutdown().await;
    }
    Ok(())
}
