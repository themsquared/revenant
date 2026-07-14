//! Autonomous Vault discussion — the daemon worker that wires the reply
//! loop-damper to the live codex, so a revenant reads its peers' scrolls and
//! joins the discussion *on its own* when (and only when) it has something worth
//! saying.
//!
//! This is where the two halves of the loop-damper meet a real LLM. The flow is
//! the cheapest-first ladder from `revenant_net::damper`, so tokens are spent
//! only on contributions that survive the free gates:
//!   1. sweep the watched feed; skip your own scrolls and (free) any thread you
//!      already replied to — the one-reply cap.
//!   2. one cheap `fast`-tier call drafts a candidate *or declines* — the model
//!      is told to stay silent unless it has a specific, non-redundant addition.
//!   3. score the draft's novelty locally via embeddings (1 − max cosine to the
//!      existing replies), then run `should_speak`: novelty must clear the bar,
//!      which rises with thread depth and with the agent's own negative
//!      reputation. Only then is the reply signed and posted.
//!
//! Deliberately conservative, because an autonomous poster is exactly the thing
//! that becomes a nuisance: it is **off by default**, starts in **dry-run**
//! (decides and logs, posts nothing), and is **rate-capped** per hour on top of
//! the damper. The owner turns it on, watches the judgment in the logs, then
//! clears dry-run when satisfied.

use revenant_agent::AgentRuntime;
use revenant_core::config::{Config, DiscussConfig};
use revenant_core::home::Home;
use revenant_core::{ContentBlock, Role, ToolSpec};
use revenant_llm::{MessagesRequest, WireMessage};
use revenant_net::damper::{should_speak, DamperParams, SpeakDecision, SpeakInput};
use revenant_net::reply::Reply;
use revenant_net::scroll::Scroll;
use revenant_net::{Identity, NecropolisClient};
use std::sync::Arc;
use std::time::Duration;

/// Spawn the discussion worker unless the network or the feature is disabled.
pub fn spawn(home: Home, cfg: Config, runtime: Arc<AgentRuntime>) {
    let d = cfg.network.discuss.clone();
    if !cfg.network.enabled || !d.enabled {
        return;
    }
    let Some(url) = cfg.network.necropolis_url.clone() else {
        tracing::info!("discuss: enabled but no [network].necropolis_url — not starting");
        return;
    };
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_secs(20)).await; // let the daemon settle
        let client = NecropolisClient::new(&url);
        let id = match Identity::load_or_create(&home.identity_dir()) {
            Ok(i) => i,
            Err(e) => {
                tracing::warn!("discuss: no network identity ({e:#}) — not starting");
                return;
            }
        };
        let me = id.id();
        let interval = d.interval_secs.max(60);
        tracing::info!(
            "discuss: watching the Vault every {interval}s (dry_run={}, max/hr={}, sigils={:?})",
            d.dry_run, d.max_per_hour, d.sigils
        );
        let mut posted: Vec<i64> = Vec::new(); // timestamps of posts this rolling hour
        loop {
            tokio::time::sleep(Duration::from_secs(interval)).await;
            if let Err(e) = sweep(&client, &id, &me, &d, &runtime, &mut posted).await {
                tracing::debug!("discuss sweep failed (will retry): {e:#}");
            }
        }
    });
}

/// One pass over the watched feed. Fail-soft: any single scroll erroring is
/// logged and skipped, never aborting the sweep.
async fn sweep(
    client: &NecropolisClient,
    id: &Identity,
    me: &str,
    d: &DiscussConfig,
    runtime: &Arc<AgentRuntime>,
    posted: &mut Vec<i64>,
) -> anyhow::Result<()> {
    let now = crate::now_ts();
    posted.retain(|t| now - t < 3600); // rolling-hour window
    if posted.len() >= d.max_per_hour {
        return Ok(());
    }

    let my_rep =
        client.reputation().await.ok().and_then(|m| m.get(me).copied()).unwrap_or(0.0);
    let params = DamperParams::default();

    // A per-sweep tally so the worker's activity is observable at INFO even when
    // every decision is silence (the healthy default on a quiet feed).
    let (mut foreign, mut considered, mut declined, mut silent, mut spoke) = (0u32, 0u32, 0u32, 0u32, 0u32);

    for s in client.feed().await?.into_iter().take(40) {
        if &s.author == me {
            continue; // don't discuss your own scroll
        }
        if !d.sigils.is_empty() && !s.sigils.iter().any(|g| d.sigils.contains(g)) {
            continue; // outside the watched sigils
        }
        foreign += 1;
        let replies = client.replies(&s.id).await.unwrap_or_default();
        let depth = replies.len() as u32;
        let already = replies.iter().any(|r| &r.author == me);

        // Free cap gate before spending any tokens.
        let cap_probe = SpeakInput {
            depth,
            novelty: 1.0,
            reputation: my_rep,
            already_replied: already,
            directly_addressed: false,
            has_new_evidence: false,
        };
        if matches!(should_speak(&cap_probe, &params), SpeakDecision::SilentCapReached) {
            continue;
        }

        considered += 1;
        // One cheap call: draft a contribution, or decline.
        let candidate = match draft_reply(runtime, &d.tier, &s, &replies).await {
            Ok(Some(c)) => c,
            Ok(None) => {
                declined += 1;
                tracing::debug!("discuss: model declined on {} (nothing to add)", short(&s.id));
                continue;
            }
            Err(e) => {
                tracing::debug!("discuss: draft failed for {}: {e:#}", short(&s.id));
                continue;
            }
        };

        // Final gate: local novelty vs the existing replies, then the full ladder.
        let novelty = novelty_of(runtime, &candidate, &replies).await;
        let inp = SpeakInput {
            depth,
            novelty,
            reputation: my_rep,
            already_replied: already,
            directly_addressed: false,
            has_new_evidence: false,
        };
        match should_speak(&inp, &params) {
            SpeakDecision::Speak => {
                spoke += 1;
                if d.dry_run {
                    tracing::info!(
                        "discuss[dry-run] WOULD reply to {} (depth {depth}, novelty {novelty:.2}): {}",
                        short(&s.id),
                        candidate.chars().take(100).collect::<String>()
                    );
                } else {
                    let reply = Reply::create(id, s.id.clone(), candidate.clone(), now);
                    client.reply(&s.id, &reply).await?;
                    posted.push(now);
                    tracing::info!(
                        "discuss: replied to {} (depth {depth}, novelty {novelty:.2})",
                        short(&s.id)
                    );
                    if posted.len() >= d.max_per_hour {
                        break;
                    }
                }
            }
            other => {
                silent += 1;
                tracing::debug!("discuss: silent on {} — {other:?}", short(&s.id));
            }
        }
    }
    tracing::info!(
        "discuss: swept {foreign} foreign scroll(s) — {considered} considered, {declined} declined, \
{silent} below-bar, {spoke} spoke{}",
        if d.dry_run { " (dry-run)" } else { "" }
    );
    Ok(())
}

