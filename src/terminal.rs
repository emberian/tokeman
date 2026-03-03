#[cfg_attr(target_os = "macos", allow(unused_imports))]
use anyhow::bail;
use anyhow::Result;
use std::process::Command;

/// Launch a new terminal window running `claude` with the given token.
pub fn launch_in_terminal(
    claude_bin: &str,
    args: &[String],
    token_key: &str,
    terminal_pref: Option<&str>,
) -> Result<()> {
    let mut full_args = vec![claude_bin.to_string()];
    full_args.extend(args.iter().cloned());
    let cmd_str = full_args.join(" ");

    launch_platform(&cmd_str, token_key, terminal_pref)
}

#[cfg(target_os = "macos")]
fn launch_platform(cmd: &str, token_key: &str, terminal_pref: Option<&str>) -> Result<()> {
    let app = terminal_pref.unwrap_or("Terminal");
    let escaped_key = token_key.replace('\'', "'\\''");
    let escaped_cmd = cmd.replace('\'', "'\\''");

    let script = match app {
        "iTerm2" | "iTerm" | "iterm2" | "iterm" => {
            format!(
                r#"tell application "iTerm2"
    create window with default profile
    tell current session of current window
        write text "export CLAUDE_CODE_OAUTH_TOKEN='{escaped_key}'; {escaped_cmd}"
    end tell
end tell"#
            )
        }
        _ => {
            format!(
                r#"tell application "Terminal"
    activate
    do script "export CLAUDE_CODE_OAUTH_TOKEN='{escaped_key}'; {escaped_cmd}"
end tell"#
            )
        }
    };

    Command::new("osascript").arg("-e").arg(&script).spawn()?;
    Ok(())
}

#[cfg(target_os = "linux")]
fn launch_platform(cmd: &str, token_key: &str, terminal_pref: Option<&str>) -> Result<()> {
    let shell_cmd = format!(
        "export CLAUDE_CODE_OAUTH_TOKEN='{}'; {}; exec $SHELL",
        token_key.replace('\'', "'\\''"),
        cmd.replace('\'', "'\\''"),
    );

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

    for term in &terminals {
        let result = match term.as_str() {
            "gnome-terminal" => Command::new(term)
                .arg("--")
                .arg("bash")
                .arg("-c")
                .arg(&shell_cmd)
                .spawn(),
            _ => Command::new(term)
                .arg("-e")
                .arg("bash")
                .arg("-c")
                .arg(&shell_cmd)
                .spawn(),
        };
        if result.is_ok() {
            return Ok(());
        }
    }
    bail!("No terminal emulator found. Set terminal in settings.")
}

#[cfg(target_os = "windows")]
fn launch_platform(cmd: &str, token_key: &str, _terminal_pref: Option<&str>) -> Result<()> {
    Command::new("cmd")
        .args([
            "/c",
            "start",
            "cmd",
            "/k",
            &format!("set CLAUDE_CODE_OAUTH_TOKEN={}&& {}", token_key, cmd),
        ])
        .spawn()?;
    Ok(())
}

#[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
fn launch_platform(_cmd: &str, _token_key: &str, _terminal_pref: Option<&str>) -> Result<()> {
    bail!("Terminal launching not supported on this platform")
}
