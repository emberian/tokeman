//! `~/.claude/settings.json`: the `env` block that hands Claude its credential
//! and the SessionStart hook that reports new sessions. Nothing else in
//! tokeman writes this file.
//!
//! Claude copies `env` into its process environment on every change and
//! rebuilds its API credentials from it, so every live session spends the
//! account installed here from its next request.

use std::path::Path;

use anyhow::{Context, Result};
use serde_json::{Map, Value, json};

use crate::account::{Account, Credential};
use crate::fs::{read_if_present, write_atomic};
use crate::history::{self, Event, EventKind};
use crate::paths;

pub const OAUTH_TOKEN: &str = "CLAUDE_CODE_OAUTH_TOKEN";
/// Claude assumes an env token is inference-only unless its scopes are
/// declared, and gates /feedback, Remote Control and profile reads on that.
pub const OAUTH_SCOPES: &str = "CLAUDE_CODE_OAUTH_SCOPES";
/// How long a refused request waits for settings to offer a replacement.
/// Claude's default is zero, which turns an expired token into a failed
/// request instead of a pause.
pub const WAIT_401: &str = "CLAUDE_CODE_OAUTH_401_WAIT_MS";
pub const WAIT_401_MS: &str = "120000";
/// The account name the installed credential belongs to.
pub const ACCOUNT: &str = "TOKEMAN_ACCOUNT";

const MANAGED_KEYS: [&str; 4] = [OAUTH_TOKEN, ACCOUNT, OAUTH_SCOPES, WAIT_401];

/// The settings document, as Claude wrote it.
pub type Settings = Map<String, Value>;

pub fn load() -> Result<Settings> {
    load_from(&paths::claude_settings())
}

fn load_from(path: &Path) -> Result<Settings> {
    let Some(contents) = read_if_present(path)? else {
        return Ok(Settings::new());
    };
    let value: Value = serde_json::from_str(&contents)
        .with_context(|| format!("failed to parse Claude settings {}", path.display()))?;
    match value {
        Value::Object(settings) => Ok(settings),
        _ => anyhow::bail!("Claude settings {} must be a JSON object", path.display()),
    }
}

/// Owner-only: the file carries a bearer token once tokeman manages it.
pub fn save(settings: &Settings) -> Result<()> {
    save_to(&paths::claude_settings(), settings)
}

fn save_to(path: &Path, settings: &Settings) -> Result<()> {
    let mut bytes = serde_json::to_vec_pretty(settings)?;
    bytes.push(b'\n');
    write_atomic(path, &bytes, 0o600)
}

/// Save, then record what settings now offer. A history failure is reported
/// rather than fatal: it degrades attribution, not the credential.
pub fn commit(settings: &Settings, event: Option<Event>) -> Result<()> {
    save(settings)?;
    if let Some(event) = event
        && let Err(error) = history::append(&event)
    {
        eprintln!("could not record credential history: {error:#}");
    }
    Ok(())
}

/// A non-empty string from the `env` block.
pub fn env<'a>(settings: &'a Settings, key: &str) -> Option<&'a str> {
    settings
        .get("env")?
        .get(key)?
        .as_str()
        .filter(|value| !value.is_empty())
}

pub fn configured_token(settings: &Settings) -> Option<&str> {
    env(settings, OAUTH_TOKEN)
}

/// The top-level `model` setting, which `claude` starts on without `--model`.
pub fn target_model(settings: &Settings) -> Option<&str> {
    settings
        .get("model")
        .and_then(Value::as_str)
        .filter(|model| !model.is_empty())
}

fn env_mut(settings: &mut Settings) -> &mut Map<String, Value> {
    let slot = settings
        .entry("env")
        .or_insert_with(|| Value::Object(Map::new()));
    if !slot.is_object() {
        *slot = Value::Object(Map::new());
    }
    slot.as_object_mut().expect("env was just made an object")
}

