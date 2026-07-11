//! Standalone Necropolis server for deployment (Fly.io / VPS / the mini). Lean
//! — depends only on revenant-net, not the whole harness — so the container
//! image stays small. Binds 0.0.0.0 for containers. Configured by env:
//!   PORT           listen port (default 8080)
//!   NECROPOLIS_DB  ledger path (default /data/necropolis.db — a Fly volume)

use std::sync::{Arc, Mutex};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt().with_target(false).init();
    let port: u16 = std::env::var("PORT").ok().and_then(|p| p.parse().ok()).unwrap_or(8080);
    let db = std::env::var("NECROPOLIS_DB").unwrap_or_else(|_| "/data/necropolis.db".to_string());

    let dir = revenant_net::necropolis::Directory::open(&db)?;
    tracing::info!("necropolis ledger '{db}' verified: {} entries", dir.ledger_len()?);
    let addr = std::net::SocketAddr::from(([0, 0, 0, 0], port));
    revenant_net::necropolis::serve(addr, Arc::new(Mutex::new(dir))).await
}
