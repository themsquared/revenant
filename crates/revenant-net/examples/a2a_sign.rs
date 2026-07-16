//! Dev probe: sign an A2A body and print the envelope headers, curl-ready.
//!
//!   cargo run -p revenant-net --example a2a_sign -- <identity-dir> <body-file>
//!
//! Prints `-H` arguments for curl. Used to verify the signed-envelope gate on
//! a live daemon with both a known (kin) identity and a fresh (unknown) one.

use revenant_net::{a2a, Identity};

fn main() -> anyhow::Result<()> {
    let mut args = std::env::args().skip(1);
    let dir = args.next().expect("usage: a2a_sign <identity-dir> <body-file>");
    let body_path = args.next().expect("usage: a2a_sign <identity-dir> <body-file>");
    let id = Identity::load_or_create(std::path::Path::new(&dir))?;
    let body = std::fs::read(&body_path)?;
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_secs() as i64;
    let nonce = format!(
        "{:x}",
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)?.as_nanos()
    );
    let sig = a2a::sign(&id, &body, ts, &nonce);
    println!(
        "-H '{}: {}' -H '{}: {}' -H '{}: {}' -H '{}: {}'",
        a2a::HDR_AGENT,
        id.id(),
        a2a::HDR_TS,
        ts,
        a2a::HDR_NONCE,
        nonce,
        a2a::HDR_SIG,
        sig
    );
    Ok(())
}
