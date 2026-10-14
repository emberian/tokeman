//! The per-user launchd service that runs `tokeman rotate daemon` (macOS).
//!
//! launchd starts a shell shim that repairs the binary's ad-hoc signature
//! before executing it: `cargo install` strips the linker signature, and
//! taskgated kills an unsigned replacement before `main` runs. The agent is
//! `Standard` priority: a Background agent is scheduled below every normal
//! process, and rotation matters most when the box is pegged.

use anyhow::Result;
use std::path::{Path, PathBuf};

use crate::paths;

#[cfg(target_os = "macos")]
use anyhow::{Context, anyhow, bail};
#[cfg(target_os = "macos")]
use std::process::Command;

pub const LAUNCHER: &str = r#"#!/bin/sh
set -eu
binary=$1
if ! /usr/bin/codesign --verify --strict "$binary" >/dev/null 2>&1; then
  /usr/bin/codesign --force --sign - --timestamp=none "$binary"
fi
exec "$binary" rotate daemon
"#;

pub fn plist(binary: &Path, launcher: &Path, home: &Path, log: &Path) -> String {
    let path = format!(
        "{}/.cargo/bin:/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin",
        home.display()
    );
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key>
  <string>{label}</string>
  <key>ProgramArguments</key>
  <array>
    <string>/bin/sh</string>
    <string>{launcher}</string>
    <string>{binary}</string>
  </array>
  <key>EnvironmentVariables</key>
  <dict>
    <key>HOME</key><string>{home}</string>
    <key>PATH</key><string>{path}</string>
  </dict>
  <key>RunAtLoad</key><true/>
  <key>KeepAlive</key><true/>
  <key>ThrottleInterval</key><integer>10</integer>
  <key>ProcessType</key><string>Standard</string>
  <key>StandardOutPath</key><string>{log}</string>
  <key>StandardErrorPath</key><string>{log}</string>
  <key>Umask</key><integer>63</integer>
</dict>
</plist>
"#,
        label = paths::LAUNCH_AGENT_LABEL,
        launcher = xml_escape(&launcher.to_string_lossy()),
        binary = xml_escape(&binary.to_string_lossy()),
        home = xml_escape(&home.to_string_lossy()),
        path = xml_escape(&path),
        log = xml_escape(&log.to_string_lossy()),
    )
}

pub fn xml_escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

pub fn is_installed() -> bool {
    paths::launch_agent_plist().exists()
}

/// Stop the running agent, if any. Done before taking the rotation lock,
/// since a daemon mid-probe can hold it for a network timeout.
#[cfg(target_os = "macos")]
pub fn stop() {
    let plist = paths::launch_agent_plist();
    if plist.exists()
        && let Ok(domain) = domain()
    {
        let _ = Command::new("/bin/launchctl")
            .args(["bootout", &domain, &plist.to_string_lossy()])
            .status();
    }
}

/// Write the shim and the plist for `binary`, then load and start the agent.
#[cfg(target_os = "macos")]
pub fn install(binary: &Path) -> Result<PathBuf> {
    let home = dirs::home_dir().context("could not find home directory")?;
    let launcher = paths::daemon_launcher();
    let plist_path = paths::launch_agent_plist();
    let log = paths::launchd_log();
    crate::fs::ensure_private_parent(&log)?;
    crate::fs::write_atomic(&launcher, LAUNCHER.as_bytes(), 0o700)?;
    crate::fs::write_atomic(
        &plist_path,
        plist(binary, &launcher, &home, &log).as_bytes(),
        0o644,
    )?;
    let lint = Command::new("/usr/bin/plutil")
        .args(["-lint", &plist_path.to_string_lossy()])
        .status()?;
    if !lint.success() {
        bail!("generated LaunchAgent plist failed validation");
    }
    let domain = domain()?;
    launchctl(["bootstrap", &domain, &plist_path.to_string_lossy()])?;
    launchctl(["enable", &format!("{domain}/{}", paths::LAUNCH_AGENT_LABEL)])?;
    launchctl([
        "kickstart",
        "-k",
        &format!("{domain}/{}", paths::LAUNCH_AGENT_LABEL),
    ])?;
    Ok(plist_path)
}

/// Stop the agent and remove its plist and shim.
#[cfg(target_os = "macos")]
pub fn uninstall() -> Result<()> {
    stop();
    for path in [paths::launch_agent_plist(), paths::daemon_launcher()] {
        if path.exists() {
            std::fs::remove_file(&path)
                .with_context(|| format!("failed to remove {}", path.display()))?;
        }
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn domain() -> Result<String> {
    let output = Command::new("/usr/bin/id").arg("-u").output()?;
    if !output.status.success() {
        bail!("failed to determine user id for launchctl");
    }
    Ok(format!("gui/{}", String::from_utf8(output.stdout)?.trim()))
}

#[cfg(target_os = "macos")]
fn launchctl<const N: usize>(args: [&str; N]) -> Result<()> {
    let status = Command::new("/bin/launchctl").args(args).status()?;
    if status.success() {
        Ok(())
    } else {
        Err(anyhow!("launchctl exited with {status}"))
    }
}

#[cfg(not(target_os = "macos"))]
pub fn stop() {}

#[cfg(not(target_os = "macos"))]
pub fn install(_binary: &Path) -> Result<PathBuf> {
    anyhow::bail!(
        "the background service is installed through launchd and needs macOS; run `tokeman rotate daemon` under your init system instead"
    )
}

#[cfg(not(target_os = "macos"))]
pub fn uninstall() -> Result<()> {
    anyhow::bail!("the background service is a launchd agent and needs macOS")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_plist_escapes_paths_and_runs_at_standard_priority() {
        let text = plist(
            Path::new("/Users/x y/bin/tokeman"),
            Path::new("/Users/x y/.config/tokeman/launch-rotate-daemon.sh"),
            Path::new("/Users/x y"),
            Path::new("/Users/x y/.config/tokeman/<log>"),
        );
        assert!(text.contains("<string>/Users/x y/bin/tokeman</string>"));
        assert!(text.contains("&lt;log&gt;"));
        assert!(text.contains("<key>ProcessType</key><string>Standard</string>"));
        assert!(!text.contains("LowPriorityIO"));
        assert!(text.contains(paths::LAUNCH_AGENT_LABEL));
    }
}
