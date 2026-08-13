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
    if value.is_empty() {
        return "''".into();
    }
    format!("'{}'", value.replace('\'', "'\\''"))
}

#[cfg(target_os = "macos")]
fn launch_platform(command: &str, terminal_pref: Option<&str>) -> Result<()> {
    let app = terminal_pref.unwrap_or("Terminal");
    let badge = "printf '\\033]1337;SetBadgeFormat=dG9rZW1hbiDCtyBtYW5hZ2Vk\\007'; ";

    let script = match app {
        "iTerm2" | "iTerm" | "iterm2" | "iterm" => {
            let command = apple_script_escape(&format!("{badge}{command}"));
            format!(
                r#"tell application "iTerm2"
    activate
    create window with default profile
    tell current session of current window
        set name to "Claude · tokeman"
        write text "{badge}{command}"
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
    };

    let status = Command::new("osascript").arg("-e").arg(&script).status()?;
    if !status.success() {
        bail!("osascript exited with {status}");
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn apple_script_escape(value: &str) -> String {
    value.replace('\\', "\\\\").replace('"', "\\\"")
}

#[cfg(target_os = "linux")]
fn launch_platform(command: &str, terminal_pref: Option<&str>) -> Result<()> {
    let shell_command = format!("{command}; exec \"${{SHELL:-/bin/sh}}\"");

    let terminals: Vec<String> = if let Some(pref) = terminal_pref {
        vec![pref.to_string()]
    } else {
        vec![
            "x-terminal-emulator".into(),
            "gnome-terminal".into(),
            "konsole".into(),
            "xfce4-terminal".into(),
            "alacritty".into(),
            "kitty".into(),
            "wezterm".into(),
            "xterm".into(),
        ]
    };

    for terminal in &terminals {
        let result = match terminal.as_str() {
            "gnome-terminal" => Command::new(terminal)
                .arg("--")
                .arg("bash")
                .arg("-c")
                .arg(&shell_command)
                .spawn(),
            _ => Command::new(terminal)
                .arg("-e")
                .arg("bash")
                .arg("-c")
                .arg(&shell_command)
                .spawn(),
        };
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
    }
}
