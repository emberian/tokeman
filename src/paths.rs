//! Every location tokeman reads or writes, in one place.
//!
//! Config (`$XDG_CONFIG_HOME/tokeman`, default `~/.config/tokeman`) holds the
//! account file and every piece of daemon state, because all of it derives
//! from credentials. Data (`$XDG_DATA_HOME/tokeman`) holds the probe history.

use std::path::PathBuf;

fn home() -> PathBuf {
    dirs::home_dir().unwrap_or_else(|| PathBuf::from("."))
}

fn xdg(var: &str, default: &str) -> PathBuf {
    std::env::var_os(var)
        .map(PathBuf::from)
        .unwrap_or_else(|| home().join(default))
        .join("tokeman")
}

pub fn config_dir() -> PathBuf {
    xdg("XDG_CONFIG_HOME", ".config")
}

pub fn data_dir() -> PathBuf {
    xdg("XDG_DATA_HOME", ".local/share")
}

pub fn cache_dir() -> PathBuf {
    xdg("XDG_CACHE_HOME", ".cache")
}

/// `tokens.toml`: accounts, launch settings, policy, Codex profiles.
pub fn accounts_file() -> PathBuf {
    config_dir().join("tokens.toml")
}

pub fn accounts_lock() -> PathBuf {
    config_dir().join("tokens.lock")
}

/// Which credential settings.json offered, and when (JSONL of fingerprints).
pub fn credential_history() -> PathBuf {
    config_dir().join("claude-credential-history.jsonl")
}

/// Sessions bound by the SessionStart hook and the limits seen in their transcripts.
pub fn admission_state() -> PathBuf {
    config_dir().join("claude-admission-state.json")
}

pub fn admission_lock() -> PathBuf {
    config_dir().join("claude-admission-state.lock")
}

/// Serializes every change to Claude's settings and to rotation state.
pub fn rotation_lock() -> PathBuf {
    config_dir().join("claude-rotate.lock")
}

/// Present while rotation is paused.
pub fn pause_marker() -> PathBuf {
    config_dir().join("claude-rotate.paused")
}

pub fn cadence_state() -> PathBuf {
    config_dir().join("claude-rotate-state.json")
}

pub fn rotation_log() -> PathBuf {
    config_dir().join("claude-rotate.log")
}

pub fn launchd_log() -> PathBuf {
    config_dir().join("claude-rotate-launchd.log")
}

pub fn daemon_launcher() -> PathBuf {
    config_dir().join("launch-rotate-daemon.sh")
}

/// The /login Keychain record rotation set aside, so pause can put it back.
pub fn login_backup() -> PathBuf {
    config_dir().join("claude-login-keychain-backup.json")
}

/// Cached `/api/oauth/usage` answers and their backoff.
pub fn profile_cache() -> PathBuf {
    config_dir().join("claude-profile-reads.json")
}

pub fn snapshots_db() -> PathBuf {
    data_dir().join("snapshots.db")
}

pub fn default_chart() -> PathBuf {
    cache_dir().join("chart.png")
}

pub const LAUNCH_AGENT_LABEL: &str = "com.ember.tokeman-claude-rotate";

pub fn launch_agent_plist() -> PathBuf {
    home()
        .join("Library/LaunchAgents")
        .join(format!("{LAUNCH_AGENT_LABEL}.plist"))
}

pub fn claude_dir() -> PathBuf {
    home().join(".claude")
}

/// `~/.claude/settings.json`, or `$CLAUDE_SETTINGS` for tests and odd layouts.
pub fn claude_settings() -> PathBuf {
    std::env::var_os("CLAUDE_SETTINGS")
        .map(PathBuf::from)
        .unwrap_or_else(|| claude_dir().join("settings.json"))
}

/// Expand a leading `~/` to the home directory.
pub fn expand_tilde(path: &str) -> PathBuf {
    match path.strip_prefix("~/") {
        Some(rest) => home().join(rest),
        None => PathBuf::from(path),
    }
}

/// The inverse of [`expand_tilde`], for display and for portable config values.
pub fn tilde(path: &std::path::Path) -> String {
    let display = path.display().to_string();
    let home = home().display().to_string();
    match display.strip_prefix(&home) {
        Some(rest) if rest.is_empty() || rest.starts_with('/') => format!("~{rest}"),
        _ => display,
    }
}
