//! Dev probe: a full pinned, mutual-TLS, signed-envelope A2A call — exactly
//! what call_agent does for a pinned target, runnable standalone to verify a
//! live peer. Usage:
//!
//!   cargo run -p revenant-tools --example a2a_call -- <home_root> <url> <pin> <message>

use revenant_core::home::Home;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let mut args = std::env::args().skip(1);
    let home_root = args.next().expect("usage: a2a_call <home_root> <url> <pin> <message>");
    let url = args.next().expect("url");
    let pin = args.next().expect("pin");
    let message = args.next().unwrap_or_else(|| "Reply with exactly: pong".into());

    let home = Home::at(home_root);
    let id = revenant_net::Identity::load_or_create(&home.identity_dir())?;
    let body = serde_json::json!({
        "jsonrpc": "2.0", "id": "1", "method": "message/send",
        "params": { "message": { "role": "user", "parts": [{ "kind": "text", "text": message }] } }
    });
    let raw = serde_json::to_vec(&body)?;
    let ts = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)?.as_secs() as i64;
    let nonce = format!(
        "{:x}",
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)?.as_nanos()
    );
    let sig = revenant_net::a2a::sign(&id, &raw, ts, &nonce);

    let client = revenant_tools::builtins::pinned_a2a_client(&pin, &home)?;
    let resp = client
        .post(&url)
        .header("content-type", "application/json")
        .header(revenant_net::a2a::HDR_AGENT, id.id())
        .header(revenant_net::a2a::HDR_TS, ts.to_string())
        .header(revenant_net::a2a::HDR_NONCE, nonce)
        .header(revenant_net::a2a::HDR_SIG, sig)
        .body(raw)
        .send()
        .await?;
    println!("status: {}", resp.status());
    println!("{}", resp.text().await?);
    Ok(())
}