/// Point settings at the account's current credential and return the history
/// event to append once they are saved.
pub fn install(
    settings: &mut Settings,
    account: &Account,
    now_ms: i64,
    kind: EventKind,
) -> Result<Event> {
    let credential = account.credential(now_ms).with_context(|| {
        format!(
            "{} has no usable credential; run `tokeman login {}`",
            account.name, account.name
        )
    })?;
    let env = env_mut(settings);
    env.insert(OAUTH_TOKEN.into(), Value::String(credential.value().into()));
    env.insert(ACCOUNT.into(), Value::String(account.name.clone()));
    env.insert(WAIT_401.into(), Value::String(WAIT_401_MS.into()));
    match credential {
        Credential::Login { scopes, .. } if !scopes.is_empty() => {
            env.insert(OAUTH_SCOPES.into(), Value::String(scopes.join(" ")));
        }
        _ => {
            env.remove(OAUTH_SCOPES);
        }
    }
    Ok(Event::offer(
        now_ms / 1000,
        &account.name,
        credential.value(),
        credential.expires_at().map(|ms| ms / 1000),
        kind,
    ))
}

/// Remove every key tokeman manages; `true` when something was there.
pub fn clear(settings: &mut Settings) -> bool {
    let Some(env) = settings.get_mut("env").and_then(Value::as_object_mut) else {
        return false;
    };
    let mut changed = false;
    for key in MANAGED_KEYS {
        changed |= env.remove(key).is_some();
    }
    if env.is_empty() {
        settings.remove("env");
    }
    changed
}

/// Which credential an account offers now, without secrets.
pub fn describe_credential(account: &Account, now_ms: i64) -> String {
    match account.credential(now_ms) {
        Some(Credential::Login { expires_at, .. }) => match expires_at {
            Some(expires_at) => format!(
                "a full-scope login token (valid {}m)",
                (expires_at - now_ms).max(0) / 60_000
            ),
            None => "a full-scope login token".into(),
        },
        Some(Credential::Setup(_)) => {
            if account
                .login
                .as_ref()
                .is_some_and(|login| login.error.is_some())
            {
                "its inference-only setup token (login refused; run `tokeman login`)".into()
            } else {
                "its inference-only setup token".into()
            }
        }
        None => "no usable credential".into(),
    }
}

/// Why the entry for `account` needs rewriting, if it does.
pub fn reinstall_reason(settings: &Settings, account: &Account, now_ms: i64) -> Option<EventKind> {
    let credential = account.credential(now_ms)?;
    let installed = configured_token(settings);
    if installed != Some(credential.value()) {
        return Some(match credential {
            Credential::Login { .. } if installed == account.setup_key.as_deref() => {
                EventKind::Upgrade
            }
            Credential::Login { .. } => EventKind::Refresh,
            Credential::Setup(_) => EventKind::Fallback,
        });
    }
    let scopes = env(settings, OAUTH_SCOPES);
    let scopes_ok = match credential {
        Credential::Login { scopes: wanted, .. } if !wanted.is_empty() => {
            scopes == Some(wanted.join(" ").as_str())
        }
        _ => scopes.is_none(),
    };
    let marker_ok = env(settings, ACCOUNT) == Some(account.name.as_str());
    let wait_ok = env(settings, WAIT_401) == Some(WAIT_401_MS);
    (!(scopes_ok && marker_ok && wait_ok)).then_some(EventKind::Repair)
}

// The SessionStart hook.

/// The command Claude runs at every session start; `$PPID` is Claude's pid.
pub fn hook_command(binary: &Path) -> String {
    format!(
        "{} rotate observe-session --pid \"$PPID\"",
        shell_quote(&binary.to_string_lossy())
    )
}

pub fn is_observer_hook(command: &str) -> bool {
    command.contains(" rotate observe-session ") || command.ends_with(" rotate observe-session")
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\"'\"'"))
}

/// Replace any earlier observer hook with one for `binary`; other hooks stay.
pub fn install_observer_hook(settings: &mut Settings, binary: &Path) {
    remove_observer_hook(settings);
    let hooks = settings
        .entry("hooks")
        .or_insert_with(|| Value::Object(Map::new()));
    if !hooks.is_object() {
        *hooks = Value::Object(Map::new());
    }
    let groups = hooks
        .as_object_mut()
        .expect("hooks was just made an object")
        .entry("SessionStart")
        .or_insert_with(|| Value::Array(Vec::new()));
    if !groups.is_array() {
        *groups = Value::Array(Vec::new());
    }
    groups
        .as_array_mut()
        .expect("SessionStart was just made an array")
        .push(json!({
            "hooks": [{
                "type": "command",
                "command": hook_command(binary),
                "timeout": 5
            }]
        }));
}

