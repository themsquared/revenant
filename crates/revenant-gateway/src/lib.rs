//! revenant-gateway: renders agentgateway config from harness state and
//! supervises the gateway as a child process.

mod render;
mod supervisor;

pub use render::render_gateway_yaml;
pub use supervisor::{GatewaySupervisor, SupervisorHandle};

use anyhow::{bail, Context, Result};
use revenant_core::config::Config;
use revenant_core::home::Home;
use sha2::{Digest, Sha256};
use std::path::PathBuf;

/// Resolve the agentgateway binary: explicit dev override, already-downloaded
/// pinned version, or download+verify from the GitHub release.
pub async fn ensure_binary(home: &Home, cfg: &Config) -> Result<PathBuf> {
    if let Some(path) = &cfg.gateway.binary {
        if !path.exists() {
            bail!("configured gateway.binary does not exist: {}", path.display());
        }
        return Ok(path.clone());
    }
    let version = &cfg.gateway.version;
    let bin_dir = home.gateway_bin_dir();
    let target = bin_dir.join(format!("agentgateway-v{version}"));
    if target.exists() {
        return Ok(target);
    }

    std::fs::create_dir_all(&bin_dir)?;
    let (os, arch) = release_platform()?;
    let base = format!(
        "https://github.com/agentgateway/agentgateway/releases/download/v{version}/agentgateway-{os}-{arch}"
    );
    tracing::info!("downloading agentgateway v{version} ({os}-{arch})");
    let http = reqwest::Client::new();

    let expected_sha = http
        .get(format!("{base}.sha256"))
        .send()
        .await?
        .error_for_status()
        .context("fetching gateway checksum")?
        .text()
        .await?;
    let expected_sha = expected_sha
        .split_whitespace()
        .next()
        .context("empty checksum file")?
        .to_lowercase();

    let bytes = http
        .get(&base)
        .send()
        .await?
        .error_for_status()
        .context("downloading gateway binary")?
        .bytes()
        .await?;
    let actual_sha = hex::encode(Sha256::digest(&bytes));
    if actual_sha != expected_sha {
        bail!("gateway binary checksum mismatch: expected {expected_sha}, got {actual_sha}");
    }

    let tmp = bin_dir.join(format!(".agentgateway-v{version}.tmp"));
    std::fs::write(&tmp, &bytes)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o755))?;
    }
    std::fs::rename(&tmp, &target)?;
    tracing::info!("agentgateway v{version} installed at {}", target.display());
    Ok(target)
}

fn release_platform() -> Result<(&'static str, &'static str)> {
    let os = match std::env::consts::OS {
        "macos" => "darwin",
        "linux" => "linux",
        other => bail!("unsupported OS for gateway download: {other}"),
    };
    let arch = match std::env::consts::ARCH {
        "aarch64" => "arm64",
        "x86_64" => "amd64",
        other => bail!("unsupported arch for gateway download: {other}"),
    };
    Ok((os, arch))
}

/// Render → validate → atomically swap the gateway config file.
/// agentgateway file-watches the config and hot-reloads, keeping last-good
/// on error; the `--validate-only` pass here catches mistakes even earlier.
/// Validation runs with the same env the child will get, since the gateway
/// expands `$VAR` references at load time.
pub async fn write_gateway_config(
    home: &Home,
    cfg: &Config,
    binary: &PathBuf,
    env: &[(String, String)],
) -> Result<()> {
    let available: std::collections::HashSet<String> =
        env.iter().map(|(k, _)| k.clone()).collect();
    let yaml = render_gateway_yaml(cfg, &available)?;
    std::fs::create_dir_all(home.gateway_dir())?;
    let next = home.gateway_config_next_path();
    let live = home.gateway_config_path();
    std::fs::write(&next, &yaml)?;

    let output = tokio::process::Command::new(binary)
        .arg("--validate-only")
        .arg("-f")
        .arg(&next)
        .env_clear()
        .env("PATH", std::env::var("PATH").unwrap_or_default())
        .envs(env.iter().map(|(k, v)| (k.as_str(), v.as_str())))
        .output()
        .await
        .context("running agentgateway --validate-only")?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("gateway config failed validation:\n{stderr}");
    }
    std::fs::rename(&next, &live)?;
    Ok(())
}

/// Parse `secrets.env` (KEY=VALUE lines) into env pairs for the child.
pub fn load_secrets(home: &Home) -> Result<Vec<(String, String)>> {
    let path = home.secrets_path();
    if !path.exists() {
        return Ok(vec![]);
    }
    let content = std::fs::read_to_string(&path)?;
    let mut out = Vec::new();
    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some((k, v)) = line.split_once('=') {
            out.push((k.trim().to_string(), v.trim().to_string()));
        }
    }
    Ok(out)
}
