//! Agent heartbeat — the daemon periodically publishes a signed `AgentProfile`
//! so this revenant appears (live, with its specs) on the horde roster and the
//! My Horde dashboard. Opt-in (`[network].heartbeat`), coarse specs only, and
//! it mirrors the agent's claimed Handle name so the roster reads in names, not
//! hashes. Fail-soft: a failed post just logs and retries next tick.

use revenant_core::config::Config;
use revenant_core::home::Home;
use revenant_net::profile::{AgentProfile, MachineSpecs};
use revenant_net::{Identity, NecropolisClient};
use std::time::Duration;

const INTERVAL_SECS: u64 = 900; // every 15 minutes — liveness, not chatter

/// Spawn the heartbeat worker unless the network or the feature is disabled.
pub fn spawn(home: Home, cfg: Config) {
    if !cfg.network.enabled || !cfg.network.heartbeat {
        return;
    }
    let Some(url) = cfg.network.necropolis_url.clone() else {
        tracing::info!("heartbeat: enabled but no [network].necropolis_url — not starting");
        return;
    };
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_secs(10)).await;
        let client = NecropolisClient::new(&url);
        let id = match Identity::load_or_create(&home.identity_dir()) {
            Ok(i) => i,
            Err(e) => {
                tracing::warn!("heartbeat: no network identity ({e:#}) — not starting");
                return;
            }
        };
        let specs = detect_specs();
        let caps = capabilities(&cfg);
        tracing::info!(
            "heartbeat: advertising {}·{} {}c/{}MB every {}s",
            specs.os, specs.arch, specs.cpus, specs.ram_mb, INTERVAL_SECS
        );
        loop {
            // Mirror the claimed handle (falls back to the deterministic lore-name).
            let name = client.name_of(&id.id()).await.unwrap_or_default();
            let profile =
                AgentProfile::create(&id, name, specs.clone(), caps.clone(), crate::now_ts());
            match client.post_profile(&profile).await {
                Ok(()) => tracing::debug!("heartbeat: posted"),
                Err(e) => tracing::debug!("heartbeat: post failed (will retry): {e:#}"),
            }
            tokio::time::sleep(Duration::from_secs(INTERVAL_SECS)).await;
        }
    });
}

/// Coarse, best-effort machine specs — std where possible, one cheap platform
/// read for RAM. Never anything that identifies the host on a network.
pub(crate) fn detect_specs() -> MachineSpecs {
    let cpus = std::thread::available_parallelism().map(|n| n.get() as u32).unwrap_or(0);
    MachineSpecs {
        os: std::env::consts::OS.to_string(),
        arch: std::env::consts::ARCH.to_string(),
        cpus,
        ram_mb: detect_ram_mb(),
        gpu: None, // cross-platform GPU detection isn't worth a dep here
    }
}

/// Total RAM in MB, best-effort per platform; 0 when unknown (schema stays stable).
fn detect_ram_mb() -> u64 {
    #[cfg(target_os = "macos")]
    {
        std::process::Command::new("sysctl")
            .args(["-n", "hw.memsize"])
            .output()
            .ok()
            .and_then(|o| String::from_utf8(o.stdout).ok())
            .and_then(|s| s.trim().parse::<u64>().ok())
            .map(|bytes| bytes / (1024 * 1024))
            .unwrap_or(0)
    }
    #[cfg(target_os = "linux")]
    {
        std::fs::read_to_string("/proc/meminfo")
            .ok()
            .and_then(|s| {
                s.lines().find_map(|l| {
                    l.strip_prefix("MemTotal:")
                        .and_then(|v| v.trim().trim_end_matches(" kB").trim().parse::<u64>().ok())
                })
            })
            .map(|kb| kb / 1024)
            .unwrap_or(0)
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        0
    }
}

/// A coarse, truthful capability list derived from what the daemon has enabled.
pub(crate) fn capabilities(cfg: &Config) -> Vec<String> {
    let mut caps = vec!["chat".to_string()];
    if cfg.memory.enabled {
        caps.push("memory".into());
    }
    if cfg.ascension.enabled {
        caps.push("ascend".into());
    }
    if cfg.network.discuss.enabled {
        caps.push("discuss".into());
    }
    caps
}
