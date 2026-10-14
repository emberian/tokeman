//! `tokeman launch`: pick the default, then start claude, in this terminal
//! or a new one.

use anyhow::{Context, Result, bail};
use std::process::Command;

use crate::account::Fleet;
use crate::daemon::cycle::{self, Options};

const TAG: &str = " \x1b[1mtokeman:\x1b[0m";

/// Run one cycle, then exec claude with the configured arguments. With
/// `auto`, keep cycling on the sip cadence while claude runs.
pub async fn run(fleet: Fleet, auto: bool, claude_args: Vec<String>) -> Result<()> {
    if fleet.accounts.is_empty() {
        bail!(
            "No accounts configured. Use `tokeman login <name>` or `tokeman add <name> <key>` to add one."
        );
    }
    eprintln!(
        "{TAG} probing {} accounts and selecting the default...",
        fleet.accounts.len()
    );
    let outcome = cycle::run(&fleet, Options::default()).await?;
    eprintln!("{TAG} {}", outcome.message);
    let (binary, args) = fleet.launch.command(claude_args);
    let shown: Vec<String> = args.iter().map(|arg| format!("{arg:?}")).collect();
    eprintln!("{TAG} launching {binary} {}", shown.join(" "));
    if auto {
        eprintln!(
            "{TAG} default monitor enabled ({}s normal / {}s sip-and-drain)",
            fleet.policy.normal_probe_interval_secs, fleet.policy.sip_probe_interval_secs
        );
    }
    eprintln!();
    // The child reads its credential from settings.json like every other
    // session; an inherited token would pin it to the launch-time account.
    let mut child = tokio::process::Command::new(&binary)
        .args(&args)
        .env_remove("CLAUDE_CODE_OAUTH_TOKEN")
        .spawn()
        .with_context(|| format!("failed to launch {binary}"))?;
    let monitor = auto.then(|| {
        let fleet = fleet.clone();
        tokio::spawn(async move {
            let mut wake = tokio::time::interval(std::time::Duration::from_secs(
                fleet.policy.sip_probe_interval_secs.max(1),
            ));
            wake.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            wake.tick().await; // the launch itself just ran a cycle
            loop {
                wake.tick().await;
                let scheduled = Options {
                    scheduled: true,
                    ..Options::default()
                };
                match cycle::run(&fleet, scheduled).await {
                    Ok(outcome) if outcome.changed() => {
                        eprintln!("\n{TAG}\x1b[32m {}\x1b[0m", outcome.message)
                    }
                    Ok(outcome) if outcome.action == cycle::Action::NoneViable => {
                        eprintln!("\n{TAG}\x1b[33m {}\x1b[0m", outcome.message)
                    }
                    Ok(_) => {}
                    Err(error) => {
                        eprintln!("\n{TAG}\x1b[31m rotation probe failed: {error:#}\x1b[0m")
                    }
                }
            }
        })
    });
    let status = child.wait().await?;
    if let Some(monitor) = monitor {
        monitor.abort();
    }
    if !status.success() {
        std::process::exit(status.code().unwrap_or(1));
    }
    Ok(())
}

/// Open a new terminal window running claude (the trays use this).
pub fn in_terminal(binary: &str, args: &[String], terminal: Option<&str>) -> Result<()> {
    let mut command = shell_quote(binary);
    for arg in args {
        command.push(' ');
        command.push_str(&shell_quote(arg));
    }
    open_terminal(&command, terminal)
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

#[cfg(target_os = "macos")]
fn open_terminal(command: &str, terminal: Option<&str>) -> Result<()> {
    let script = apple_script(terminal.unwrap_or("Terminal"), command);
    let status = Command::new("osascript").arg("-e").arg(&script).status()?;
    if !status.success() {
        bail!("osascript exited with {status}");
    }
    Ok(())
}

#[cfg(any(target_os = "macos", test))]
const ITERM_BADGE: &str = "printf '\\033]1337;SetBadgeFormat=dG9rZW1hbiDCtyBtYW5hZ2Vk\\007'; ";

/// The badge is part of the shell line, escaped once with the command.
#[cfg(any(target_os = "macos", test))]
fn apple_script(app: &str, command: &str) -> String {
    match app {
        "iTerm2" | "iTerm" | "iterm2" | "iterm" => {
            let command = apple_script_escape(&format!("{ITERM_BADGE}{command}"));
            format!(
                "tell application \"iTerm2\"\n    activate\n    create window with default profile\n    tell current session of current window\n        set name to \"Claude · tokeman\"\n        write text \"{command}\"\n    end tell\nend tell"
            )
        }
        _ => {
            let command = apple_script_escape(command);
            format!(
                "tell application \"Terminal\"\n    activate\n    do script \"{command}\"\nend tell"
            )
        }
    }
}

#[cfg(any(target_os = "macos", test))]
fn apple_script_escape(value: &str) -> String {
    value.replace('\\', "\\\\").replace('"', "\\\"")
}

#[cfg(target_os = "linux")]
fn open_terminal(command: &str, terminal: Option<&str>) -> Result<()> {
    let shell_command = format!("{command}; exec \"${{SHELL:-/bin/sh}}\"");
    let preferred = terminal.map(|pref| [pref]);
    let terminals: &[&str] = match &preferred {
        Some(pref) => pref,
        None => &[
            "x-terminal-emulator",
            "gnome-terminal",
            "konsole",
            "xfce4-terminal",
            "alacritty",
            "kitty",
            "wezterm",
            "xterm",
        ],
    };
    for &terminal in terminals {
        let exec_flag = if terminal == "gnome-terminal" {
            "--"
        } else {
            "-e"
        };
        if Command::new(terminal)
            .args([exec_flag, "bash", "-c", shell_command.as_str()])
            .spawn()
            .is_ok()
        {
            return Ok(());
        }
    }
    bail!("No terminal emulator found. Set terminal in settings.")
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn open_terminal(command: &str, _terminal: Option<&str>) -> Result<()> {
    Command::new("cmd")
        .args(["/c", "start", "cmd", "/k", command])
        .spawn()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn iterm_script_sets_the_badge_once_and_escaped() {
        let script = apple_script("iTerm2", "'claude' '--model' \"x\"");
        assert_eq!(script.matches("SetBadgeFormat").count(), 1);
        assert!(script.contains("write text \"printf '\\\\033]1337;SetBadgeFormat="));
        assert!(script.contains("\\\"x\\\""));
        assert!(apple_script("Terminal", "'claude'").contains("do script \"'claude'\""));
        assert_eq!(shell_quote("it's"), "'it'\\''s'");
    }
}
