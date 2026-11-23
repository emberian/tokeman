//! `tokeman claude run <name> [args]...`: start a `claude` process pinned to one
//! account, side by side with the global rotated default.
//!
//! Because Claude Code reloads `~/.claude/settings.json`'s `env` block whenever
//! the background rotation daemon rewrites it, env variables alone do not keep
//! a process on an explicit account unless its `CLAUDE_CONFIG_DIR` points at an
//! isolated config home with its own `settings.json`. Each account gets a `0700`
//! directory under [`paths::claude_homes`] holding its own `settings.json` and
//! symlinks to the user's shared `~/.claude` resources (`CLAUDE.md`, `skills`,
//! `commands`, `agents`, `projects`, `plugins`).

use std::path::Path;

use anyhow::{Context, Result};

use super::settings::{self, ACCOUNT, OAUTH_SCOPES, OAUTH_TOKEN, Settings, WAIT_401, WAIT_401_MS};
use crate::account::{Account, Credential, Fleet};
use crate::daemon;
use crate::fs::set_mode;
use crate::history::EventKind;
use crate::launch::Launch;
use crate::paths;
use crate::quota::Service;

/// Entries of `~/.claude` linked into each per-account `CLAUDE_CONFIG_DIR` when
/// present so skills, commands, project memory and user rules stay visible.
const SHARED: &[&str] = &[
    "CLAUDE.md",
    "skills",
    "commands",
    "agents",
    "projects",
    "plugins",
    "keybindings.json",
];

/// Filesystem-safe directory name for `account` under [`paths::claude_homes`].
pub fn home_name(account: &str) -> String {
    account
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || "@.+-_".contains(c) {
                c
            } else {
                '_'
            }
        })
        .collect::<String>()
        .trim_start_matches('.')
        .to_owned()
}

/// Remove `--model <m>` or `--model=<m>` from `base` when `extra` specifies its own.
fn strip_conflicting_model(base: &[String], extra: &[String]) -> Vec<String> {
    let extra_sets_model = extra
        .iter()
        .any(|arg| arg == "--model" || arg.starts_with("--model="));
    if !extra_sets_model {
        return base.to_vec();
    }
    let mut out = Vec::with_capacity(base.len());
    let mut skip_next = false;
    for arg in base {
        if skip_next {
            skip_next = false;
            continue;
        }
        if arg == "--model" {
            skip_next = true;
            continue;
        }
        if arg.starts_with("--model=") {
            continue;
        }
        out.push(arg.clone());
    }
    out
}

fn explicit_model(args: &[String]) -> Option<&str> {
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        if arg == "--model" {
            return iter.next().map(String::as_str);
        }
        if let Some(val) = arg.strip_prefix("--model=") {
            return Some(val);
        }
    }
    None
}

/// Materialize `account`'s isolated `CLAUDE_CONFIG_DIR` at `home_dir`, seeded from
/// `real_claude_dir`, and build the [`Launch`] that seats the child on `account`.
pub fn prepare_in(
    fleet: &Fleet,
    account: &Account,
    now_ms: i64,
    real_claude_dir: &Path,
    homes_root: &Path,
    extra_args: Vec<String>,
) -> Result<Launch> {
    let home = homes_root.join(home_name(&account.name));
    std::fs::create_dir_all(&home)
        .with_context(|| format!("failed to create {}", home.display()))?;
    set_mode(&home, 0o700)?;

    #[cfg(unix)]
    for entry in SHARED {
        let src = real_claude_dir.join(entry);
        let dst = home.join(entry);
        if src.exists() && std::fs::symlink_metadata(&dst).is_err() {
            let _ = std::os::unix::fs::symlink(&src, &dst);
        }
    }

    let base_settings_path = real_claude_dir.join("settings.json");
    let mut settings: Settings = settings::load_from(&base_settings_path).unwrap_or_default();
    settings::clear(&mut settings);
    if let Some(m) = explicit_model(&extra_args) {
        settings.insert("model".into(), serde_json::Value::String(m.to_owned()));
    }
    settings::install(&mut settings, account, now_ms, EventKind::Activate)?;
    settings::save_to(&home.join("settings.json"), &settings)?;

    let credential = account.credential(now_ms).with_context(|| {
        format!(
            "{} has no usable credential; run `tokeman claude login {}`",
            account.name, account.name
        )
    })?;

    let (binary, _) = fleet.launch.command(std::iter::empty::<String>());
    let mut args = strip_conflicting_model(&fleet.launch.launch_args, &extra_args);
    args.extend(extra_args);
    const SKIP: &str = "--dangerously-skip-permissions";
    if fleet.launch.dangerous_mode && !args.iter().any(|arg| arg == SKIP) {
        args.push(SKIP.into());
    }

    let mut launch = Launch::new(Service::Claude, &account.name, None, binary, args)
        .unset("ANTHROPIC_API_KEY")
        .unset("ANTHROPIC_AUTH_TOKEN")
        .unset(OAUTH_SCOPES)
        .set("CLAUDE_CONFIG_DIR", home.into_os_string())
        .set(ACCOUNT, account.name.clone())
        .set(WAIT_401, WAIT_401_MS)
        .set_secret(OAUTH_TOKEN, credential.value());

    if let Credential::Login { scopes, .. } = credential
        && !scopes.is_empty()
    {
        launch = launch.set(OAUTH_SCOPES, scopes.join(" "));
    }
    Ok(launch)
}

