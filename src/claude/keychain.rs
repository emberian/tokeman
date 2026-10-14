//! The /login record in Claude's Keychain item (`Claude Code-credentials`).
//! Claude prefers a Keychain login over the settings env token, so rotation
//! sets the record aside while it manages the credential and puts it back on
//! pause or uninstall. The rest of the item (MCP OAuth and anything newer)
//! is never touched.
//!
//! The backup on disk is written before the Keychain changes: `captured`
//! means the write may not have finished and the next check retries;
//! `suppressed` means the Keychain holds no login and the backup is the copy.

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::fs::{read_if_present, write_atomic};
use crate::paths;

const VERSION: u32 = 1;
const LOGIN_KEY: &str = "claudeAiOauth";
#[cfg(target_os = "macos")]
const SERVICE: &str = "Claude Code-credentials";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum BackupState {
    /// Saved to disk; the Keychain write that follows may not have completed.
    Captured,
    /// The Keychain no longer holds the login; this file is the copy.
    Suppressed,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LoginBackup {
    version: u32,
    pub state: BackupState,
    #[serde(rename = "claudeAiOauth")]
    pub login: Option<Value>,
}

impl LoginBackup {
    fn new(state: BackupState, login: Option<Value>) -> Self {
        Self {
            version: VERSION,
            state,
            login,
        }
    }
}

/// Take the login out of the Keychain document; `None` when it had none.
pub fn split_login(document: &mut Value) -> Result<Option<Value>> {
    Ok(object_of(document)?.remove(LOGIN_KEY))
}

/// Put a saved login back, unless a newer manual /login is already there.
pub fn restore_login(document: &mut Value, login: Value) -> Result<bool> {
    let object = object_of(document)?;
    if object.contains_key(LOGIN_KEY) {
        return Ok(false);
    }
    object.insert(LOGIN_KEY.into(), login);
    Ok(true)
}

fn object_of(document: &mut Value) -> Result<&mut serde_json::Map<String, Value>> {
    document
        .as_object_mut()
        .context("Claude Keychain credential must contain a JSON object")
}

pub fn load_backup() -> Result<Option<LoginBackup>> {
    let path = paths::login_backup();
    let Some(contents) = read_if_present(&path)? else {
        return Ok(None);
    };
    let backup: LoginBackup = serde_json::from_str(&contents)
        .with_context(|| format!("failed to parse login backup {}", path.display()))?;
    if backup.version != VERSION {
        bail!(
            "unsupported Claude login backup version {} in {}",
            backup.version,
            path.display()
        );
    }
    Ok(Some(backup))
}

pub fn save_backup(backup: &LoginBackup) -> Result<()> {
    let mut bytes = serde_json::to_vec_pretty(backup)?;
    bytes.push(b'\n');
    write_atomic(&paths::login_backup(), &bytes, 0o600)
}

/// The backup to leave when the Keychain holds no login: an earlier capture
/// is what pause restores, so finding nothing now keeps it.
pub fn settled(existing: Option<LoginBackup>) -> LoginBackup {
    LoginBackup::new(
        BackupState::Suppressed,
        existing.and_then(|backup| backup.login),
    )
}

/// Move the /login record from the Keychain to the backup. `force_check`
/// looks even when the backup already says suppressed. Returns whether the
/// Keychain changed.
#[cfg(target_os = "macos")]
pub fn suppress(force_check: bool) -> Result<bool> {
    let existing = load_backup()?;
    if !force_check
        && existing
            .as_ref()
            .is_some_and(|backup| backup.state == BackupState::Suppressed)
    {
        return Ok(false);
    }
    let Some(mut document) = read_document()? else {
        save_backup(&settled(existing))?;
        return Ok(false);
    };
    let Some(login) = split_login(&mut document)? else {
        save_backup(&settled(existing))?;
        return Ok(false);
    };
    let mut backup = LoginBackup::new(BackupState::Captured, Some(login));
    save_backup(&backup)?;
    write_document(&document)?;
    backup.state = BackupState::Suppressed;
    save_backup(&backup)?;
    Ok(true)
}

#[cfg(not(target_os = "macos"))]
pub fn suppress(_force_check: bool) -> Result<bool> {
    Ok(false)
}

/// Put the backed-up login back and remove the backup. Returns whether the
/// Keychain changed; a newer manual /login is left alone.
#[cfg(target_os = "macos")]
pub fn restore() -> Result<bool> {
    let Some(backup) = load_backup()? else {
        return Ok(false);
    };
    let changed = match backup.login {
        Some(login) => {
            let mut document = read_document()?.unwrap_or_else(|| serde_json::json!({}));
            if restore_login(&mut document, login)? {
                write_document(&document)?;
                true
            } else {
                false
            }
        }
        None => false,
    };
    remove_backup()?;
    Ok(changed)
}

#[cfg(not(target_os = "macos"))]
pub fn restore() -> Result<bool> {
    Ok(false)
}

#[cfg(target_os = "macos")]
fn remove_backup() -> Result<()> {
    let path = paths::login_backup();
    match std::fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => {
            Err(error).with_context(|| format!("failed to remove login backup {}", path.display()))
        }
    }
}

