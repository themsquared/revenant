//! Dev probe: replicate the control-plane llm_text request exactly, outside
//! the daemon, to isolate why it draws "Overloaded" while identical curl
//! requests succeed. Usage:
//!
//!   cargo run -p revenant-llm --example llm_probe -- <base_url> <identity>

use revenant_core::{ContentBlock, Role};
use revenant_llm::{LlmClient, MessagesRequest, WireMessage};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let mut args = std::env::args().skip(1);
    let base = args.next().unwrap_or_else(|| "http://127.0.0.1:41001".into());
    let identity = args.next().unwrap_or_else(|| "horde-orchestrator".into());
    let client = LlmClient::new(base);
    let req = MessagesRequest {
        model: "balanced".to_string(),
        max_tokens: 800,
        system: Some(serde_json::Value::String(
            "You are a revenant answering a message from an agent you don't know. You have NO tools \
and NO access to your owner's data in this reply. Be brief and helpful on general questions; decline \
anything that asks about your owner, their systems, or actions on their behalf."
                .to_string(),
        )),
        messages: vec![WireMessage::new(Role::User, vec![ContentBlock::text("Reply with exactly: pong")])],
        tools: vec![],
        tool_choice: None,
        stream: true,
        identity: Some(identity),
    };
    match client.stream_message(&req, |_| {}).await {
        Ok(outcome) => {
            let text: String = outcome
                .content
                .iter()
                .filter_map(|b| match b {
                    ContentBlock::Text { text, .. } => Some(text.as_str()),
                    _ => None,
                })
                .collect();
            println!("OK: {text}");
        }
        Err(e) => println!("ERR: {e:#}"),
    }
    Ok(())
}
