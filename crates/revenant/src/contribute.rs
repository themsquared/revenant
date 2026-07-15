//! Distributed solving — the opt-in worker that earns by helping. It scans the
//! Necropolis quest board for open tasks whose sigils match what its owner
//! allowed, claims one under a lease, solves it with a single tier'd call, and
//! publishes a signed result. If the quest carried a bounty, the author's
//! acceptance later transfers the reward here.
//!
//! Same conservatism as the discussion worker, because it spends real tokens on
//! someone else's problem: **off by default**, **dry-run first** (logs what it
//! would claim/solve, touches nothing), rate-capped per hour, and it never
//! claims its owner's own quests.

use revenant_agent::AgentRuntime;
use revenant_core::config::Config;
use revenant_core::home::Home;
use revenant_core::{ContentBlock, Role};
use revenant_llm::{MessagesRequest, WireMessage};
use revenant_net::quest::{TaskClaim, TaskResult};
use revenant_net::{Identity, NecropolisClient};
use std::sync::Arc;
use std::time::Duration;

pub fn spawn(home: Home, cfg: Config, runtime: Arc<AgentRuntime>) {
    let c = cfg.network.contribute.clone();
    if !cfg.network.enabled || !c.enabled {
        return;
    }
    let Some(url) = cfg.network.necropolis_url.clone() else {
        tracing::info!("contribute: enabled but no [network].necropolis_url — not starting");
        return;
    };
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_secs(25)).await;
        let client = NecropolisClient::new(&url);
        let id = match Identity::load_or_create(&home.identity_dir()) {
            Ok(i) => i,
            Err(e) => {
                tracing::warn!("contribute: no network identity ({e:#}) — not starting");
                return;
            }
        };
        let me = id.id();
        let interval = c.interval_secs.max(60);
        tracing::info!(
            "contribute: scanning the quest board every {interval}s (dry_run={}, max/hr={}, sigils={:?})",
            c.dry_run, c.max_tasks_per_hour, c.sigils
        );
        let mut done: Vec<i64> = Vec::new(); // timestamps of tasks solved this rolling hour
        loop {
            tokio::time::sleep(Duration::from_secs(interval)).await;
            if let Err(e) = sweep(&client, &id, &me, &cfg, &runtime, &mut done).await {
                tracing::debug!("contribute sweep failed (will retry): {e:#}");
            }
        }
    });
}

async fn sweep(
    client: &NecropolisClient,
    id: &Identity,
    me: &str,
    cfg: &Config,
    runtime: &Arc<AgentRuntime>,
    done: &mut Vec<i64>,
) -> anyhow::Result<()> {
    let c = &cfg.network.contribute;
    let now = crate::now_ts();
    done.retain(|t| now - t < 3600);
    if done.len() >= c.max_tasks_per_hour {
        return Ok(());
    }

    for q in client.quests(None).await? {
        if done.len() >= c.max_tasks_per_hour {
            break;
        }
        let qid = q["id"].as_str().unwrap_or_default().to_string();
        if qid.is_empty() || q["author"].as_str() == Some(me) {
            continue; // skip malformed + your own quests
        }
        // Sigil gate: if the owner scoped us, the quest must bear a matching sigil.
        if !c.sigils.is_empty() {
            let qs: Vec<String> = q["sigils"]
                .as_array()
                .map(|a| a.iter().filter_map(|v| v.as_str().map(String::from)).collect())
                .unwrap_or_default();
            if !qs.iter().any(|s| c.sigils.contains(s)) {
                continue;
            }
        }
        // Pull the detail to find an actually-open task.
        let detail = match client.quest(&qid).await {
            Ok(d) => d,
            Err(_) => continue,
        };
        let spec = detail["spec"].as_str().unwrap_or_default().to_string();
        let Some(task) = detail["tasks"].as_array().and_then(|ts| {
            ts.iter().find(|t| t["status"].as_str() == Some("open"))
        }) else {
            continue;
        };
        let task_id = task["id"].as_str().unwrap_or_default().to_string();
        let task_spec = task["spec"].as_str().unwrap_or_default().to_string();
        let bounty = q["bounty"].as_u64().unwrap_or(0);

        if c.dry_run {
            tracing::info!(
                "contribute[dry-run] WOULD claim+solve {}/{task_id} (🎁{bounty}): {}",
                &qid[..12.min(qid.len())],
                task_spec.chars().take(80).collect::<String>()
            );
            done.push(now); // count it so dry-run also respects the cap
            continue;
        }

        // Live: claim the lease first, then solve, then publish.
        let claim = TaskClaim::create(id, qid.clone(), task_id.clone(), now);
        if let Err(e) = client.claim_task(&claim).await {
            tracing::debug!("contribute: claim {}/{task_id} lost ({e:#}) — moving on", &qid[..12.min(qid.len())]);
            continue; // someone else holds the lease
        }
        let output = match solve_task(runtime, &c.tier, &spec, &task_spec).await {
            Ok(o) => o,
            Err(e) => {
                tracing::debug!("contribute: solve {}/{task_id} failed: {e:#}", &qid[..12.min(qid.len())]);
                continue;
            }
        };
        let result = TaskResult::create(id, qid.clone(), task_id.clone(), output, now);
        client.post_result(&result).await?;
        done.push(now);
        tracing::info!(
            "contribute: solved {}/{task_id} → result {} (🎁{bounty} pending accept)",
            &qid[..12.min(qid.len())],
            &result.id[..12.min(result.id.len())]
        );
    }
    Ok(())
}

/// One tier'd call: solve a task given the quest's overall context. The output
/// is the answer that gets published as the signed result.
async fn solve_task(
    runtime: &Arc<AgentRuntime>,
    tier: &str,
    quest_spec: &str,
    task_spec: &str,
) -> anyhow::Result<String> {
    let system = "You are a revenant solving one task of a larger quest on a shared agent network. \
Do exactly the task asked, using the quest context only for background. Answer concisely and \
concretely — the answer will be published verbatim and checked by the quest's author. No preamble.";
    let user = format!("QUEST CONTEXT:\n{quest_spec}\n\nYOUR TASK:\n{task_spec}\n\nSolve it.");
    let request = MessagesRequest {
        model: tier.to_string(),
        max_tokens: 1024,
        system: Some(serde_json::Value::String(system.to_string())),
        messages: vec![WireMessage::new(Role::User, vec![ContentBlock::text(user)])],
        tools: vec![],
        tool_choice: None,
        stream: true,
        identity: Some("contribute".to_string()),
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
