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
use std::process::{Command, Output, Stdio};
#[cfg(target_os = "macos")]
use std::time::{Duration, Instant};

#[cfg(target_os = "macos")]
const CMD_TIMEOUT: Duration = Duration::from_secs(15);
#[cfg(target_os = "macos")]
const MAX_LAUNCHD_LOG_BYTES: u64 = 4 * 1024 * 1024;

#[cfg(target_os = "macos")]
fn run_bounded(program: &str, args: &[&str]) -> Result<Output> {
    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("failed to start {program}"))?;
    let deadline = Instant::now() + CMD_TIMEOUT;
    loop {
        if child.try_wait()?.is_some() {
            return child
                .wait_with_output()
                .with_context(|| format!("failed to collect output from {program}"));
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            bail!("{program} timed out after {}s", CMD_TIMEOUT.as_secs());
        }
        std::thread::sleep(Duration::from_millis(25));
    }
}

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
        let _ = run_bounded(
            "/bin/launchctl",
            &["bootout", &domain, &plist.to_string_lossy()],
        );
    }
}

/// Write the shim and the plist for `binary`, then load and start the agent.
#[cfg(target_os = "macos")]
pub fn install(binary: &Path) -> Result<PathBuf> {
    let home = paths::home();
    let launcher = paths::daemon_launcher();
    let plist_path = paths::launch_agent_plist();
    let log = paths::launchd_log();
    crate::fs::ensure_private_parent(&log)?;
    if std::fs::metadata(&log).is_ok_and(|meta| meta.len() > MAX_LAUNCHD_LOG_BYTES) {
        let old = log.with_extension("log.old");
        let _ = std::fs::rename(&log, &old);
        let _ = crate::fs::set_mode(&old, 0o600);
    }
    crate::fs::write_atomic(&launcher, LAUNCHER.as_bytes(), 0o700)?;
    crate::fs::write_atomic(
        &plist_path,
        plist(binary, &launcher, &home, &log).as_bytes(),
        0o644,
    )?;
    let lint = run_bounded("/usr/bin/plutil", &["-lint", &plist_path.to_string_lossy()])?;
    if !lint.status.success() {
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
    let output = run_bounded("/usr/bin/id", &["-u"])?;
    if !output.status.success() {
        bail!("failed to determine user id for launchctl");
    }
    Ok(format!("gui/{}", String::from_utf8(output.stdout)?.trim()))
}

#[cfg(target_os = "macos")]
fn launchctl<const N: usize>(args: [&str; N]) -> Result<()> {
    let output = run_bounded("/bin/launchctl", &args)?;
    if output.status.success() {
        Ok(())
    } else {
        Err(anyhow!("launchctl exited with {}", output.status))
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