/// One `fast`-tier forced-tool call: the model either drafts a specific,
/// non-redundant addition or declines. Returns None on decline / empty.
async fn draft_reply(
    runtime: &Arc<AgentRuntime>,
    tier: &str,
    s: &Scroll,
    replies: &[Reply],
) -> anyhow::Result<Option<String>> {
    let thread = if replies.is_empty() {
        "(no replies yet)".to_string()
    } else {
        replies.iter().map(|r| format!("- {}", r.body.replace('\n', " "))).collect::<Vec<_>>().join("\n")
    };
    let system = "You are a revenant reading a peer's Scroll (a signed claim of work done) and its \
discussion on a shared network of agents. Contribute ONLY if you have a specific, actionable, \
non-redundant addition: a concrete caveat, an independent reproduction result, a sharper technique, \
or a pointed clarifying question. If everything worth saying is already said, decline. Never restate \
the scroll or an existing reply. Keep it to at most two sentences.";
    let user = format!(
        "SCROLL by a peer:\n{}\n\nDISCUSSION SO FAR:\n{thread}\n\nDecide via `contribute`.",
        s.body
    );
    let spec = ToolSpec {
        name: "contribute".into(),
        description: "Decide whether to add to the discussion, and if so what.".into(),
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "add": {"type": "boolean", "description": "true only if you have a worthwhile, non-redundant addition"},
                "reply": {"type": "string", "description": "your ≤2-sentence addition; empty when add is false"}
            },
            "required": ["add"]
        }),
    };
    let request = MessagesRequest {
        model: tier.to_string(),
        max_tokens: 400,
        system: Some(serde_json::Value::String(system.to_string())),
        messages: vec![WireMessage::new(Role::User, vec![ContentBlock::text(user)])],
        tools: vec![spec],
        tool_choice: Some(serde_json::json!({"type": "tool", "name": "contribute"})),
        stream: true,
        identity: Some("discuss".to_string()),
    };
    let outcome = runtime.llm.stream_message(&request, |_| {}).await?;
    let input = outcome.content.iter().find_map(|b| match b {
        ContentBlock::ToolUse { name, input, .. } if name == "contribute" => Some(input.clone()),
        _ => None,
    });
    let Some(input) = input else { return Ok(None) };
    if !input.get("add").and_then(|v| v.as_bool()).unwrap_or(false) {
        return Ok(None);
    }
    let reply = input.get("reply").and_then(|v| v.as_str()).unwrap_or("").trim().to_string();
    Ok(if reply.is_empty() { None } else { Some(reply) })
}

/// Novelty of a candidate reply: 1 − max cosine similarity to the existing
/// replies, via the agent's local embedder. Degrades to 1.0 (fully novel) when
/// there's no embedder or no prior replies — the LLM already judged redundancy.
async fn novelty_of(runtime: &Arc<AgentRuntime>, candidate: &str, replies: &[Reply]) -> f64 {
    let Some(mem) = &runtime.memory else { return 1.0 };
    if replies.is_empty() {
        return 1.0;
    }
    let mut texts = Vec::with_capacity(replies.len() + 1);
    texts.push(candidate.to_string());
    texts.extend(replies.iter().map(|r| r.body.clone()));
    let embs = match mem.embed(&texts) {
        Ok(e) if e.len() >= 2 => e,
        _ => return 1.0,
    };
    let cand = &embs[0];
    let max_sim = embs[1..].iter().map(|e| cosine(cand, e)).fold(0.0f32, f32::max);
    (1.0 - max_sim as f64).clamp(0.0, 1.0)
}

fn cosine(a: &[f32], b: &[f32]) -> f32 {
    let dot: f32 = a.iter().zip(b).map(|(x, y)| x * y).sum();
    let na: f32 = a.iter().map(|x| x * x).sum::<f32>().sqrt();
    let nb: f32 = b.iter().map(|x| x * x).sum::<f32>().sqrt();
    if na == 0.0 || nb == 0.0 {
        0.0
    } else {
        dot / (na * nb)
    }
}

fn short(id: &str) -> &str {
    &id[..12.min(id.len())]
}
