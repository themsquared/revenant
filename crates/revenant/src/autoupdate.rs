//! Background auto-update.
//!
//! The daemon checks the configured release channel on an interval and, per
//! `[update].auto`, either notifies the owner or installs + restarts. It reuses
//! the exact same download → checksum-verify → atomic-swap core as
//! `revenant update`, so an unattended update is as safe as a manual one.
//!
//! Reliability first (the bar OpenClaw missed): every check is fail-soft — a
//! network hiccup, a GitHub rate-limit, or a bad parse just logs and waits for
//! the next tick. It NEVER panics the daemon, and in `notify` mode (the
//! default) it never swaps a binary or restarts anything.

use revenant_core::config::{AutoUpdate, Config};
use revenant_core::event::{Event, EventBus};
use revenant_core::home::Home;
use std::time::Duration;

/// Spawn the background updater unless it's off / unsupported. Cheap no-op
/// otherwise, so it's always safe to call from `cmd_up`.
pub fn spawn(home: Home, cfg: Config, events: EventBus) {
    if cfg.update.auto == AutoUpdate::Off {
        return;
    }
    let Some(triple) = crate::update_triple() else {
        tracing::info!("auto-update: this platform has no prebuilt release; skipping");
        return;
    };
    // Clamp the interval so a mis-set 0/tiny value can't hammer GitHub.
    let interval = Duration::from_secs(cfg.update.check_interval_secs.max(300));
    tokio::spawn(async move {
        // Don't slow startup; let the daemon settle first.
        tokio::time::sleep(Duration::from_secs(30)).await;
        let mut tick = tokio::time::interval(interval);
        loop {
            tick.tick().await;
            if let Err(err) = check_once(&home, triple, &cfg, &events).await {
                tracing::warn!("auto-update check failed (will retry): {err:#}");
            }
        }
    });
}

async fn check_once(
    home: &Home,
    triple: &'static str,
    cfg: &Config,
    events: &EventBus,
) -> anyhow::Result<()> {
    let channel = cfg.update.channel;
    // resolve_update_target shells out to curl (blocking) — keep it off the
    // async runtime's worker threads.
    let latest = tokio::task::spawn_blocking(move || crate::resolve_update_target(channel))
        .await
        .map_err(|e| anyhow::anyhow!("join: {e}"))??;
    let Some(latest) = latest else {
        return Ok(()); // no release on this channel yet
    };

    let marker = home.root().join("update-available");
    let current = crate::installed_release_tag(home);
    let current_cv = current.as_deref().and_then(crate::parse_calver).unwrap_or((0, 0, 0));
    if crate::parse_calver(&latest).unwrap_or((0, 0, 0)) <= current_cv {
        // Caught up (updated). Clear a stale banner so `status` stops nagging.
        let _ = std::fs::remove_file(&marker);
        return Ok(());
    }
    let channel_str = crate::channel_label(channel).to_string();

    match cfg.update.auto {
        AutoUpdate::Notify => {
            // Tell the owner ONCE per version, then never again — dedup on the
            // on-disk marker so it survives daemon restarts (the in-memory guard
            // reset on every restart, which made this feel spammy). The marker
            // also drives the `revenant status` banner (persists until updated).
            let already =
                std::fs::read_to_string(&marker).ok().map(|s| s.trim().to_string());
            if already.as_deref() == Some(latest.as_str()) {
                return Ok(()); // already announced this version
            }
            let _ = std::fs::write(&marker, &latest);
            tracing::warn!(
                "update available: {} → {latest} ({channel_str})",
                current.as_deref().unwrap_or("source")
            );
            events.emit(Event::UpdateAvailable { current, latest, channel: channel_str });
        }
        AutoUpdate::Install => {
            let (h, tag) = (home.clone(), latest.clone());
            let installed = tokio::task::spawn_blocking(move || {
                let installed = crate::perform_update(&h, triple, &tag)?;
                // Heal split installs: if the service unit launches a
                // different binary than the one just swapped, update it too —
                // otherwise the service restart cycle keeps booting the old
                // version forever (the failure seen live on the mini).
                match crate::sync_service_binary() {
                    Ok(Some(p)) => {
                        tracing::info!("auto-update: service binary synced ({})", p.display())
                    }
                    Ok(None) => {}
                    Err(e) => tracing::warn!(
                        "auto-update: service binary NOT synced — the service may keep \
                         running the old version (see `revenant doctor`): {e:#}"
                    ),
                }
                Ok::<_, anyhow::Error>(installed)
            })
            .await
            .map_err(|e| anyhow::anyhow!("join: {e}"))?;
            match installed {
                Ok(_) => {
                    // Deliberately do NOT self-restart: killing this process
                    // would orphan the supervised gateway child (it holds the
                    // LLM ports), and exit() skips the Drop cleanup that would
                    // reap it. The new binary is on disk; a clean restart —
                    // manual, or the service's own restart cycle — applies it.
                    // Safe on every platform, which is the whole point.
                    tracing::warn!("auto-update installed {latest} — restart to apply");
                    // Leave the marker so `revenant status` shows "restart to apply".
                    let _ = std::fs::write(home.root().join("update-available"), &latest);
                    events.emit(Event::UpdateInstalled { tag: latest, restarting: false });
                }
                Err(e) => {
                    tracing::warn!("auto-update install failed (will retry): {e:#}");
                }
            }
        }
        AutoUpdate::Off => {}
    }
    Ok(())
}
