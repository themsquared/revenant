//! Vault watcher: human edits (e.g. in Obsidian) are authoritative — any
//! external change triggers a reindex. Our own atomic writes are recognized
//! by content hash and ignored.

use crate::MemoryEngine;
use notify_debouncer_full::{
    new_debouncer,
    notify::{RecursiveMode, Watcher},
    DebounceEventResult,
};
use sha2::{Digest, Sha256};
use std::sync::Arc;
use std::time::Duration;

pub fn start(engine: Arc<MemoryEngine>) {
    let root = engine.vault.root().to_path_buf();
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Vec<std::path::PathBuf>>();

    // The debouncer runs its own thread; keep it alive by leaking the handle
    // into the engine's lifetime via the spawned task below.
    let debouncer = new_debouncer(
        Duration::from_secs(1),
        None,
        move |result: DebounceEventResult| {
            if let Ok(events) = result {
                let paths: Vec<_> = events
                    .into_iter()
                    .flat_map(|e| e.event.paths)
                    .filter(|p| p.extension().and_then(|x| x.to_str()) == Some("md"))
                    .collect();
                if !paths.is_empty() {
                    let _ = tx.send(paths);
                }
            }
        },
    );
    let mut debouncer = match debouncer {
        Ok(d) => d,
        Err(err) => {
            tracing::warn!("vault watcher failed to start: {err}");
            return;
        }
    };
    if let Err(err) = debouncer.watcher().watch(&root, RecursiveMode::Recursive) {
        tracing::warn!("vault watcher failed to watch {}: {err}", root.display());
        return;
    }
    tracing::info!("watching vault at {}", root.display());

    tokio::spawn(async move {
        let _debouncer = debouncer; // keep the watcher thread alive
        while let Some(paths) = rx.recv().await {
            let mut external_change = false;
            for path in paths {
                let Ok(rel) = path.strip_prefix(engine.vault.root()) else { continue };
                let rel = rel.to_string_lossy().to_string();
                // Self-event suppression: if the content hash matches our own
                // last write, this event is ours.
                let ours = match std::fs::read(&path) {
                    Ok(bytes) => {
                        let hash: [u8; 32] = Sha256::digest(&bytes).into();
                        engine
                            .suppress
                            .lock()
                            .unwrap()
                            .get(&rel)
                            .map(|recorded| *recorded == hash)
                            .unwrap_or(false)
                    }
                    Err(_) => false, // deleted / unreadable => external
                };
                if !ours {
                    external_change = true;
                    tracing::debug!("external vault change: {rel}");
                }
            }
            if external_change {
                match engine.reindex().await {
                    Ok(status) => tracing::info!(
                        "vault reindexed after external edit: {} entities, {} facts",
                        status.entities,
                        status.facts
                    ),
                    Err(err) => tracing::warn!("reindex after external edit failed: {err:#}"),
                }
            }
        }
    });
}