/// Refresh `name` if due, prepare its isolated config home, and return the [`Launch`].
pub async fn prepare(name: &str, extra_args: Vec<String>) -> Result<Launch> {
    let _ = daemon::refresh::refresh_now(Some(name)).await;
    let fleet = Fleet::load()?;
    let account = fleet
        .account(name)
        .with_context(|| format!("account '{name}' is not configured"))?;
    let now_ms = chrono::Utc::now().timestamp_millis();
    prepare_in(
        &fleet,
        account,
        now_ms,
        &paths::claude_dir(),
        &paths::claude_homes(),
        extra_args,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn isolated_run_writes_per_account_settings_without_touching_global() {
        let dir = tempfile::tempdir().unwrap();
        let real_claude = dir.path().join("real-claude");
        let homes = dir.path().join("homes");
        std::fs::create_dir_all(&real_claude).unwrap();
        std::fs::write(
            real_claude.join("settings.json"),
            r#"{"model":"claude-fable-5-2","env":{"TOKEMAN_ACCOUNT":"bobb@ember.software","CLAUDE_CODE_OAUTH_TOKEN":"sk-bobb"}}"#,
        )
        .unwrap();
        std::fs::write(real_claude.join("CLAUDE.md"), "# rules\n").unwrap();

        let mut fleet = Fleet::default();
        fleet.launch.launch_args = vec!["--model".into(), "claude-fable-5-2".into()];
        fleet.add_setup_key("tobb@ember.software", "sk-tobb-token".into());
        let account = fleet.account("tobb@ember.software").unwrap();
        let launch = prepare_in(
            &fleet,
            account,
            1_500_000,
            &real_claude,
            &homes,
            vec!["-p".into(), "--model".into(), "claude-opus-5-5".into()],
        )
        .unwrap();

        // Global settings.json stays on bobb@ember.software.
        let global = settings::load_from(&real_claude.join("settings.json")).unwrap();
        assert_eq!(settings::env(&global, ACCOUNT), Some("bobb@ember.software"));

        // Isolated settings.json carries tobb@ember.software and updated model.
        let isolated_dir = homes.join("tobb@ember.software");
        let isolated = settings::load_from(&isolated_dir.join("settings.json")).unwrap();
        assert_eq!(
            settings::env(&isolated, ACCOUNT),
            Some("tobb@ember.software")
        );
        assert_eq!(settings::configured_token(&isolated), Some("sk-tobb-token"));
        assert_eq!(settings::target_model(&isolated), Some("claude-opus-5-5"));
        assert!(isolated_dir.join("CLAUDE.md").exists());

        // Conflicting --model from launch_args was stripped in favor of explicit --model.
        assert_eq!(launch.args(), ["-p", "--model", "claude-opus-5-5"]);
    }
}
