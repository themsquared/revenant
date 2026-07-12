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
        let mut last_acted: Option<String> = None;
        let mut tick = tokio::time::interval(interval);
        loop {
            tick.tick().await;
            if let Err(err) = check_once(&home, triple, &cfg, &events, &mut last_acted).await {
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
    last_acted: &mut Option<String>,
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

    let current = crate::installed_release_tag(home);
    let current_cv = current.as_deref().and_then(crate::parse_calver).unwrap_or((0, 0, 0));
    if crate::parse_calver(&latest).unwrap_or((0, 0, 0)) <= current_cv {
        return Ok(()); // already current
    }
    // Only act once per newly-seen version (don't re-notify every tick).
    if last_acted.as_deref() == Some(latest.as_str()) {
        return Ok(());
    }
    *last_acted = Some(latest.clone());
    let channel_str = crate::channel_label(channel).to_string();

    match cfg.update.auto {
        AutoUpdate::Notify => {
            tracing::warn!(
                "update available: {} → {latest} ({channel_str})",
                current.as_deref().unwrap_or("source")
            );
            // A marker `revenant status` reads to surface the banner locally.
            let _ = std::fs::write(home.root().join("update-available"), &latest);
            events.emit(Event::UpdateAvailable { current, latest, channel: channel_str });
        }
        AutoUpdate::Install => {
            let (h, tag) = (home.clone(), latest.clone());
            let installed =
                tokio::task::spawn_blocking(move || crate::perform_update(&h, triple, &tag))
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
                    // Reset so the next tick retries this version.
                    *last_acted = None;
                    tracing::warn!("auto-update install failed (will retry): {e:#}");
                }
            }
        }
        AutoUpdate::Off => {}
    }
    Ok(())
}