/// Remove tokeman's observer hook; `true` when one was there.
pub fn remove_observer_hook(settings: &mut Settings) -> bool {
    let Some(hooks) = settings.get_mut("hooks").and_then(Value::as_object_mut) else {
        return false;
    };
    let Some(groups) = hooks.get_mut("SessionStart").and_then(Value::as_array_mut) else {
        return false;
    };
    let mut changed = false;
    for group in groups.iter_mut() {
        let Some(commands) = group.get_mut("hooks").and_then(Value::as_array_mut) else {
            continue;
        };
        let before = commands.len();
        commands.retain(|hook| {
            !hook
                .get("command")
                .and_then(Value::as_str)
                .is_some_and(is_observer_hook)
        });
        changed |= commands.len() != before;
    }
    groups.retain(|group| {
        group
            .get("hooks")
            .and_then(Value::as_array)
            .is_none_or(|commands| !commands.is_empty())
    });
    if groups.is_empty() {
        hooks.remove("SessionStart");
    }
    changed
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::account::Login;
    use crate::history::Fingerprint;

    const HOUR_MS: i64 = 3_600_000;

    fn login_account(name: &str) -> Account {
        Account {
            name: name.into(),
            setup_key: Some(format!("sk-ant-oat01-setup-{name}")),
            login: Some(Login {
                access_token: Some(format!("sk-ant-oat01-access-{name}")),
                refresh_token: Some("sk-ant-ort01-refresh".into()),
                expires_at: Some(8 * HOUR_MS),
                obtained_at: Some(0),
                scopes: vec!["user:inference".into(), "user:profile".into()],
                error: None,
            }),
        }
    }

    #[test]
    fn installing_a_login_declares_its_scopes_and_a_401_wait() {
        let account = login_account("a");
        let mut settings = Settings::new();
        let event = install(&mut settings, &account, 0, EventKind::Switch).unwrap();
        let env = settings["env"].as_object().unwrap();
        assert_eq!(env[OAUTH_TOKEN], "sk-ant-oat01-access-a");
        assert_eq!(env[OAUTH_SCOPES], "user:inference user:profile");
        assert_eq!(env[WAIT_401], WAIT_401_MS);
        assert_eq!(env[ACCOUNT], "a");
        assert_eq!(event.account.as_deref(), Some("a"));
        assert_eq!(
            event.fingerprint,
            Some(Fingerprint::of("sk-ant-oat01-access-a"))
        );
        assert_eq!(event.expires_at, Some(8 * 3600));
        assert_eq!(event.kind, EventKind::Switch);
        assert_eq!(reinstall_reason(&settings, &account, 0), None);
    }

    #[test]
    fn falling_back_to_a_setup_token_withdraws_the_scope_claim() {
        let mut account = login_account("a");
        let mut settings = Settings::new();
        install(&mut settings, &account, 0, EventKind::Switch).unwrap();
        account.login.as_mut().unwrap().error = Some("invalid_grant".into());
        assert_eq!(
            reinstall_reason(&settings, &account, 0),
            Some(EventKind::Fallback)
        );
        let event = install(&mut settings, &account, 0, EventKind::Fallback).unwrap();
        let env = settings["env"].as_object().unwrap();
        assert_eq!(env[OAUTH_TOKEN], "sk-ant-oat01-setup-a");
        assert!(!env.contains_key(OAUTH_SCOPES));
        assert_eq!(event.expires_at, None);
        assert_eq!(
            describe_credential(&account, 0),
            "its inference-only setup token (login refused; run `tokeman login`)"
        );
        assert_eq!(
            describe_credential(&login_account("a"), 0),
            "a full-scope login token (valid 480m)"
        );
        assert_eq!(
            describe_credential(&Account::new("empty"), 0),
            "no usable credential"
        );
    }

    #[test]
    fn reinstall_reasons_distinguish_upgrade_refresh_and_repair() {
        let mut account = login_account("a");
        let mut settings = Settings::new();
        // Settings still hold the setup token from before `tokeman login`.
        let setup_only = Account {
            login: None,
            ..account.clone()
        };
        install(&mut settings, &setup_only, 0, EventKind::Switch).unwrap();
        assert_eq!(
            reinstall_reason(&settings, &account, 0),
            Some(EventKind::Upgrade)
        );

        install(&mut settings, &account, 0, EventKind::Upgrade).unwrap();
        account.login.as_mut().unwrap().access_token = Some("sk-ant-oat01-access-a-2".into());
        assert_eq!(
            reinstall_reason(&settings, &account, 0),
            Some(EventKind::Refresh)
        );

        install(&mut settings, &account, 0, EventKind::Refresh).unwrap();
        settings
            .get_mut("env")
            .and_then(Value::as_object_mut)
            .unwrap()
            .remove(WAIT_401);
        assert_eq!(
            reinstall_reason(&settings, &account, 0),
            Some(EventKind::Repair)
        );
    }

    #[test]
    fn clearing_removes_every_managed_key() {
        let mut settings = Settings::new();
        install(&mut settings, &login_account("a"), 0, EventKind::Switch).unwrap();
        settings
            .get_mut("env")
            .and_then(Value::as_object_mut)
            .unwrap()
            .insert("UNRELATED".into(), Value::String("kept".into()));
        assert!(clear(&mut settings));
        let env = settings["env"].as_object().unwrap();
        assert_eq!(env.len(), 1);
        assert_eq!(env["UNRELATED"], "kept");
        assert!(!clear(&mut settings));
        let mut only_ours = Settings::new();
        install(&mut only_ours, &login_account("a"), 0, EventKind::Switch).unwrap();
        assert!(clear(&mut only_ours));
        assert!(!only_ours.contains_key("env"));
    }

    #[test]
    fn observer_hook_detection_does_not_match_unrelated_hooks() {
        assert!(is_observer_hook(
            "'/tmp/tokeman' rotate observe-session --pid \"$PPID\""
        ));
        assert!(!is_observer_hook("bash ~/.claude/hooks/postmark-nudge.sh"));
        assert_eq!(
            hook_command(Path::new("/opt/to'ken/tokeman")),
            "'/opt/to'\"'\"'ken/tokeman' rotate observe-session --pid \"$PPID\""
        );
    }

    #[test]
    fn observer_hook_install_is_idempotent_and_preserves_other_hooks() {
        let mut settings = json!({
            "hooks": {
                "SessionStart": [{
                    "hooks": [{
                        "type": "command",
                        "command": "bash ~/.claude/hooks/postmark-nudge.sh"
                    }]
                }]
            }
        })
        .as_object()
        .unwrap()
        .clone();
        let binary = Path::new("/tmp/tokeman");

        install_observer_hook(&mut settings, binary);
        install_observer_hook(&mut settings, binary);

        let commands = |settings: &Settings| -> Vec<String> {
            Value::Object(settings.clone())
                .pointer("/hooks/SessionStart")
                .and_then(Value::as_array)
                .unwrap()
                .iter()
                .flat_map(|group| {
                    group
                        .get("hooks")
                        .and_then(Value::as_array)
                        .into_iter()
                        .flatten()
                        .filter_map(|hook| hook.get("command").and_then(Value::as_str))
                        .map(str::to_owned)
                        .collect::<Vec<_>>()
                })
                .collect()
        };
        let installed = commands(&settings);
        assert_eq!(
            installed
                .iter()
                .filter(|command| is_observer_hook(command))
                .count(),
            1
        );
        assert!(
            installed
                .iter()
                .any(|command| command.contains("postmark-nudge"))
        );

        assert!(remove_observer_hook(&mut settings));
        assert_eq!(commands(&settings).len(), 1);
        assert!(!remove_observer_hook(&mut settings));
    }

    #[test]
    fn settings_round_trip_as_an_owner_only_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        assert!(load_from(&path).unwrap().is_empty());
        let mut settings = Settings::new();
        settings.insert("model".into(), Value::String("opus".into()));
        install(&mut settings, &login_account("a"), 0, EventKind::Switch).unwrap();
        save_to(&path, &settings).unwrap();
        let loaded = load_from(&path).unwrap();
        assert_eq!(configured_token(&loaded), Some("sk-ant-oat01-access-a"));
        assert_eq!(target_model(&loaded), Some("opus"));
        std::fs::write(&path, "[]").unwrap();
        assert!(load_from(&path).is_err());
    }
}
