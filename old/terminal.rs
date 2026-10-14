use anyhow::Result;
#[cfg_attr(target_os = "macos", allow(unused_imports))]
use anyhow::bail;
use std::process::Command;

/// Launch a new terminal window running Claude against the current startup default.
///
/// No OAuth credential is passed here. In particular, putting it in `export`
/// would leak the token into scrollback/process listings. Claude reads the
/// shared user setting at startup.
pub fn launch_in_terminal(
    claude_bin: &str,
    args: &[String],
    terminal_pref: Option<&str>,
) -> Result<()> {
    let mut command = shell_quote(claude_bin);
    for arg in args {
        command.push(' ');
        command.push_str(&shell_quote(arg));
    }
    launch_platform(&command, terminal_pref)
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

#[cfg(target_os = "macos")]
fn launch_platform(command: &str, terminal_pref: Option<&str>) -> Result<()> {
    let script = apple_script(terminal_pref.unwrap_or("Terminal"), command);
    let status = Command::new("osascript").arg("-e").arg(&script).status()?;
    if !status.success() {
        bail!("osascript exited with {status}");
    }
    Ok(())
}

/// Marks an iTerm2 window as launched by tokeman ("tokeman · managed").
#[cfg(target_os = "macos")]
const ITERM_BADGE: &str = "printf '\\033]1337;SetBadgeFormat=dG9rZW1hbiDCtyBtYW5hZ2Vk\\007'; ";

/// The AppleScript that opens `app` and runs the shell `command` in it.
#[cfg(target_os = "macos")]
fn apple_script(app: &str, command: &str) -> String {
    match app {
        "iTerm2" | "iTerm" | "iterm2" | "iterm" => {
            // The badge is part of the shell line, so it is escaped once with
            // the command; writing it a second time, raw, printed the escape
            // sequence as text in the new window.
            let command = apple_script_escape(&format!("{ITERM_BADGE}{command}"));
            format!(
                r#"tell application "iTerm2"
    activate
    create window with default profile
    tell current session of current window
        set name to "Claude · tokeman"
        write text "{command}"
    end tell
end tell"#
            )
        }
        _ => {
            let command = apple_script_escape(command);
            format!(
                r#"tell application "Terminal"
    activate
    do script "{command}"
end tell"#
            )
        }
    }
}

#[cfg(target_os = "macos")]
fn apple_script_escape(value: &str) -> String {
    value.replace('\\', "\\\\").replace('"', "\\\"")
}

#[cfg(target_os = "linux")]
fn launch_platform(command: &str, terminal_pref: Option<&str>) -> Result<()> {
    let shell_command = format!("{command}; exec \"${{SHELL:-/bin/sh}}\"");

    let preferred = terminal_pref.map(|pref| [pref]);
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
        let result = Command::new(terminal)
            .args([exec_flag, "bash", "-c", shell_command.as_str()])
            .spawn();
        if result.is_ok() {
            return Ok(());
        }
    }
    bail!("No terminal emulator found. Set terminal in settings.")
}

#[cfg(target_os = "windows")]
fn launch_platform(command: &str, _terminal_pref: Option<&str>) -> Result<()> {
    Command::new("cmd")
        .args(["/c", "start", "cmd", "/k", command])
        .spawn()?;
    Ok(())
}

#[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
fn launch_platform(_command: &str, _terminal_pref: Option<&str>) -> Result<()> {
    bail!("Terminal launching not supported on this platform")
}

#[cfg(test)]
mod tests {
    use super::shell_quote;

    #[test]
    fn shell_quote_preserves_spaces_and_single_quotes() {
        assert_eq!(shell_quote("hello world"), "'hello world'");
        assert_eq!(shell_quote("it's"), "'it'\\''s'");
        assert_eq!(shell_quote(""), "''");
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn iterm_script_sets_the_badge_once_and_escaped() {
        let script = super::apple_script("iTerm2", "'claude' '--model' 'opus'");
        assert_eq!(script.matches("SetBadgeFormat").count(), 1);
        // One backslash in the shell line becomes two in AppleScript source.
        assert!(script.contains("write text \"printf '\\\\033]1337;SetBadgeFormat="));
        assert!(script.contains("'claude' '--model' 'opus'\"\n"));

        let plain = super::apple_script("Terminal", "'claude'");
        assert!(!plain.contains("SetBadgeFormat"));
        assert!(plain.contains("do script \"'claude'\""));
    }
}
