//! `revenant service install|uninstall` — always-on daemon via the platform
//! service manager. launchd (user agent) on macOS, systemd --user on Linux.
//! The unit files are versioned by the binary, not templated by the installer.

use anyhow::{bail, Context, Result};
use revenant_core::home::Home;
use std::path::PathBuf;

const LABEL: &str = "dev.revenant.agent";

pub fn install() -> Result<()> {
    let exe = std::env::current_exe().context("resolving revenant binary path")?;
    match std::env::consts::OS {
        "macos" => install_launchd(&exe),
        "linux" => install_systemd(&exe),
        other => bail!("service install not supported on {other}; run `revenant up` under your own supervisor"),
    }
}

pub fn uninstall() -> Result<()> {
    match std::env::consts::OS {
        "macos" => {
            let plist = launchd_path()?;
            let _ = std::process::Command::new("launchctl")
                .args(["unload", &plist.to_string_lossy()])
                .status();
            if plist.exists() {
                std::fs::remove_file(&plist)?;
            }
            println!("uninstalled launchd agent {LABEL}");
        }
        "linux" => {
            let _ = std::process::Command::new("systemctl")
                .args(["--user", "disable", "--now", "revenant.service"])
                .status();
            let unit = systemd_path()?;
            if unit.exists() {
                std::fs::remove_file(&unit)?;
            }
            println!("uninstalled systemd unit revenant.service");
        }
        _ => {}
    }
    Ok(())
}

/// Restart the running service so it picks up a new binary/config — the missing
/// third verb alongside install/uninstall. Reuses each platform's manager
/// (unload+load on launchd, `systemctl restart` on systemd) rather than killing
/// the process, so the gateway child is torn down and respawned cleanly.
pub fn restart() -> Result<()> {
    match std::env::consts::OS {
        "macos" => {
            let plist = launchd_path()?;
            if !plist.exists() {
                bail!("service not installed — run `revenant service install` first");
            }
            let plist_s = plist.to_string_lossy().to_string();
            // unload is best-effort (may already be down); load must succeed.
            let _ = std::process::Command::new("launchctl")
                .args(["unload", &plist_s])
                .status();
            let status = std::process::Command::new("launchctl")
                .args(["load", &plist_s])
                .status()
                .context("launchctl load")?;
            if !status.success() {
                bail!("launchctl load failed");
            }
            println!("restarted launchd agent {LABEL}");
        }
        "linux" => {
            if !systemd_path()?.exists() {
                bail!("service not installed — run `revenant service install` first");
            }
            let status = std::process::Command::new("systemctl")
                .args(["--user", "restart", "revenant.service"])
                .status()
                .context("systemctl restart")?;
            if !status.success() {
                bail!("systemctl restart failed");
            }
            println!("restarted systemd unit revenant.service");
        }
        other => bail!("`service restart` is not supported on {other}"),
    }
    Ok(())
}

fn launchd_path() -> Result<PathBuf> {
    let home = dirs::home_dir().context("no home dir")?;
    Ok(home.join("Library/LaunchAgents").join(format!("{LABEL}.plist")))
}

fn install_launchd(exe: &std::path::Path) -> Result<()> {
    let home = Home::resolve();
    let logs = home.logs_dir();
    std::fs::create_dir_all(&logs)?;
    let plist_path = launchd_path()?;
    std::fs::create_dir_all(plist_path.parent().unwrap())?;

    let plist = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key><string>{label}</string>
  <key>ProgramArguments</key>
  <array><string>{exe}</string><string>up</string></array>
  <key>RunAtLoad</key><true/>
  <key>KeepAlive</key><true/>
  <key>StandardOutPath</key><string>{out}</string>
  <key>StandardErrorPath</key><string>{err}</string>
  <key>EnvironmentVariables</key>
  <dict><key>PATH</key><string>/usr/bin:/bin:/usr/sbin:/sbin:/usr/local/bin:/opt/homebrew/bin</string></dict>
</dict>
</plist>
"#,
        label = LABEL,
        exe = exe.display(),
        out = logs.join("daemon.out.log").display(),
        err = logs.join("daemon.err.log").display(),
    );
    std::fs::write(&plist_path, plist)?;
    // Reload (silence the expected error when nothing is loaded yet).
    let _ = std::process::Command::new("launchctl")
        .args(["unload", &plist_path.to_string_lossy()])
        .stderr(std::process::Stdio::null())
        .status();
    std::process::Command::new("launchctl")
        .args(["load", &plist_path.to_string_lossy()])
        .status()
        .context("launchctl load")?;
    println!("installed launchd agent {LABEL}");
    println!("  logs: {}", logs.join("daemon.err.log").display());
    println!("  stop: revenant service uninstall");
    Ok(())
}

fn systemd_path() -> Result<PathBuf> {
    let home = dirs::home_dir().context("no home dir")?;
    Ok(home.join(".config/systemd/user/revenant.service"))
}

fn install_systemd(exe: &std::path::Path) -> Result<()> {
    let unit_path = systemd_path()?;
    std::fs::create_dir_all(unit_path.parent().unwrap())?;
    let unit = format!(
        "[Unit]\n\
         Description=revenant agent\n\
         After=network-online.target\n\n\
         [Service]\n\
         ExecStart={exe} up\n\
         Restart=always\n\
         RestartSec=3\n\n\
         [Install]\n\
         WantedBy=default.target\n",
        exe = exe.display(),
    );
    std::fs::write(&unit_path, unit)?;
    let run = |args: &[&str]| {
        let _ = std::process::Command::new("systemctl").args(args).status();
    };
    run(&["--user", "daemon-reload"]);
    run(&["--user", "enable", "--now", "revenant.service"]);
    // Survive logout so the agent keeps running.
    let _ = std::process::Command::new("loginctl")
        .args(["enable-linger"])
        .status();
    println!("installed systemd unit revenant.service (enabled + started)");
    println!("  status: systemctl --user status revenant");
    println!("  stop:   revenant service uninstall");
    Ok(())
}
