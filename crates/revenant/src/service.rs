//! `revenant service install|uninstall` — always-on daemon via the platform
//! service manager. launchd (user agent) on macOS, systemd --user on Linux.
//! The unit files are versioned by the binary, not templated by the installer.

use anyhow::{bail, Context, Result};
use revenant_core::home::Home;
use std::path::PathBuf;

const LABEL: &str = "dev.revenant.agent";

pub fn install() -> Result<()> {
    let exe = service_exe()?;
    match std::env::consts::OS {
        "macos" => install_launchd(&exe),
        "linux" => install_systemd(&exe),
        other => bail!("service install not supported on {other}; run `revenant up` under your own supervisor"),
    }
}

/// The path the unit file should launch. Resolves symlinks so the unit points
/// at the real file, and steers a legacy `~/.local/bin` COPY back to the
/// updater-managed `~/.revenant/bin` binary — a unit baked against a stray
/// copy never sees another update (`revenant update` swaps one file while the
/// service keeps launching the other; seen live on the mini).
fn service_exe() -> Result<std::path::PathBuf> {
    let exe = std::env::current_exe().context("resolving revenant binary path")?;
    let exe = exe.canonicalize().unwrap_or(exe);
    let managed = Home::resolve().root().join("bin/revenant");
    if exe == crate::dirs_local_bin() && managed.exists() {
        let managed = managed.canonicalize().unwrap_or(managed);
        println!(
            "note: pointing the service at {} (the auto-updated binary), not the copy at {}",
            managed.display(),
            exe.display()
        );
        return Ok(managed);
    }
    Ok(exe)
}

/// The binary path the installed service actually launches, parsed out of the
/// platform unit file. None when no service is installed (or the unit doesn't
/// name a revenant binary). This is the ground truth the updater and doctor
/// compare against — NOT where `service install` would put it today.
pub fn configured_binary() -> Option<PathBuf> {
    match std::env::consts::OS {
        "macos" => binary_from_plist(&std::fs::read_to_string(launchd_path().ok()?).ok()?),
        "linux" => binary_from_unit(&std::fs::read_to_string(systemd_path().ok()?).ok()?),
        _ => None,
    }
}

/// The ProgramArguments executable in a launchd plist. Plists may pack the
/// whole `<array>` on one line, so scan `<string>…</string>` fragments rather
/// than lines.
fn binary_from_plist(plist: &str) -> Option<PathBuf> {
    plist
        .split("<string>")
        .skip(1)
        .filter_map(|frag| frag.split("</string>").next())
        .find(|v| v.ends_with("/revenant"))
        .map(PathBuf::from)
}

/// The executable (first token) of a systemd unit's `ExecStart=` line.
fn binary_from_unit(unit: &str) -> Option<PathBuf> {
    unit.lines()
        .find_map(|l| l.trim().strip_prefix("ExecStart="))
        .and_then(|rest| rest.split_whitespace().next())
        .map(PathBuf::from)
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

/// Is the always-on service installed (unit/plist present)? Best-effort, for
/// `revenant doctor` — distinct from "is the daemon currently up", which the
/// health check answers. None on unsupported platforms.
pub fn is_installed() -> Option<bool> {
    match std::env::consts::OS {
        "macos" => Some(launchd_path().map(|p| p.exists()).unwrap_or(false)),
        "linux" => Some(systemd_path().map(|p| p.exists()).unwrap_or(false)),
        _ => None,
    }
}

fn launchd_path() -> Result<PathBuf> {
    let home = dirs::home_dir().context("no home dir")?;
    Ok(home.join("Library/LaunchAgents").join(format!("{LABEL}.plist")))
}

fn render_plist(exe: &std::path::Path, out: &std::path::Path, err: &std::path::Path) -> String {
    format!(
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
        out = out.display(),
        err = err.display(),
    )
}

fn install_launchd(exe: &std::path::Path) -> Result<()> {
    let home = Home::resolve();
    let logs = home.logs_dir();
    std::fs::create_dir_all(&logs)?;
    let plist_path = launchd_path()?;
    std::fs::create_dir_all(plist_path.parent().unwrap())?;

    let plist =
        render_plist(exe, &logs.join("daemon.out.log"), &logs.join("daemon.err.log"));
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

fn render_unit(exe: &std::path::Path) -> String {
    format!(
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
    )
}

fn install_systemd(exe: &std::path::Path) -> Result<()> {
    let unit_path = systemd_path()?;
    std::fs::create_dir_all(unit_path.parent().unwrap())?;
    std::fs::write(&unit_path, render_unit(exe))?;
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

#[cfg(test)]
mod tests {
    use super::{binary_from_plist, binary_from_unit, render_plist, render_unit};
    use std::path::{Path, PathBuf};

    // The invariant the split-install fix rests on: whatever path we write
    // into a unit file, configured_binary's parser reads the same path back.
    #[test]
    fn plist_round_trips_the_exe_path() {
        let exe = Path::new("/Users/mike/.revenant/bin/revenant");
        let plist =
            render_plist(exe, Path::new("/tmp/out.log"), Path::new("/tmp/err.log"));
        assert_eq!(binary_from_plist(&plist), Some(exe.to_path_buf()));
    }

    #[test]
    fn unit_round_trips_the_exe_path() {
        let exe = Path::new("/home/mike/.revenant/bin/revenant");
        assert_eq!(binary_from_unit(&render_unit(exe)), Some(exe.to_path_buf()));
    }

    // Real-world plists (launchctl, editors) often pack the ProgramArguments
    // array on a single line — the parser must not depend on line structure.
    #[test]
    fn plist_parser_handles_packed_single_line_array() {
        let plist = "<dict><key>ProgramArguments</key><array><string>/Users/mike/.local/bin/revenant</string><string>up</string></array></dict>";
        assert_eq!(
            binary_from_plist(plist),
            Some(PathBuf::from("/Users/mike/.local/bin/revenant"))
        );
    }

    // Log paths etc. must not be mistaken for the executable.
    #[test]
    fn plist_parser_skips_non_binary_strings() {
        let plist = "<array><string>/tmp/daemon.out.log</string><string>up</string></array>";
        assert_eq!(binary_from_plist(plist), None);
    }

    #[test]
    fn unit_parser_takes_first_execstart_token() {
        let unit = "[Service]\nExecStart=/opt/revenant/bin/revenant up --flag\nRestart=always\n";
        assert_eq!(binary_from_unit(unit), Some(PathBuf::from("/opt/revenant/bin/revenant")));
    }
}
