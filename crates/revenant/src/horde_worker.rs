//! The horde worker — this agent's half of distributed thinking. It polls the
//! account's PRIVATE board (not the public quest board), claims an open subtask
//! under a lease, solves it with one tier'd call, and submits a signed result.
//!
//! Unlike `contribute` (which works strangers' public quests for bounties),
//! this is your own account's work: no sigil gate, no economy, and a snappier
//! cadence so a fan-out feels responsive. Same conservatism otherwise — off by
//! default, dry-run first, rate-capped — because it spends real tokens.

use revenant_agent::AgentRuntime;
use revenant_core::config::Config;
use revenant_core::home::Home;
use revenant_core::{ContentBlock, Role};
use revenant_llm::{MessagesRequest, WireMessage};
use revenant_net::horde::{HordeClaim, HordeResult};
use revenant_net::{Identity, NecropolisClient};
use std::sync::Arc;
use std::time::Duration;

pub fn spawn(home: Home, cfg: Config, runtime: Arc<AgentRuntime>) {
    let h = cfg.network.horde.clone();
    if !cfg.network.enabled || !h.enabled {
        return;
    }
    let Some(url) = cfg.network.necropolis_url.clone() else {
        tracing::info!("horde worker: enabled but no [network].necropolis_url — not starting");
        return;
    };
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_secs(20)).await;
        let client = NecropolisClient::new(&url);
        let id = match Identity::load_or_create(&home.identity_dir()) {
            Ok(i) => i,
            Err(e) => {
                tracing::warn!("horde worker: no network identity ({e:#}) — not starting");
                return;
            }
        };
        let interval = h.interval_secs.max(5);
        tracing::info!(
            "horde worker: polling the account board every {interval}s (dry_run={}, max/hr={})",
            h.dry_run, h.max_tasks_per_hour
        );
        let mut done: Vec<i64> = Vec::new(); // solve timestamps this rolling hour
        loop {
            tokio::time::sleep(Duration::from_secs(interval)).await;
            if let Err(e) = sweep(&client, &id, &cfg, &runtime, &mut done).await {
                tracing::debug!("horde sweep failed (will retry): {e:#}");
            }
        }
    });
}

async fn sweep(
    client: &NecropolisClient,
    id: &Identity,
    cfg: &Config,
    runtime: &Arc<AgentRuntime>,
    done: &mut Vec<i64>,
) -> anyhow::Result<()> {
    let h = &cfg.network.horde;
    let now = crate::now_ts();
    done.retain(|t| now - t < 3600);
    if done.len() >= h.max_tasks_per_hour {
        return Ok(());
    }

    // Open subtasks on my account's board, oldest first (fair FIFO within a run).
    let mut open = client.horde_open(id, None).await?;
    open.reverse(); // horde_open is newest-first; take the oldest waiting first
    for t in open {
        if done.len() >= h.max_tasks_per_hour {
            break;
        }
        let tid = t["id"].as_str().unwrap_or_default().to_string();
        let title = t["title"].as_str().unwrap_or_default().to_string();
        let spec = t["spec"].as_str().unwrap_or_default().to_string();
        if tid.is_empty() {
            continue;
        }

        if h.dry_run {
            tracing::info!(
                "horde[dry-run] WOULD claim+solve {} ({}): {}",
                &tid[..12.min(tid.len())],
                title,
                spec.chars().take(80).collect::<String>()
            );
            done.push(now);
            continue;
        }

        // Claim the lease first; if another of my agents holds it, move on.
        let claim = HordeClaim::create(id, tid.clone(), now);
        if let Err(e) = client.claim_horde(&claim).await {
            tracing::debug!("horde: claim {} lost ({e:#}) — moving on", &tid[..12.min(tid.len())]);
            continue;
        }
        let output = match solve(runtime, &h.tier, &title, &spec).await {
            Ok(o) => o,
            Err(e) => {
                tracing::debug!("horde: solve {} failed: {e:#}", &tid[..12.min(tid.len())]);
                continue;
            }
        };
        let result = HordeResult::create(id, tid.clone(), output, now);
        client.submit_horde(&result).await?;
        done.push(now);
        tracing::info!("horde: solved {} ({title})", &tid[..12.min(tid.len())]);
    }
    Ok(())
}

/// One tier'd call to solve a subtask. The output is submitted verbatim and
/// later synthesized by the orchestrator, so answer concretely, no preamble.
async fn solve(
    runtime: &Arc<AgentRuntime>,
    tier: &str,
    title: &str,
    spec: &str,
) -> anyhow::Result<String> {
    let system = "You are one revenant in a personal horde, solving a single subtask of a larger goal \
your owner is working. Do exactly the subtask; answer concisely and concretely — your answer is \
combined with your siblings' and synthesized into a final result. No preamble.";
    let user = format!("SUBTASK: {title}\n\n{spec}\n\nSolve it.");
    let request = MessagesRequest {
        model: tier.to_string(),
        max_tokens: 1500,
        system: Some(serde_json::Value::String(system.to_string())),
        messages: vec![WireMessage::new(Role::User, vec![ContentBlock::text(user)])],
        tools: vec![],
        tool_choice: None,
        stream: true,
        identity: Some("horde".to_string()),
    };
    let outcome = runtime.llm.stream_message(&request, |_| {}).await?;
    let text: String = outcome
        .content
        .iter()
        .filter_map(|b| match b {
            ContentBlock::Text { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("")
        .trim()
        .to_string();
    if text.is_empty() {
        anyhow::bail!("solver produced no output");
    }
    Ok(text)
}