#[cfg(target_os = "macos")]
fn keychain_account() -> Result<String> {
    std::env::var("USER")
        .ok()
        .filter(|value| !value.is_empty())
        .or_else(|| {
            dirs::home_dir()?
                .file_name()
                .map(|value| value.to_string_lossy().into_owned())
        })
        .context("could not determine macOS Keychain account")
}

#[cfg(target_os = "macos")]
fn read_document() -> Result<Option<Value>> {
    use security_framework::passwords::get_generic_password;
    const ERR_SEC_ITEM_NOT_FOUND: i32 = -25_300;

    let account = keychain_account()?;
    let bytes = match get_generic_password(SERVICE, &account) {
        Ok(bytes) => bytes,
        Err(error) if error.code() == ERR_SEC_ITEM_NOT_FOUND => return Ok(None),
        Err(error) => return Err(error).context("could not read Claude's Keychain credential"),
    };
    let value =
        serde_json::from_slice(&bytes).context("Claude Keychain credential is not valid JSON")?;
    Ok(Some(value))
}

#[cfg(target_os = "macos")]
fn write_document(document: &Value) -> Result<()> {
    use security_framework::passwords::set_generic_password;

    let account = keychain_account()?;
    set_generic_password(SERVICE, &account, &serde_json::to_vec(document)?)
        .context("could not update Claude's Keychain credential")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn keychain_suppression_preserves_mcp_and_unknown_fields() {
        let login = json!({
            "accessToken": "short-lived",
            "refreshToken": "refresh"
        });
        let mut document = json!({
            "mcpOAuth": {"linear": {"accessToken": "mcp-secret"}},
            "claudeAiOauth": login.clone(),
            "futureField": {"value": 7}
        });

        assert_eq!(split_login(&mut document).unwrap(), Some(login));
        assert!(document.get("claudeAiOauth").is_none());
        assert_eq!(
            document.pointer("/mcpOAuth/linear/accessToken"),
            Some(&json!("mcp-secret"))
        );
        assert_eq!(document.pointer("/futureField/value"), Some(&json!(7)));
        assert_eq!(split_login(&mut document).unwrap(), None);
    }

    #[test]
    fn keychain_restore_never_overwrites_a_newer_manual_login() {
        let mut without_login = json!({"mcpOAuth": {"linear": {}}});
        assert!(restore_login(&mut without_login, json!({"accessToken": "saved"})).unwrap());
        assert_eq!(
            without_login.pointer("/claudeAiOauth/accessToken"),
            Some(&json!("saved"))
        );
        assert!(!restore_login(&mut without_login, json!({"accessToken": "older"})).unwrap());
        assert_eq!(
            without_login.pointer("/claudeAiOauth/accessToken"),
            Some(&json!("saved"))
        );
    }

    #[test]
    fn keychain_transform_rejects_non_object_documents() {
        let mut document = json!(["not", "an", "object"]);
        assert!(split_login(&mut document).is_err());
        assert!(restore_login(&mut document, json!({})).is_err());
    }

    #[test]
    fn an_empty_keychain_does_not_erase_the_captured_login_backup() {
        // Suppressed and backed up earlier; a later forced check finds the
        // Keychain without a login. The record pause restores must survive.
        let saved = json!({"refreshToken": "saved"});
        let suppressed = LoginBackup::new(BackupState::Suppressed, Some(saved.clone()));
        let settled_backup = settled(Some(suppressed));
        assert_eq!(settled_backup.state, BackupState::Suppressed);
        assert_eq!(settled_backup.login, Some(saved.clone()));

        // A capture whose Keychain write was interrupted settles the same way.
        let interrupted = LoginBackup::new(BackupState::Captured, Some(saved));
        assert_eq!(settled(Some(interrupted)).state, BackupState::Suppressed);

        assert_eq!(settled(None).login, None);
    }

    #[test]
    fn the_backup_keeps_its_stored_spelling() {
        let json = r#"{"version":1,"state":"captured","claudeAiOauth":{"accessToken":"x"}}"#;
        let backup: LoginBackup = serde_json::from_str(json).unwrap();
        assert_eq!(backup.state, BackupState::Captured);
        assert_eq!(serde_json::to_string(&backup).unwrap(), json);
        assert!(serde_json::from_str::<LoginBackup>(r#"{"version":1,"state":"lost"}"#).is_err());
    }
}
