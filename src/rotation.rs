use std::cmp::Ordering;
use std::collections::BTreeMap;
use std::fs::OpenOptions;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

#[cfg(target_os = "macos")]
use anyhow::anyhow;
use anyhow::{Context, Result, bail};
use chrono::{Local, NaiveDateTime, TimeZone, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use crate::admission;
use crate::config::{Config, CredentialKind, RotationSettings, Token};
use crate::credential_history::{self, CredentialEvent};
use crate::private_fs::{FileLock, ensure_private_parent, set_mode, state_dir, write_atomic};
use crate::probe::{self, ModelQuotaBucket, ModelUsage, ModelUsageSource, ProbeResult};
use crate::store::Store;

const SERVICE_LABEL: &str = "com.ember.tokeman-claude-rotate";
const OAUTH_SETTING: &str = "CLAUDE_CODE_OAUTH_TOKEN";
/// Claude assumes an env token is inference-only unless told otherwise, and
/// gates /feedback, Remote Control and profile reads on that assumption.
const SCOPES_SETTING: &str = "CLAUDE_CODE_OAUTH_SCOPES";
/// How long a refused Claude request waits for `settings.json` to offer a
/// replacement token. Claude's local default is zero, which turns an expired
/// access token into a failed request instead of a pause; two minutes covers
/// the daemon's 20s wake plus a slow refresh.
const WAIT_SETTING: &str = "CLAUDE_CODE_OAUTH_401_WAIT_MS";
const WAIT_MS: &str = "120000";
#[cfg(target_os = "macos")]
const CLAUDE_KEYCHAIN_SERVICE: &str = "Claude Code-credentials";
const LOGIN_BACKUP_VERSION: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize)]
struct LoginKeychainBackup {
    version: u32,
    /// `captured` means the backup is durable but the Keychain update did not
    /// finish. Only `suppressed` is safe to skip on a later daemon start.
    state: String,
    #[serde(rename = "claudeAiOauth")]
    claude_ai_oauth: Option<Value>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum RotationMode {
    Normal,
    SipAndDrain,
}

impl RotationMode {
    pub fn label(self) -> &'static str {
        match self {
            Self::Normal => "normal",
            Self::SipAndDrain => "sip-and-drain",
        }
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub struct RotateOptions {
    pub force: bool,
    pub dry_run: bool,
    pub scheduled: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct RotationOutcome {
    pub action: String,
    pub mode: RotationMode,
    pub active_before: Option<String>,
    pub active_after: Option<String>,
    pub changed: bool,
    pub probed: bool,
    pub message: String,
}

impl RotationOutcome {
    /// An outcome that left the default where it was.
    fn held(
        action: &str,
        mode: RotationMode,
        active: Option<String>,
        probed: bool,
        message: impl Into<String>,
    ) -> Self {
        Self {
            action: action.into(),
            mode,
            active_before: active.clone(),
            active_after: active,
            changed: false,
            probed,
            message: message.into(),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct RotationTokenStatus {
    pub name: String,
    pub remaining_5h: Option<f64>,
    pub remaining_7d: Option<f64>,
    pub remaining_opus_7d: Option<f64>,
    pub remaining_sonnet_7d: Option<f64>,
    pub opus_source: Option<ModelUsageSource>,
    pub sonnet_source: Option<ModelUsageSource>,
    pub model_buckets: Vec<RotationModelBucketStatus>,
    pub admission_limits: Vec<admission::ObservedLimit>,
    pub viable: bool,
    pub is_default: bool,
    pub error: Option<String>,
    /// Which credential this account offers Claude, without secrets.
    pub credential: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct RotationModelBucketStatus {
    pub key: String,
    pub label: String,
    pub remaining: f64,
    pub reset: i64,
    pub source: ModelUsageSource,
    pub relevant: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct ClaudeSessionStatus {
    pub pid: u32,
    pub tty: Option<String>,
    pub started_at: chrono::DateTime<Utc>,
    pub session_id: Option<String>,
    /// Exact for an inherited process credential or SessionStart launch
    /// binding, otherwise an estimate from default history.
    pub account: Option<String>,
    pub credential_source: String,
}

/// An account that is no longer the default, still has live sessions bound to
/// it, and has fallen below a rotation floor.
///
/// Rotating the default protects only *new* processes. Nothing previously
/// noticed when the account tokeman had just stepped away from kept being
/// drained past its floor by the sessions left on it, so the fleet could
/// exhaust an account the policy believed it had already rescued.
#[derive(Debug, Clone, Serialize)]
pub struct AbandonedDrain {
    pub account: String,
    pub bound_sessions: usize,
    pub remaining_5h: Option<f64>,
    pub remaining_7d: Option<f64>,
}

impl AbandonedDrain {
    pub fn summary(&self) -> String {
        let pct = |value: Option<f64>| match value {
            Some(value) => format!("{:.0}%", value * 100.0),
            None => "n/a".to_string(),
        };
        format!(
            "{} is below floor (5h {} left, 7d {} left) with {} live session(s) still bound to it",
            self.account,
            pct(self.remaining_5h),
            pct(self.remaining_7d),
            self.bound_sessions
        )
    }
}

/// Accounts being drained past their floor by sessions the daemon can no
/// longer move. Restarting or resuming those sessions is the only way to
/// release them, so the point of this is to say so out loud.
pub fn abandoned_drains(
    results: &[ProbeResult],
    default: Option<&str>,
    bound: &BTreeMap<String, usize>,
    mode: RotationMode,
    policy: &RotationSettings,
    target_model: Option<&str>,
) -> Vec<AbandonedDrain> {
    let mut drains: Vec<_> = results
        .iter()
        .filter(|result| Some(result.token_name.as_str()) != default)
        .filter(|result| has_quota_reading(result))
        .filter(|result| !is_viable(result, mode, policy, target_model))
        .filter_map(|result| {
            let sessions = bound.get(&result.token_name).copied().unwrap_or(0);
            (sessions > 0).then(|| {
                let (remaining_5h, remaining_7d) = remaining(result);
                AbandonedDrain {
                    account: result.token_name.clone(),
                    bound_sessions: sessions,
                    remaining_5h,
                    remaining_7d,
                }
            })
        })
        .collect();
    drains.sort_by(|a, b| a.account.cmp(&b.account));
    drains
}

#[derive(Debug, Clone, Serialize)]
pub struct RotationStatus {
    pub monitor: String,
    pub service_installed: bool,
    pub mode: RotationMode,
    pub probe_interval_secs: u64,
    pub default_token: Option<String>,
    pub default_is_managed: bool,
    pub target_model: Option<String>,
    pub min_5h_remaining: f64,
    pub min_7d_remaining: f64,
    pub premium_admission: String,
    /// Whether and how header-only limits are being sampled.
    pub header_sampling: String,
    pub live_sessions: Vec<ClaudeSessionStatus>,
    pub abandoned_drains: Vec<AbandonedDrain>,
    pub tokens: Vec<RotationTokenStatus>,
}

/// Recheck schedule for an account the API is actively refusing.
///
/// A 401 or 403 is a decision, not a hiccup: a revoked token stays revoked
/// until someone logs in again. Measured over seven days, seven accounts failed
/// 24,390 of 24,390 probes -- 54% of all probe traffic spent re-asking a
/// question already answered. Transient failures (timeouts, connection resets)
/// are deliberately excluded: those we do want to retry immediately.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct AuthBackoff {
    failures: u32,
    next_attempt_epoch: f64,
    last_error: String,
}

/// 5m, 10m, 20m, then every 30m. Long enough to stop the bleeding, short
/// enough that a fresh login is picked up within one sip-and-drain window.
fn auth_backoff_secs(failures: u32) -> f64 {
    const CAP: f64 = 1800.0;
    let step = 300.0 * 2f64.powi(failures.saturating_sub(1).min(8) as i32);
    step.min(CAP)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct CadenceState {
    #[serde(default)]
    last_probe_epoch: f64,
    /// Accounts already reported as drained-while-abandoned, so a standing
    /// condition is logged on the transition instead of every probe cycle.
    #[serde(default)]
    drained_accounts: Vec<String>,
    /// Per-account recheck schedule for hard authentication failures.
    #[serde(default)]
    auth_backoff: BTreeMap<String, AuthBackoff>,
    #[serde(default)]
    mode: Option<RotationMode>,
    /// The next scheduled interval. This can be faster than the mode default
    /// while quota coverage is degraded or the default token is near a floor.
    #[serde(default)]
    probe_interval_secs: Option<u64>,
    #[serde(default)]
    consecutive_degraded_probes: u32,
    /// Latest target-model sample per account (see `target_model_sample_secs`).
    #[serde(default)]
    header_samples: BTreeMap<String, HeaderSample>,
}

/// What an occasional target-model probe saw of the limits only that model's
/// responses carry.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct HeaderSample {
    sampled_at: f64,
    /// Whether the sample got a quota answer at all.
    #[serde(default)]
    answered: bool,
    /// The Fable (`7d_oi`) window; `None` when the response carried none.
    window: Option<crate::probe::Window>,
}

impl Default for CadenceState {
    fn default() -> Self {
        Self {
            last_probe_epoch: 0.0,
            drained_accounts: Vec::new(),
            auth_backoff: BTreeMap::new(),
            mode: Some(RotationMode::Normal),
            probe_interval_secs: None,
            consecutive_degraded_probes: 0,
            header_samples: BTreeMap::new(),
        }
    }
}

/// Serializes every change to Claude's settings and to rotation state.
struct RotationLock;

impl RotationLock {
    fn acquire() -> Result<FileLock> {
        FileLock::acquire(&lock_path()?)
    }
}

fn lock_path() -> Result<PathBuf> {
    Ok(state_dir()?.join("claude-rotate.lock"))
}

fn pause_path() -> Result<PathBuf> {
    Ok(state_dir()?.join("claude-rotate.paused"))
}

fn cadence_path() -> Result<PathBuf> {
    Ok(state_dir()?.join("claude-rotate-state.json"))
}

fn log_path() -> Result<PathBuf> {
    Ok(state_dir()?.join("claude-rotate.log"))
}

fn login_backup_path() -> Result<PathBuf> {
    Ok(state_dir()?.join("claude-login-keychain-backup.json"))
}

fn launchd_log_path() -> Result<PathBuf> {
    Ok(state_dir()?.join("claude-rotate-launchd.log"))
}

fn launch_agent_path() -> Result<PathBuf> {
    Ok(dirs::home_dir()
        .context("could not find home directory")?
        .join("Library/LaunchAgents")
        .join(format!("{SERVICE_LABEL}.plist")))
}

fn daemon_launcher_path() -> Result<PathBuf> {
    Ok(state_dir()?.join("launch-rotate-daemon.sh"))
}

fn claude_settings_path() -> Result<PathBuf> {
    if let Some(path) = std::env::var_os("CLAUDE_SETTINGS") {
        return Ok(PathBuf::from(path));
    }
    Ok(dirs::home_dir()
        .context("could not find home directory")?
        .join(".claude/settings.json"))
}

fn split_claude_login(document: &mut Value) -> Result<Option<Value>> {
    let object = document
        .as_object_mut()
        .context("Claude Keychain credential must contain a JSON object")?;
    Ok(object.remove("claudeAiOauth"))
}

fn restore_claude_login(document: &mut Value, login: Value) -> Result<bool> {
    let object = document
        .as_object_mut()
        .context("Claude Keychain credential must contain a JSON object")?;
    if object.contains_key("claudeAiOauth") {
        return Ok(false);
    }
    object.insert("claudeAiOauth".into(), login);
    Ok(true)
}

fn save_login_backup(backup: &LoginKeychainBackup) -> Result<()> {
    let mut bytes = serde_json::to_vec_pretty(backup)?;
    bytes.push(b'\n');
    write_atomic(&login_backup_path()?, &bytes, 0o600)
}

fn load_login_backup() -> Result<Option<LoginKeychainBackup>> {
    let path = login_backup_path()?;
    if !path.exists() {
        return Ok(None);
    }
    let backup: LoginKeychainBackup = serde_json::from_slice(
        &std::fs::read(&path)
            .with_context(|| format!("failed to read login backup {}", path.display()))?,
    )
    .with_context(|| format!("failed to parse login backup {}", path.display()))?;
    if backup.version != LOGIN_BACKUP_VERSION {
        bail!(
            "unsupported Claude login backup version {} in {}",
            backup.version,
            path.display()
        );
    }
    Ok(Some(backup))
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
fn read_claude_keychain_document() -> Result<Option<Value>> {
    use security_framework::passwords::get_generic_password;

    let account = keychain_account()?;
    let bytes = match get_generic_password(CLAUDE_KEYCHAIN_SERVICE, &account) {
        Ok(bytes) => bytes,
        // errSecItemNotFound
        Err(error) if error.code() == -25_300 => return Ok(None),
        Err(error) => return Err(error).context("could not read Claude's Keychain credential"),
    };
    let value =
        serde_json::from_slice(&bytes).context("Claude Keychain credential is not valid JSON")?;
    Ok(Some(value))
}

#[cfg(target_os = "macos")]
fn write_claude_keychain_document(document: &Value) -> Result<()> {
    use security_framework::passwords::set_generic_password;

    let account = keychain_account()?;
    let bytes = serde_json::to_vec(document)?;
    set_generic_password(CLAUDE_KEYCHAIN_SERVICE, &account, &bytes)
        .context("could not update Claude's Keychain credential")
}

#[cfg(target_os = "macos")]
fn suppress_login_shadow(force_check: bool) -> Result<bool> {
    if !force_check && load_login_backup()?.is_some_and(|backup| backup.state == "suppressed") {
        return Ok(false);
    }

    let Some(mut document) = read_claude_keychain_document()? else {
        save_login_backup(&LoginKeychainBackup {
            version: LOGIN_BACKUP_VERSION,
            state: "suppressed".into(),
            claude_ai_oauth: None,
        })?;
        return Ok(false);
    };
    let login = split_claude_login(&mut document)?;
    let mut backup = LoginKeychainBackup {
        version: LOGIN_BACKUP_VERSION,
        state: "captured".into(),
        claude_ai_oauth: login,
    };
    // Capture before mutation. If the Keychain write is interrupted, the next
    // call sees `captured` and retries rather than falsely assuming success.
    save_login_backup(&backup)?;
    if backup.claude_ai_oauth.is_some() {
        write_claude_keychain_document(&document)?;
    }
    backup.state = "suppressed".into();
    save_login_backup(&backup)?;
    Ok(backup.claude_ai_oauth.is_some())
}

#[cfg(not(target_os = "macos"))]
fn suppress_login_shadow(_force_check: bool) -> Result<bool> {
    Ok(false)
}

#[cfg(target_os = "macos")]
fn restore_login_shadow() -> Result<bool> {
    let Some(backup) = load_login_backup()? else {
        return Ok(false);
    };
    let changed = if let Some(login) = backup.claude_ai_oauth {
        let mut document = read_claude_keychain_document()?.unwrap_or_else(|| json!({}));
        if restore_claude_login(&mut document, login)? {
            write_claude_keychain_document(&document)?;
            true
        } else {
            // A newer manual /login wins over the older saved record.
            false
        }
    } else {
        false
    };
    let path = login_backup_path()?;
    if path.exists() {
        std::fs::remove_file(&path).with_context(|| {
            format!("failed to remove restored login backup {}", path.display())
        })?;
    }
    Ok(changed)
}

#[cfg(not(target_os = "macos"))]
fn restore_login_shadow() -> Result<bool> {
    Ok(false)
}

/// Reconcile after the user deliberately ran `/login` to enroll/capture an
/// account. Managed fleet auth remains authoritative; MCP OAuth data survives.
pub fn reconcile_keychain_after_login() -> Result<()> {
    let _lock = RotationLock::acquire()?;
    if pause_path()?.exists() {
        return Ok(());
    }
    if suppress_login_shadow(true)? {
        append_log("suppressed short-lived /login Keychain auth after account enrollment");
    }
    Ok(())
}

/// Past this size the log is moved aside to `.old`, replacing any previous
/// one, so it cannot grow without bound.
const LOG_ROTATE_BYTES: u64 = 4 * 1024 * 1024;

fn previous_log_path(path: &Path) -> PathBuf {
    path.with_extension("log.old")
}

fn append_log(message: &str) {
    // Unit tests exercise code paths that log; they must never write into the
    // user's real rotation log (which `default_events` also parses).
    if cfg!(test) {
        return;
    }
    let Ok(path) = log_path() else {
        return;
    };
    if std::fs::metadata(&path).is_ok_and(|metadata| metadata.len() > LOG_ROTATE_BYTES) {
        let _ = std::fs::rename(&path, previous_log_path(&path));
    }
    let stamp = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ");
    let _ = crate::private_fs::append_line(&path, &format!("{stamp} {message}"));
}

fn now_epoch() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs_f64()
}

#[cfg(unix)]
pub(crate) fn executable_identity() -> Option<(u64, u64)> {
    use std::os::unix::fs::MetadataExt;
    let executable = std::env::current_exe().ok()?.canonicalize().ok()?;
    let metadata = std::fs::metadata(executable).ok()?;
    Some((metadata.dev(), metadata.ino()))
}

#[cfg(not(unix))]
pub(crate) fn executable_identity() -> Option<(u64, u64)> {
    None
}

fn load_cadence() -> CadenceState {
    cadence_path()
        .ok()
        .and_then(|path| std::fs::read(path).ok())
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default()
}

fn save_cadence(state: &CadenceState) -> Result<()> {
    let mut bytes = serde_json::to_vec_pretty(state)?;
    bytes.push(b'\n');
    write_atomic(&cadence_path()?, &bytes, 0o600)
}

fn load_claude_settings() -> Result<Map<String, Value>> {
    let path = claude_settings_path()?;
    if !path.exists() {
        return Ok(Map::new());
    }
    let value: Value = serde_json::from_slice(
        &std::fs::read(&path)
            .with_context(|| format!("failed to read Claude settings {}", path.display()))?,
    )
    .with_context(|| format!("failed to parse Claude settings {}", path.display()))?;
    value
        .as_object()
        .cloned()
        .context("Claude settings must contain a JSON object")
}

fn save_claude_settings(settings: &Map<String, Value>) -> Result<()> {
    let path = claude_settings_path()?;
    let mut bytes = serde_json::to_vec_pretty(settings)?;
    bytes.push(b'\n');
    let existing_mode = file_mode(&path).unwrap_or(0o600);
    write_atomic(&path, &bytes, existing_mode)
}

#[cfg(unix)]
fn file_mode(path: &Path) -> Option<u32> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path)
        .ok()
        .map(|metadata| metadata.permissions().mode() & 0o777)
}

#[cfg(not(unix))]
fn file_mode(_path: &Path) -> Option<u32> {
    None
}

/// A non-empty string from Claude settings' `env` block.
fn configured_env<'a>(settings: &'a Map<String, Value>, key: &str) -> Option<&'a str> {
    settings
        .get("env")?
        .get(key)?
        .as_str()
        .filter(|value| !value.is_empty())
}

fn configured_token(settings: &Map<String, Value>) -> Option<&str> {
    configured_env(settings, OAUTH_SETTING)
}

fn configured_target_model(settings: &Map<String, Value>) -> Option<&str> {
    settings
        .get("model")
        .and_then(Value::as_str)
        .filter(|model| !model.is_empty())
}

/// Point `settings.json` at an account's current credential.
///
/// Returns the history event to record once the settings are saved. Claude
/// copies settings `env` into its process environment on every change, so
/// running sessions see this immediately, but each keeps using its cached
/// token until the API refuses it; the event is what lets tokeman reconstruct
/// which account every process is actually spending.
fn install_credential(
    settings: &mut Map<String, Value>,
    token: &Token,
    kind: &str,
) -> Result<CredentialEvent> {
    let now_ms = Utc::now().timestamp_millis();
    let credential = token.credential(now_ms).with_context(|| {
        format!(
            "{} has no usable credential; run `tokeman login {}`",
            token.name, token.name
        )
    })?;
    if !settings.get("env").is_some_and(Value::is_object) {
        settings.insert("env".into(), Value::Object(Map::new()));
    }
    let env = settings
        .get_mut("env")
        .and_then(Value::as_object_mut)
        .expect("env was just initialized");
    env.insert(OAUTH_SETTING.into(), Value::String(credential.value.into()));
    env.insert(
        admission::ACCOUNT_ENV.into(),
        Value::String(token.name.clone()),
    );
    env.insert(WAIT_SETTING.into(), Value::String(WAIT_MS.into()));
    match credential.kind {
        CredentialKind::Login if !credential.scopes.is_empty() => {
            env.insert(
                SCOPES_SETTING.into(),
                Value::String(credential.scopes.join(" ")),
            );
        }
        _ => {
            env.remove(SCOPES_SETTING);
        }
    }
    Ok(CredentialEvent {
        at: now_ms / 1000,
        account: Some(token.name.clone()),
        fingerprint: Some(credential_history::fingerprint(credential.value)),
        expires_at: credential.expires_at.map(|ms| ms / 1000),
        kind: kind.into(),
    })
}

/// Save settings, then record what they now offer. History is best-effort:
/// failing to write it degrades attribution, not the credential itself.
fn commit_settings(settings: &Map<String, Value>, event: Option<CredentialEvent>) -> Result<()> {
    save_claude_settings(settings)?;
    if let Some(event) = event
        && let Err(error) = credential_history::append(&event)
    {
        append_log(&format!(
            "ERROR could not record credential history: {error:#}"
        ));
    }
    Ok(())
}

fn clear_configured_token(settings: &mut Map<String, Value>) -> bool {
    let Some(env) = settings.get_mut("env").and_then(Value::as_object_mut) else {
        return false;
    };
    let mut changed = false;
    for key in [
        OAUTH_SETTING,
        admission::ACCOUNT_ENV,
        SCOPES_SETTING,
        WAIT_SETTING,
    ] {
        changed |= env.remove(key).is_some();
    }
    if env.is_empty() {
        settings.remove("env");
    }
    changed
}

/// Why the settings entry for the default account needs rewriting, if it does.
fn reinstall_reason(settings: &Map<String, Value>, token: &Token) -> Option<&'static str> {
    let credential = token.credential(Utc::now().timestamp_millis())?;
    let installed = configured_token(settings);
    if installed != Some(credential.value) {
        return Some(match credential.kind {
            CredentialKind::Login if installed == token.setup_key() => "upgrade",
            CredentialKind::Login => "refresh",
            CredentialKind::Setup => "fallback",
        });
    }
    let scopes = configured_env(settings, SCOPES_SETTING);
    let scopes_ok = match credential.kind {
        CredentialKind::Login if !credential.scopes.is_empty() => {
            scopes == Some(credential.scopes.join(" ").as_str())
        }
        _ => scopes.is_none(),
    };
    let marker_ok = configured_env(settings, admission::ACCOUNT_ENV) == Some(token.name.as_str());
    let wait_ok = configured_env(settings, WAIT_SETTING) == Some(WAIT_MS);
    (!(scopes_ok && marker_ok && wait_ok)).then_some("repair")
}

/// Which credential an account currently offers, without secrets.
fn describe_credential(token: &Token) -> String {
    let now_ms = Utc::now().timestamp_millis();
    match token.credential(now_ms) {
        Some(credential) if credential.kind == CredentialKind::Login => match credential.expires_at
        {
            Some(expires_at) => format!(
                "a full-scope login token (valid {}m)",
                (expires_at - now_ms).max(0) / 60_000
            ),
            None => "a full-scope login token".into(),
        },
        Some(_) if token.login_error.is_some() => {
            "its inference-only setup token (login refused; run `tokeman login`)".into()
        }
        Some(_) => "its inference-only setup token".into(),
        None => "no usable credential".into(),
    }
}

fn token_name_for_value<'a>(tokens: &'a [Token], value: Option<&str>) -> Option<&'a str> {
    let value = value?;
    if let Some(token) = tokens.iter().find(|token| token.holds(value)) {
        return Some(token.name.as_str());
    }
    // An access token tokeman installed and has since refreshed past.
    let account = credential_history::account_for_fingerprint(
        &credential_history::load(),
        &credential_history::fingerprint(value),
    )?;
    tokens
        .iter()
        .find(|token| token.name == account)
        .map(|token| token.name.as_str())
}

fn utilization(result: &ProbeResult) -> Option<(f64, f64)> {
    let quota = result.quota.as_ref()?;
    let session = quota.session.as_ref()?.utilization;
    let weekly = quota.weekly.as_ref()?.utilization;
    Some((session, weekly))
}

pub fn remaining(result: &ProbeResult) -> (Option<f64>, Option<f64>) {
    utilization(result)
        .map(|(five, seven)| (Some(1.0 - five), Some(1.0 - seven)))
        .unwrap_or((None, None))
}

fn status_allows(result: &ProbeResult) -> bool {
    result.error.is_none()
        && result
            .quota
            .as_ref()
            .is_some_and(|quota| quota.status == "allowed" || quota.status == "allowed_warning")
}

/// True when a probe contains enough information to make a rotation decision,
/// including a definitive rejected/below-floor result.
fn has_quota_reading(result: &ProbeResult) -> bool {
    result.error.is_none() && utilization(result).is_some()
}

fn is_hard_auth_failure(result: &ProbeResult) -> bool {
    result.error.as_deref().is_some_and(|error| {
        error.starts_with("HTTP 401")
            || error.starts_with("HTTP 403")
            || error.starts_with(probe::NO_CREDENTIAL)
    })
}

fn probe_is_complete(results: &[ProbeResult], expected: usize) -> bool {
    results.len() == expected
        && results
            .iter()
            .all(|result| has_quota_reading(result) || is_hard_auth_failure(result))
}

/// Derive policy mode without entering sip-and-drain from a partial fleet view.
/// A successful normal candidate is sufficient to return to normal; exhausting
/// the normal pool requires an authoritative reading of every configured token.
pub fn safe_mode_for(
    results: &[ProbeResult],
    expected: usize,
    policy: &RotationSettings,
    previous: RotationMode,
    target_model: Option<&str>,
) -> RotationMode {
    let normal_candidate_seen = results
        .iter()
        .any(|result| is_viable(result, RotationMode::Normal, policy, target_model));
    if probe_is_complete(results, expected) || normal_candidate_seen {
        mode_for(results, policy, target_model)
    } else {
        previous
    }
}

pub fn is_viable(
    result: &ProbeResult,
    mode: RotationMode,
    policy: &RotationSettings,
    target_model: Option<&str>,
) -> bool {
    if !status_allows(result) {
        return false;
    }
    let Some((five, seven)) = utilization(result) else {
        return false;
    };
    let (five_floor, seven_floor) = floors(mode, policy);
    // Compare utilization directly: 1.0 - 0.95 can be slightly greater than
    // 0.05 and accidentally make the exact boundary appear viable.
    five < 1.0 - five_floor
        && seven < 1.0 - seven_floor
        && relevant_model_windows(result, target_model)
            .into_iter()
            .all(|window| window.utilization < 1.0 - seven_floor)
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ModelDescriptor {
    family: String,
    version: Option<String>,
}

fn normalized_model(value: &str) -> String {
    value
        .split('[')
        .next()
        .unwrap_or(value)
        .chars()
        .filter(|ch| ch.is_ascii_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect()
}

fn family_version(value: &str) -> Option<ModelDescriptor> {
    let base = value
        .split('[')
        .next()
        .unwrap_or(value)
        .to_ascii_lowercase();
    let normalized = normalized_model(&base);
    for family in ["opus", "sonnet", "fable"] {
        let Some(normalized_start) = normalized.find(family) else {
            continue;
        };
        let version = if let Some(raw_start) = base.find(family) {
            let parts = base[raw_start + family.len()..]
                .split(|ch: char| !ch.is_ascii_digit())
                .filter(|part| !part.is_empty())
                .collect::<Vec<_>>();
            match parts.as_slice() {
                [major, minor, ..] if major.len() == 1 && minor.len() == 1 => {
                    Some(format!("{major}{minor}"))
                }
                [major, ..] => Some((*major).to_owned()),
                [] => None,
            }
        } else {
            let suffix = &normalized[normalized_start + family.len()..];
            (!suffix.is_empty()).then(|| suffix.to_owned())
        };
        return Some(ModelDescriptor {
            family: family.into(),
            version: if version.is_none() {
                // Claude Code's `opus` alias currently selects Opus 5. Keeping
                // that mapping here prevents an exhausted Opus 4.8/Fable
                // bucket from poisoning new Opus 5 sessions.
                (family == "opus" && normalized == "opus").then(|| "5".into())
            } else {
                version
            },
        });
    }
    None
}

fn bucket_matches_model(bucket: &ModelQuotaBucket, target_model: &str) -> bool {
    let target = family_version(target_model);
    let bucket_descriptor = family_version(&bucket.key).or_else(|| family_version(&bucket.label));
    match (target, bucket_descriptor) {
        (Some(target), Some(bucket)) if target.family == bucket.family => {
            match (target.version.as_deref(), bucket.version.as_deref()) {
                (_, None) => true,
                // A major-only bucket ("Fable 5") covers its point releases.
                (Some(target), Some(bucket)) if bucket.len() == 1 => target.starts_with(bucket),
                (target, bucket) => target == bucket,
            }
        }
        (Some(_), Some(_)) => false,
        _ => {
            let target = normalized_model(target_model);
            let key = normalized_model(&bucket.key);
            let label = normalized_model(&bucket.label);
            !target.is_empty() && (key == target || label == target)
        }
    }
}

fn relevant_model_windows<'a>(
    result: &'a ProbeResult,
    target_model: Option<&str>,
) -> Vec<&'a crate::probe::Window> {
    let Some(target_model) = target_model else {
        return Vec::new();
    };
    let family = family_version(target_model).map(|descriptor| descriptor.family);
    let mut windows = Vec::new();
    if family.as_deref() == Some("fable") {
        // The header form of the Fable limit, when the probe carried it.
        windows.extend(result.quota.as_ref().and_then(|quota| quota.fable.as_ref()));
    }
    let Some(usage) = result.model_usage.as_ref() else {
        return windows;
    };
    match family.as_deref() {
        Some("opus") => windows.extend(usage.opus_weekly.as_ref()),
        Some("sonnet") => windows.extend(usage.sonnet_weekly.as_ref()),
        _ => {}
    }
    windows.extend(
        usage
            .scoped_weekly
            .iter()
            .filter(|bucket| bucket_matches_model(bucket, target_model))
            .map(|bucket| &bucket.window),
    );
    windows
}

pub fn mode_for(
    results: &[ProbeResult],
    policy: &RotationSettings,
    target_model: Option<&str>,
) -> RotationMode {
    if results
        .iter()
        .any(|result| is_viable(result, RotationMode::Normal, policy, target_model))
    {
        RotationMode::Normal
    } else if results
        .iter()
        .any(|result| is_viable(result, RotationMode::SipAndDrain, policy, target_model))
    {
        RotationMode::SipAndDrain
    } else {
        // An exhausted fleet is still an emergency. Returning to normal here
        // made the daemon advertise 120s sampling exactly when it needed to
        // notice a newly reset or newly added account as quickly as possible.
        RotationMode::SipAndDrain
    }
}

pub fn floors(mode: RotationMode, policy: &RotationSettings) -> (f64, f64) {
    match mode {
        RotationMode::Normal => (
            policy.normal_min_5h_remaining,
            policy.normal_min_7d_remaining,
        ),
        RotationMode::SipAndDrain => (policy.sip_min_5h_remaining, policy.sip_min_7d_remaining),
    }
}

pub fn interval_secs(mode: RotationMode, policy: &RotationSettings) -> u64 {
    match mode {
        RotationMode::Normal => policy.normal_probe_interval_secs,
        RotationMode::SipAndDrain => policy.sip_probe_interval_secs,
    }
}

fn fast_interval_secs(policy: &RotationSettings) -> u64 {
    policy
        .sip_probe_interval_secs
        .min(policy.normal_probe_interval_secs)
}

fn active_near_normal_floor(result: &ProbeResult, policy: &RotationSettings) -> bool {
    let (Some(five), Some(seven)) = remaining(result) else {
        return true;
    };
    five <= policy.normal_min_5h_remaining * 2.0 || seven <= policy.normal_min_7d_remaining * 2.0
}

fn next_probe_interval_secs(
    mode: RotationMode,
    active: Option<&ProbeResult>,
    complete: bool,
    policy: &RotationSettings,
) -> u64 {
    if mode == RotationMode::SipAndDrain
        || !complete
        || active.is_none_or(|result| !has_quota_reading(result))
        || active.is_some_and(|result| active_near_normal_floor(result, policy))
    {
        fast_interval_secs(policy)
    } else {
        policy.normal_probe_interval_secs
    }
}

fn active_probe_is_degraded(
    current_name: Option<&str>,
    current_result: Option<&ProbeResult>,
) -> bool {
    current_name.is_some()
        && current_result
            .is_none_or(|result| !has_quota_reading(result) && !is_hard_auth_failure(result))
}

fn compact_error(error: &str) -> String {
    crate::text::one_line(error, 160)
}

/// A few words for why a probe produced no quota, so a summary line groups
/// accounts by cause instead of repeating every response body. The full error
/// is still available in `tokeman rotate status` and the snapshot store.
fn failure_reason(error: Option<&str>) -> String {
    let Some(error) = error else {
        return "no quota headers".into();
    };
    let lower = error.to_ascii_lowercase();
    let status = error
        .strip_prefix("HTTP ")
        .and_then(|rest| rest.split_whitespace().next())
        .filter(|code| code.chars().all(|c| c.is_ascii_digit()));
    let cause = if lower.contains("revoked") {
        "revoked"
    } else if lower.contains("has expired") {
        "expired"
    } else if lower.contains("not allowed for this organization") {
        "org disallows OAuth"
    } else if lower.contains("overloaded") {
        "overloaded"
    } else if error.starts_with(probe::NO_CREDENTIAL) {
        return probe::NO_CREDENTIAL.into();
    } else if lower.contains("error sending request") || lower.contains("timed out") {
        return "network".into();
    } else {
        return match status {
            Some(code) => format!("HTTP {code}"),
            None => compact_error(error).chars().take(60).collect(),
        };
    };
    match status {
        Some(code) => format!("{code} {cause}"),
        None => cause.into(),
    }
}

fn degraded_probe_summary(results: &[ProbeResult], expected: usize) -> String {
    let available = results
        .iter()
        .filter(|result| has_quota_reading(result))
        .count();
    let mut by_reason = BTreeMap::<String, Vec<&str>>::new();
    for result in results.iter().filter(|result| !has_quota_reading(result)) {
        by_reason
            .entry(failure_reason(result.error.as_deref()))
            .or_default()
            .push(&result.token_name);
    }
    if by_reason.is_empty() {
        return format!("{available}/{expected} quota readings");
    }
    let groups = by_reason
        .into_iter()
        .map(|(reason, accounts)| format!("{reason}: {}", accounts.join(", ")))
        .collect::<Vec<_>>();
    format!(
        "{available}/{expected} quota readings; unavailable [{}]",
        groups.join("; ")
    )
}

/// Live Claude sessions still bound to each account.
///
/// A Claude process resolves its credential once at startup, so an account the
/// daemon has rotated away from keeps burning until those processes exit.
/// Measured across 70 switches: the outgoing account burns a further 1% of its
/// weekly quota at the median, but up to 28% when several sessions are bound to
/// it. That tail is how an account tokeman had already decided to protect still
/// reached 96% used, hours after it stopped being the default.
pub fn bound_session_counts(config: &Config) -> BTreeMap<String, usize> {
    let mut counts = BTreeMap::new();
    for session in claude_sessions(config) {
        if let Some(account) = session.account {
            *counts.entry(account).or_insert(0) += 1;
        }
    }
    counts
}

/// Compatibility wrapper: load-blind selection, used by the tray and by tests
/// that assert the floors in isolation.
#[allow(dead_code)]
pub fn choose_best<'a>(
    results: &'a [ProbeResult],
    mode: RotationMode,
    policy: &RotationSettings,
    target_model: Option<&str>,
) -> Option<&'a ProbeResult> {
    choose_best_with_load(results, mode, policy, target_model, &BTreeMap::new())
}

/// Pick a landing zone, discounting candidates that live sessions are already
/// draining. Bound sessions are committed burn the probe has not seen yet, so
/// an account carrying three of them is a worse destination than its raw
/// headroom suggests. This only reorders candidates that are already viable --
/// it never rescues nor rejects one, so the floors keep their exact meaning.
pub fn choose_best_with_load<'a>(
    results: &'a [ProbeResult],
    mode: RotationMode,
    policy: &RotationSettings,
    target_model: Option<&str>,
    bound: &BTreeMap<String, usize>,
) -> Option<&'a ProbeResult> {
    let mut viable: Vec<_> = results
        .iter()
        .filter(|result| is_viable(result, mode, policy, target_model))
        .collect();
    viable.sort_by(|a, b| {
        let (a_five, a_seven) = utilization(a).unwrap_or((1.0, 1.0));
        let (b_five, b_seven) = utilization(b).unwrap_or((1.0, 1.0));
        // A replacement account must absorb cold prompt-cache creation from
        // every long-running session that lands on it. Generic quota probes do
        // not expose whether one enormous Opus/1M turn will be admitted, so
        // balanced headroom is the safety signal and the primary sort key.
        // Once two candidates offer equal landing room, consume the one whose
        // limiting window replenishes sooner.
        compare_f64(
            loaded_dwell_score(b, mode, policy, target_model, bound),
            loaded_dwell_score(a, mode, policy, target_model, bound),
        )
        .then_with(|| {
            compare_optional_delay(
                limiting_reset_delay_secs(a, mode, policy, target_model),
                limiting_reset_delay_secs(b, mode, policy, target_model),
            )
        })
        .then_with(|| compare_f64(a_five, b_five))
        .then_with(|| compare_f64(a_seven, b_seven))
        .then_with(|| a.token_name.cmp(&b.token_name))
    });
    viable.into_iter().next()
}

/// `dwell_score` minus the headroom already spoken for by sessions bound to
/// this account. Clamped at the raw score so a heavily loaded account can be
/// deprioritized but never scored below an account with no room at all.
fn loaded_dwell_score(
    result: &ProbeResult,
    mode: RotationMode,
    policy: &RotationSettings,
    target_model: Option<&str>,
    bound: &BTreeMap<String, usize>,
) -> f64 {
    let raw = dwell_score(result, mode, policy, target_model);
    if !raw.is_finite() {
        return raw;
    }
    let sessions = bound.get(&result.token_name).copied().unwrap_or(0);
    raw - (sessions as f64) * policy.per_session_reserve
}

fn dwell_score(
    result: &ProbeResult,
    mode: RotationMode,
    policy: &RotationSettings,
    target_model: Option<&str>,
) -> f64 {
    let Some((five_margin, seven_margin)) = normalized_margins(result, mode, policy) else {
        return f64::NEG_INFINITY;
    };
    let (_, seven_floor) = floors(mode, policy);
    relevant_model_windows(result, target_model)
        .into_iter()
        .map(|window| ((1.0 - window.utilization) - seven_floor) / (1.0 - seven_floor))
        .fold(five_margin.min(seven_margin), f64::min)
}

fn normalized_margins(
    result: &ProbeResult,
    mode: RotationMode,
    policy: &RotationSettings,
) -> Option<(f64, f64)> {
    let (five_used, seven_used) = utilization(result)?;
    let (five_floor, seven_floor) = floors(mode, policy);
    let five_margin = ((1.0 - five_used) - five_floor) / (1.0 - five_floor);
    let seven_margin = ((1.0 - seven_used) - seven_floor) / (1.0 - seven_floor);
    Some((five_margin, seven_margin))
}

/// Delay until the reset of whichever quota window has less normalized room
/// above its active floor. A reset value of zero means the API omitted it.
fn limiting_reset_delay_secs(
    result: &ProbeResult,
    mode: RotationMode,
    policy: &RotationSettings,
    target_model: Option<&str>,
) -> Option<i64> {
    let quota = result.quota.as_ref()?;
    let session = quota.session.as_ref()?;
    let weekly = quota.weekly.as_ref()?;
    let (five_margin, seven_margin) = normalized_margins(result, mode, policy)?;
    let (_, seven_floor) = floors(mode, policy);
    let mut windows = vec![(five_margin, session.reset), (seven_margin, weekly.reset)];
    windows.extend(
        relevant_model_windows(result, target_model)
            .into_iter()
            .map(|window| {
                (
                    ((1.0 - window.utilization) - seven_floor) / (1.0 - seven_floor),
                    window.reset,
                )
            }),
    );
    windows.sort_by(|(a_margin, a_reset), (b_margin, b_reset)| {
        compare_f64(*a_margin, *b_margin).then_with(|| match (*a_reset > 0, *b_reset > 0) {
            (true, true) => a_reset.cmp(b_reset),
            (true, false) => Ordering::Less,
            (false, true) => Ordering::Greater,
            (false, false) => Ordering::Equal,
        })
    });
    let reset = windows.first().map(|(_, reset)| *reset).unwrap_or(0);
    (reset > 0).then(|| reset.saturating_sub(result.probed_at.timestamp()))
}

fn compare_optional_delay(a: Option<i64>, b: Option<i64>) -> Ordering {
    match (a, b) {
        (Some(a), Some(b)) => a.cmp(&b),
        (Some(_), None) => Ordering::Less,
        (None, Some(_)) => Ordering::Greater,
        (None, None) => Ordering::Equal,
    }
}

fn compare_f64(a: f64, b: f64) -> Ordering {
    a.partial_cmp(&b).unwrap_or(Ordering::Equal)
}

fn describe(result: Option<&ProbeResult>) -> String {
    let Some(result) = result else {
        return "quota unavailable".into();
    };
    let (five, seven) = remaining(result);
    match (five, seven) {
        (Some(five), Some(seven)) => {
            let mut description = format!(
                "5h {:.0}% left, 7d {:.0}% left",
                five * 100.0,
                seven * 100.0
            );
            if let Some(usage) = result.model_usage.as_ref() {
                if let Some(window) = usage.opus_weekly.as_ref() {
                    description.push_str(&format!(
                        ", Opus7d {:.0}% left",
                        (1.0 - window.utilization) * 100.0
                    ));
                }
                if let Some(window) = usage.sonnet_weekly.as_ref() {
                    description.push_str(&format!(
                        ", Sonnet7d {:.0}% left",
                        (1.0 - window.utilization) * 100.0
                    ));
                }
            }
            description
        }
        _ => "quota unavailable".into(),
    }
}

/// Fold this cycle's outcomes into the recheck schedule.
fn update_auth_backoff(
    backoff: &mut BTreeMap<String, AuthBackoff>,
    results: &[ProbeResult],
    probed: &std::collections::HashSet<String>,
    now: f64,
) -> Vec<String> {
    let mut newly_parked = Vec::new();
    for result in results {
        if !probed.contains(&result.token_name) {
            continue;
        }
        if is_hard_auth_failure(result) {
            let entry = backoff
                .entry(result.token_name.clone())
                .or_insert(AuthBackoff {
                    failures: 0,
                    next_attempt_epoch: 0.0,
                    last_error: String::new(),
                });
            entry.failures = entry.failures.saturating_add(1);
            entry.last_error = result.error.clone().unwrap_or_default();
            entry.next_attempt_epoch = now + auth_backoff_secs(entry.failures);
            if entry.failures == 1 {
                newly_parked.push(result.token_name.clone());
            }
        } else if backoff.remove(&result.token_name).is_some() {
            append_log(&format!(
                "{} answered again; clearing authentication backoff",
                result.token_name
            ));
        }
    }
    newly_parked
}

/// Model families with a weekly limit that only appears in rate-limit headers
/// on requests to that family: the Fable limit (`7d_oi`).
const HEADER_ONLY_FAMILIES: &[&str] = &["fable"];

/// The model to sample with, when the startup model's family has a
/// header-only limit. `[1m]` is a Claude Code context option, not part of the
/// API id; bare aliases (`fable`) name no API model, so they are not sampled.
fn sample_model(target_model: Option<&str>) -> Option<&str> {
    let base = target_model?.split('[').next()?.trim();
    let family = family_version(base)?.family;
    (base.starts_with("claude-") && HEADER_ONLY_FAMILIES.contains(&family.as_str())).then_some(base)
}

fn due_for_sample(
    token: &Token,
    samples: &BTreeMap<String, HeaderSample>,
    policy: &RotationSettings,
    now: f64,
) -> bool {
    let interval = policy.target_model_sample_secs as f64;
    // An account with profile reads already gets the limit as a model row.
    interval > 0.0
        && token.usage_credential((now * 1000.0) as i64).is_none()
        && samples
            .get(&token.name)
            .is_none_or(|sample| now - sample.sampled_at >= interval)
}

fn describe_header_sampling(
    config: &Config,
    cadence: &CadenceState,
    target_model: Option<&str>,
    now: f64,
) -> String {
    let Some(model) = sample_model(target_model) else {
        return "none for the startup model".into();
    };
    let interval = config.rotation.target_model_sample_secs;
    if interval == 0 {
        return format!("{model} has one, but sampling is off (target_model_sample_secs = 0)");
    }
    let eligible: Vec<&Token> = config
        .tokens
        .iter()
        .filter(|token| token.usage_credential((now * 1000.0) as i64).is_none())
        .filter(|token| {
            cadence
                .auth_backoff
                .get(&token.name)
                .is_none_or(|entry| now >= entry.next_attempt_epoch)
        })
        .collect();
    if eligible.is_empty() {
        return format!("{model}: no account needs sampling");
    }
    let ages: Vec<f64> = eligible
        .iter()
        .filter_map(|token| cadence.header_samples.get(&token.name))
        .map(|sample| ((now - sample.sampled_at) / 60.0).max(0.0))
        .collect();
    let sampled = match ages.iter().copied().reduce(f64::max) {
        Some(oldest) => format!("{} sampled, oldest {:.0}m ago", ages.len(), oldest),
        None => "none sampled yet".into(),
    };
    format!(
        "{model} probe every {}m on {} account(s) without profile reads ({sampled})",
        interval / 60,
        eligible.len(),
    )
}

/// Record every sample attempt, answered or not, so an account whose sample
/// keeps failing is still only asked once per interval.
fn record_header_samples(
    samples: &[ProbeResult],
    cache: &mut BTreeMap<String, HeaderSample>,
    now: f64,
) {
    for sample in samples {
        let answered = sample.quota.is_some();
        if !answered
            && cache
                .get(&sample.token_name)
                .is_none_or(|previous| previous.answered)
        {
            append_log(&format!(
                "target-model sample for {} got no quota headers ({}); next try in one interval",
                sample.token_name,
                failure_reason(sample.error.as_deref())
            ));
        }
        cache.insert(
            sample.token_name.clone(),
            HeaderSample {
                sampled_at: now,
                answered,
                window: sample.quota.as_ref().and_then(|quota| quota.fable.clone()),
            },
        );
    }
}

/// Carry each account's latest sampled window onto its probe result, until
/// the window resets, as an ordinary per-model bucket.
fn apply_header_samples(
    results: &mut [ProbeResult],
    cache: &BTreeMap<String, HeaderSample>,
    now: f64,
) {
    for result in results {
        let Some(quota) = result.quota.as_mut() else {
            continue;
        };
        if quota.fable.is_some() {
            continue;
        }
        let carried = cache
            .get(&result.token_name)
            .and_then(|sample| sample.window.clone())
            .filter(|window| window.reset == 0 || window.reset as f64 > now);
        if carried.is_some() {
            quota.fable = carried;
            probe::attach_header_buckets(result);
        }
    }
}

/// Probe every account that is not in authentication backoff, record the
/// snapshots, and restate the refusals of the ones that are.
///
/// When `sample` is set and the startup model has a header-only limit, the
/// accounts due for a sample are probed with that model instead of Haiku.
/// Skipped accounts keep the batch the same size and shape, so completeness,
/// sticky holds and viability behave as if they had been asked and refused;
/// their synthesized rows are not stored, being a restatement rather than an
/// observation.
async fn probe_and_store(
    config: &Config,
    cadence: &mut CadenceState,
    target_model: Option<&str>,
    now: f64,
    force: bool,
    sample: bool,
) -> Vec<ProbeResult> {
    let (due, skipped): (Vec<_>, Vec<_>) = config.tokens.iter().cloned().partition(|token| {
        force
            || cadence
                .auth_backoff
                .get(&token.name)
                .is_none_or(|entry| now >= entry.next_attempt_epoch)
    });
    let sample_with = sample.then(|| sample_model(target_model)).flatten();
    let to_sample: Vec<Token> = match sample_with {
        Some(_) => due
            .iter()
            .filter(|token| due_for_sample(token, &cadence.header_samples, &config.rotation, now))
            .cloned()
            .collect(),
        None => Vec::new(),
    };
    // Match the dashboard's timeout. All token requests are concurrent, and a
    // slow-but-valid 20-40s response is safer than treating a partial 15s
    // batch as fleet truth. The scheduler skips missed ticks. A sample is an
    // extra request next to the Haiku probe, never a replacement: whatever it
    // answers, the cycle keeps its quota reading.
    let timeout = std::time::Duration::from_secs(45);
    let (mut results, samples) = futures::join!(
        probe::probe_all_with_timeout(&due, timeout),
        probe::probe_all_with_model(
            &to_sample,
            timeout,
            sample_with.unwrap_or(probe::PROBE_MODEL)
        ),
    );
    record_header_samples(&samples, &mut cadence.header_samples, now);
    apply_header_samples(&mut results, &cadence.header_samples, now);
    admission::apply_observed_limits(&mut results);
    if let Ok(store) = Store::open() {
        for result in &results {
            let _ = store.insert(result);
        }
    }
    let probed_at = Utc::now();
    for token in skipped {
        let error = cadence
            .auth_backoff
            .get(&token.name)
            .map(|entry| entry.last_error.clone())
            .unwrap_or_else(|| "authentication previously refused".to_string());
        results.push(ProbeResult {
            token_name: token.name,
            probed_at,
            quota: None,
            model_usage: None,
            model_usage_error: None,
            rate_limits: Default::default(),
            error: Some(error),
        });
    }
    // Configured order, as every consumer displays it.
    results.sort_by_key(|result| {
        config
            .tokens
            .iter()
            .position(|token| token.name == result.token_name)
    });
    results
}

pub async fn rotate(config: &Config, options: RotateOptions) -> Result<RotationOutcome> {
    config.rotation.validate()?;
    if config.tokens.is_empty() {
        bail!("No tokens configured. Use `tokeman add <name> <key>` to add one.");
    }
    let _lock = RotationLock::acquire()?;

    if pause_path()?.exists() {
        return Ok(RotationOutcome::held(
            "paused",
            load_cadence().mode.unwrap_or(RotationMode::Normal),
            None,
            false,
            "rotation is paused",
        ));
    }
    if !options.scheduled && suppress_login_shadow(false)? {
        append_log("suppressed short-lived /login Keychain auth before managed rotation");
    }

    let new_admission_limits = match admission::scan_transcripts(&config.tokens) {
        Ok(observed) => observed,
        Err(error) => {
            append_log(&format!(
                "ERROR Claude admission feedback scan failed: {error:#}"
            ));
            0
        }
    };
    if new_admission_limits > 0 {
        append_log(&format!(
            "observed {new_admission_limits} Claude quota rejection(s); reevaluating default now"
        ));
    }

    let mut cadence = load_cadence();
    let previous_mode = cadence.mode.unwrap_or(RotationMode::Normal);
    let probe_started_epoch = now_epoch();
    if options.scheduled && new_admission_limits == 0 {
        let interval = cadence
            .probe_interval_secs
            .unwrap_or_else(|| interval_secs(previous_mode, &config.rotation))
            as f64;
        if probe_started_epoch - cadence.last_probe_epoch < interval {
            return Ok(RotationOutcome::held(
                "cadence-skip",
                previous_mode,
                None,
                false,
                format!("next probe is not due ({interval:.0}s cadence)"),
            ));
        }
        // Track probe starts, not completions. Otherwise request latency is
        // silently added to the configured cadence (20s + a 15s timeout).
        cadence.last_probe_epoch = probe_started_epoch;
        save_cadence(&cadence)?;
    }

    let mut settings = load_claude_settings()?;
    let target_model = configured_target_model(&settings).map(str::to_owned);
    let current_value = configured_token(&settings).map(str::to_owned);
    let current_name =
        token_name_for_value(&config.tokens, current_value.as_deref()).map(str::to_owned);
    if let Some(account) = current_name.as_deref()
        && !options.dry_run
    {
        let token = config
            .tokens
            .iter()
            .find(|token| token.name == account)
            .context("managed Claude OAuth token disappeared from configuration")?;
        if let Some(kind) = reinstall_reason(&settings, token) {
            let event = install_credential(&mut settings, token, kind)?;
            commit_settings(&settings, Some(event))?;
            if kind != "repair" {
                append_log(&format!(
                    "{kind}: {account} now offers {}",
                    describe_credential(token)
                ));
            }
        }
    }
    let probe_now = now_epoch();
    let probed: std::collections::HashSet<String> = config
        .tokens
        .iter()
        .filter(|token| {
            options.force
                || cadence
                    .auth_backoff
                    .get(&token.name)
                    .is_none_or(|entry| probe_now >= entry.next_attempt_epoch)
        })
        .map(|token| token.name.clone())
        .collect();
    // A dry run must not spend target-model requests.
    let results = probe_and_store(
        config,
        &mut cadence,
        target_model.as_deref(),
        probe_now,
        options.force,
        !options.dry_run,
    )
    .await;
    for account in update_auth_backoff(&mut cadence.auth_backoff, &results, &probed, probe_now) {
        append_log(&format!(
            "{account} refused authentication; rechecking on a backoff instead of every cycle"
        ));
    }
    let probe_has_quota = results.iter().any(has_quota_reading);
    let complete_probe = probe_is_complete(&results, config.tokens.len());
    let mode = safe_mode_for(
        &results,
        config.tokens.len(),
        &config.rotation,
        previous_mode,
        target_model.as_deref(),
    );
    let current_result = current_name
        .as_deref()
        .and_then(|name| results.iter().find(|result| result.token_name == name));
    // What to call the default in messages: a configured account, or a token
    // tokeman does not manage.
    let active_label = current_name.clone().or_else(|| {
        current_value
            .as_ref()
            .map(|_| "unmanaged OAuth token".to_string())
    });

    // Sessions bound to an account are burn the daemon cannot redirect: a
    // Claude process keeps the credential it resolved at startup. Read them
    // once per cycle so both the landing-zone choice and the drain watch below
    // work from the same picture.
    let bound = bound_session_counts(config);
    let drains = abandoned_drains(
        &results,
        current_name.as_deref(),
        &bound,
        mode,
        &config.rotation,
        target_model.as_deref(),
    );
    let drained_now: Vec<String> = drains.iter().map(|d| d.account.clone()).collect();
    if drained_now != cadence.drained_accounts {
        for drain in &drains {
            if !cadence.drained_accounts.contains(&drain.account) {
                append_log(&format!(
                    "WARNING abandoned drain: {}; rotating the default cannot move them -- restart or resume those sessions to release it",
                    drain.summary()
                ));
            }
        }
        for account in &cadence.drained_accounts {
            if !drained_now.contains(account) {
                append_log(&format!("abandoned drain cleared for {account}"));
            }
        }
        cadence.drained_accounts = drained_now;
    }

    let next_interval =
        next_probe_interval_secs(mode, current_result, complete_probe, &config.rotation);
    cadence.last_probe_epoch = probe_started_epoch;
    cadence.mode = Some(mode);
    cadence.probe_interval_secs = Some(next_interval);
    if complete_probe {
        cadence.consecutive_degraded_probes = 0;
    } else {
        cadence.consecutive_degraded_probes = cadence.consecutive_degraded_probes.saturating_add(1);
    }
    save_cadence(&cadence)?;

    if !probe_has_quota {
        let active = active_label.clone();
        let message = format!(
            "probe failed for every token; kept {}; retrying in {}s ({})",
            active.as_deref().unwrap_or("/login"),
            next_interval,
            degraded_probe_summary(&results, config.tokens.len())
        );
        append_log(&message);
        return Ok(RotationOutcome::held(
            "probe-failed",
            mode,
            active,
            true,
            message,
        ));
    }

    if mode != previous_mode {
        let (five, seven) = floors(mode, &config.rotation);
        append_log(&format!(
            "entered {} mode; sampling every {}s and rotating at {:.0}% 5h / {:.0}% 7d",
            mode.label(),
            interval_secs(mode, &config.rotation),
            five * 100.0,
            seven * 100.0
        ));
    }

    // A partial batch can prove that the current default is healthy, but it
    // cannot safely select a replacement: an omitted account may be the only
    // roomy landing zone. Retry on the fast cadence instead of rotating from
    // whichever subset happened to answer.
    if !complete_probe && !options.force {
        let default = active_label.clone();
        let message = format!(
            "partial probe; kept default {}; retrying in {}s ({})",
            default.as_deref().unwrap_or("/login"),
            next_interval,
            degraded_probe_summary(&results, config.tokens.len())
        );
        append_log(&message);
        return Ok(RotationOutcome::held(
            "partial-probe-hold",
            mode,
            default,
            true,
            message,
        ));
    }

    // Missing quota is not evidence that the default token crossed a floor.
    // The old behavior switched to whichever partial result happened to
    // succeed, which caused the cmrx incident. Keep the known credential and
    // retry quickly instead.
    if active_probe_is_degraded(current_name.as_deref(), current_result) && !options.force {
        let message = format!(
            "default quota unavailable; kept {}; retrying in {}s ({})",
            current_name.as_deref().unwrap_or("default token"),
            next_interval,
            degraded_probe_summary(&results, config.tokens.len())
        );
        append_log(&message);
        return Ok(RotationOutcome::held(
            "default-probe-degraded",
            mode,
            current_name,
            true,
            message,
        ));
    }

    let current_healthy = current_result
        .is_some_and(|result| is_viable(result, mode, &config.rotation, target_model.as_deref()));
    if current_healthy && !options.force {
        let message = format!(
            "kept {} ({})",
            current_name.as_deref().unwrap_or("default token"),
            describe(current_result)
        );
        return Ok(RotationOutcome::held(
            "kept",
            mode,
            current_name,
            true,
            message,
        ));
    }

    let Some(best) = choose_best_with_load(
        &results,
        mode,
        &config.rotation,
        target_model.as_deref(),
        &bound,
    ) else {
        let active = active_label.clone();
        let message = format!(
            "no viable token; kept {}",
            active.as_deref().unwrap_or("/login")
        );
        append_log(&message);
        return Ok(RotationOutcome::held(
            "no-viable-token",
            mode,
            active,
            true,
            message,
        ));
    };

    let best_token = config
        .tokens
        .iter()
        .find(|token| token.name == best.token_name)
        .context("probe returned a token absent from configuration")?;
    if current_name.as_deref() == Some(best.token_name.as_str()) {
        let message = format!("kept {} ({})", best.token_name, describe(Some(best)));
        return Ok(RotationOutcome::held(
            "kept",
            mode,
            current_name,
            true,
            message,
        ));
    }

    let trigger = match current_result {
        None => "no managed token was active".into(),
        Some(result) if current_healthy && options.force => {
            format!(
                "forced best-token selection from {}",
                describe(Some(result))
            )
        }
        Some(result) if is_hard_auth_failure(result) => {
            format!("authentication rejected ({})", describe(Some(result)))
        }
        Some(result) => format!(
            "{} threshold reached ({})",
            mode.label(),
            describe(Some(result))
        ),
    };
    let message = format!(
        "switch {} -> {}; {}; new {}",
        current_name.as_deref().unwrap_or("/login"),
        best.token_name,
        trigger,
        describe(Some(best))
    );

    if options.dry_run {
        return Ok(RotationOutcome {
            action: "would-switch".into(),
            mode,
            active_before: current_name.clone(),
            active_after: Some(best.token_name.clone()),
            changed: false,
            probed: true,
            message: format!("would {message}"),
        });
    }

    if !options.scheduled && suppress_login_shadow(false)? {
        append_log("suppressed short-lived /login Keychain auth before credential switch");
    }
    let event = install_credential(&mut settings, best_token, "switch")?;
    commit_settings(&settings, Some(event))?;
    append_log(&message);
    Ok(RotationOutcome {
        action: "switched".into(),
        mode,
        active_before: current_name,
        active_after: Some(best.token_name.clone()),
        changed: true,
        probed: true,
        message,
    })
}

#[derive(Debug, Clone)]
struct DefaultEvent {
    at: i64,
    account: Option<String>,
}

fn default_events() -> Vec<DefaultEvent> {
    let Ok(path) = log_path() else {
        return Vec::new();
    };
    let mut events = [previous_log_path(&path), path]
        .iter()
        .filter_map(|path| std::fs::read_to_string(path).ok())
        .flat_map(|contents| {
            contents
                .lines()
                .filter_map(parse_default_event)
                .collect::<Vec<_>>()
        })
        .collect::<Vec<_>>();
    events.sort_by_key(|event| event.at);
    events
}

/// Every credential `settings.json` has offered, oldest first: the recorded
/// history, preceded by default changes reconstructed from the rotation log.
pub fn credential_timeline() -> Vec<CredentialEvent> {
    credential_history::with_legacy(
        credential_history::load(),
        default_events()
            .into_iter()
            .map(|event| (event.at, event.account)),
    )
}

fn parse_default_event(line: &str) -> Option<DefaultEvent> {
    let (stamp, message) = line.split_once(' ')?;
    let at = chrono::DateTime::parse_from_rfc3339(stamp)
        .ok()?
        .timestamp();
    let account = if let Some(name) = message.strip_prefix("explicitly activated ") {
        Some(name.trim().to_owned())
    } else if let Some(rest) = message.strip_prefix("switch ") {
        let (_, destination) = rest.split_once(" -> ")?;
        Some(destination.split(';').next()?.trim().to_owned())
    } else if message.starts_with("rotation paused;") {
        None
    } else {
        return None;
    };
    Some(DefaultEvent { at, account })
}

fn parse_ps_session(line: &str) -> Option<(u32, Option<String>, chrono::DateTime<Utc>)> {
    // macOS: pid tty weekday month day HH:MM:SS year comm
    let fields = line.split_whitespace().collect::<Vec<_>>();
    if fields.len() < 8 || fields[7].rsplit('/').next()? != "claude" {
        return None;
    }
    let pid = fields[0].parse().ok()?;
    let tty = (fields[1] != "??").then(|| fields[1].to_owned());
    let local = NaiveDateTime::parse_from_str(
        &format!(
            "{} {} {} {} {}",
            fields[2], fields[3], fields[4], fields[5], fields[6]
        ),
        "%a %b %e %T %Y",
    )
    .ok()?;
    let started_at = Local
        .from_local_datetime(&local)
        .single()
        .or_else(|| Local.from_local_datetime(&local).earliest())?
        .with_timezone(&Utc);
    Some((pid, tty, started_at))
}

/// Inspect live Claude processes without exposing credentials.
///
/// The account is derived from the credential timeline: the token settings
/// offered when the process started, followed through every expiry at which
/// Claude would have adopted a newer one. Neither the SessionStart hook's
/// environment nor `ps eww` can say this: both show what settings offer now or
/// at exec time, not the token the process has cached.
pub fn claude_sessions(_config: &Config) -> Vec<ClaudeSessionStatus> {
    let Ok(output) = Command::new("/bin/ps")
        .args(["-axo", "pid=,tty=,lstart=,comm="])
        .output()
    else {
        return Vec::new();
    };
    if !output.status.success() {
        return Vec::new();
    }
    let timeline = credential_timeline();
    let now = Utc::now().timestamp();
    let mut sessions = String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(parse_ps_session)
        .map(|(pid, tty, started_at)| {
            let binding = admission::binding_for_pid(pid, started_at.timestamp());
            let (account, credential_source) =
                match credential_history::account_at(&timeline, started_at.timestamp(), now) {
                    Some(Some(account)) => (Some(account), "credential timeline".to_string()),
                    Some(None) => (None, "no managed credential (/login)".to_string()),
                    None => match &binding {
                        Some(binding) => (
                            Some(binding.account.clone()),
                            "SessionStart binding".to_string(),
                        ),
                        None => (None, "unknown (predates recorded default)".to_string()),
                    },
                };
            ClaudeSessionStatus {
                pid,
                tty,
                started_at,
                session_id: binding.map(|binding| binding.session_id),
                account,
                credential_source,
            }
        })
        .collect::<Vec<_>>();
    sessions.sort_by_key(|session| session.started_at);
    sessions
}

pub fn session_summary(sessions: &[ClaudeSessionStatus]) -> String {
    if sessions.is_empty() {
        return "none detected".into();
    }
    let mut counts = BTreeMap::<String, usize>::new();
    for session in sessions {
        let prefix = if session.credential_source == "credential timeline" {
            ""
        } else {
            "~"
        };
        *counts
            .entry(format!(
                "{prefix}{}",
                session.account.as_deref().unwrap_or("unknown")
            ))
            .or_default() += 1;
    }
    counts
        .into_iter()
        .map(|(name, count)| format!("{name}×{count}"))
        .collect::<Vec<_>>()
        .join(", ")
}

pub async fn status(config: &Config) -> Result<RotationStatus> {
    config.rotation.validate()?;
    let _lock = RotationLock::acquire()?;
    let paused = pause_path()?.exists();
    let settings = load_claude_settings()?;
    let target_model = configured_target_model(&settings).map(str::to_owned);
    let current_value = configured_token(&settings);
    let current_name = token_name_for_value(&config.tokens, current_value).map(str::to_owned);
    let _ = admission::scan_transcripts(&config.tokens);
    // Status carries earlier target-model samples forward but takes none.
    let mut cadence = load_cadence();
    let results = probe_and_store(
        config,
        &mut cadence,
        target_model.as_deref(),
        now_epoch(),
        true,
        false,
    )
    .await;
    let previous_mode = cadence.mode.unwrap_or(RotationMode::Normal);
    let mode = safe_mode_for(
        &results,
        config.tokens.len(),
        &config.rotation,
        previous_mode,
        target_model.as_deref(),
    );
    let (five_floor, seven_floor) = floors(mode, &config.rotation);

    let profile_usage_count = results
        .iter()
        .filter(|result| {
            result.model_usage.as_ref().is_some_and(|usage| {
                usage.opus_source == Some(ModelUsageSource::Profile)
                    || usage.sonnet_source == Some(ModelUsageSource::Profile)
                    || usage
                        .scoped_weekly
                        .iter()
                        .any(|bucket| bucket.source == ModelUsageSource::Profile)
            })
        })
        .count();
    let active_admission_limits = admission::active_limits();
    let tokens = results
        .iter()
        .map(|result| {
            let (remaining_5h, remaining_7d) = remaining(result);
            let remaining_opus_7d = result
                .model_usage
                .as_ref()
                .and_then(|usage| usage.opus_weekly.as_ref())
                .map(|window| 1.0 - window.utilization);
            let remaining_sonnet_7d = result
                .model_usage
                .as_ref()
                .and_then(|usage| usage.sonnet_weekly.as_ref())
                .map(|window| 1.0 - window.utilization);
            let model_buckets = result
                .model_usage
                .as_ref()
                .map(ModelUsage::buckets)
                .unwrap_or_default()
                .into_iter()
                .map(|bucket| RotationModelBucketStatus {
                    relevant: target_model
                        .as_deref()
                        .is_some_and(|model| bucket_matches_model(&bucket, model)),
                    key: bucket.key,
                    label: bucket.label,
                    remaining: 1.0 - bucket.window.utilization,
                    reset: bucket.window.reset,
                    source: bucket.source,
                })
                .collect();
            RotationTokenStatus {
                credential: config
                    .tokens
                    .iter()
                    .find(|token| token.name == result.token_name)
                    .map(describe_credential)
                    .unwrap_or_default(),
                name: result.token_name.clone(),
                remaining_5h,
                remaining_7d,
                remaining_opus_7d,
                remaining_sonnet_7d,
                opus_source: result
                    .model_usage
                    .as_ref()
                    .and_then(|usage| usage.opus_source),
                sonnet_source: result
                    .model_usage
                    .as_ref()
                    .and_then(|usage| usage.sonnet_source),
                model_buckets,
                admission_limits: active_admission_limits
                    .iter()
                    .filter(|limit| limit.account == result.token_name)
                    .cloned()
                    .collect(),
                viable: is_viable(result, mode, &config.rotation, target_model.as_deref()),
                is_default: current_name.as_deref() == Some(result.token_name.as_str()),
                error: result.error.clone(),
            }
        })
        .collect();
    let live_sessions = claude_sessions(config);
    let bound = live_sessions
        .iter()
        .fold(BTreeMap::new(), |mut acc, session| {
            if let Some(account) = session.account.clone() {
                *acc.entry(account).or_insert(0usize) += 1;
            }
            acc
        });
    let drains = abandoned_drains(
        &results,
        current_name.as_deref(),
        &bound,
        mode,
        &config.rotation,
        target_model.as_deref(),
    );

    let header_sampling =
        describe_header_sampling(config, &cadence, target_model.as_deref(), now_epoch());
    Ok(RotationStatus {
        monitor: if paused { "paused" } else { "running" }.into(),
        service_installed: launch_agent_path().is_ok_and(|path| path.exists()),
        mode,
        probe_interval_secs: interval_secs(mode, &config.rotation),
        default_token: current_name
            .or_else(|| current_value.map(|_| "unmanaged OAuth token".to_string())),
        default_is_managed: current_value
            .and_then(|value| token_name_for_value(&config.tokens, Some(value)))
            .is_some(),
        target_model,
        min_5h_remaining: five_floor,
        min_7d_remaining: seven_floor,
        premium_admission: format!(
            "profile buckets {profile_usage_count}/{} accounts; {} active rejection quarantine(s)",
            config.tokens.len(),
            active_admission_limits.len()
        ),
        header_sampling,
        live_sessions,
        abandoned_drains: drains,
        tokens,
    })
}

pub fn print_status(status: &RotationStatus) {
    println!(
        "monitor: {}{}",
        status.monitor,
        if status.service_installed {
            " (service installed)"
        } else {
            ""
        }
    );
    println!(
        "mode: {} ({}s sampling)",
        status.mode.label(),
        status.probe_interval_secs
    );
    println!(
        "quota probe: {} (capacity signal; large-model admission can be stricter)",
        probe::PROBE_MODEL_LABEL
    );
    println!("premium admission: {}", status.premium_admission);
    println!("header-only limits: {}", status.header_sampling);
    println!(
        "default for newly started Claude processes: {}",
        status.default_token.as_deref().unwrap_or("/login")
    );
    println!(
        "selection model: {} (only matching model-scoped buckets affect rotation)",
        status.target_model.as_deref().unwrap_or("unknown")
    );
    println!(
        "live Claude processes: {} ({}; ~=startup estimate, unmarked=derived from credential history)",
        status.live_sessions.len(),
        session_summary(&status.live_sessions)
    );
    for drain in &status.abandoned_drains {
        println!("  ! ABANDONED DRAIN: {}", drain.summary());
        println!(
            "    {:<22} rotating the default cannot move these; restart or resume them",
            ""
        );
    }
    println!(
        "default thresholds: change at 5h <= {:.0}% left or 7d <= {:.0}% left",
        status.min_5h_remaining * 100.0,
        status.min_7d_remaining * 100.0
    );
    for token in &status.tokens {
        let marker = if token.is_default { "D" } else { " " };
        match (token.remaining_5h, token.remaining_7d) {
            (Some(five), Some(seven)) => println!(
                " {marker} {:<24} 5h {:>3.0}%  7d {:>3.0}%  {}",
                token.name,
                five * 100.0,
                seven * 100.0,
                if token.viable { "ready" } else { "below floor" }
            ),
            _ => println!(
                " {marker} {:<24} {}",
                token.name,
                failure_reason(token.error.as_deref())
            ),
        }
        println!("   {:<24} offers {}", "", token.credential);
        if token.model_buckets.is_empty() {
            println!("   {:<24} model-scoped 7d: unavailable", "");
        }
        for bucket in &token.model_buckets {
            println!(
                "   {:<24} {}{} {:>3.0}%  resets {}  {}",
                "",
                if bucket.relevant { "*" } else { " " },
                bucket.label,
                bucket.remaining * 100.0,
                crate::display::format_reset_compact(bucket.reset),
                bucket.source.short_label(),
            );
        }
        for limit in &token.admission_limits {
            println!("   {:<24} ! {}", "", limit.summary());
        }
    }
}

/// Read the OAuth default used when a new Claude process starts.
pub fn default_token_name(config: &Config) -> Result<Option<String>> {
    let settings = load_claude_settings()?;
    Ok(
        token_name_for_value(config.tokens.as_slice(), configured_token(&settings))
            .map(str::to_owned),
    )
}

pub fn target_model_name() -> Result<Option<String>> {
    let settings = load_claude_settings()?;
    Ok(configured_target_model(&settings).map(str::to_owned))
}

pub async fn pause() -> Result<()> {
    let _lock = RotationLock::acquire()?;
    let path = pause_path()?;
    ensure_private_parent(&path)?;
    OpenOptions::new().create(true).append(true).open(&path)?;
    set_mode(&path, 0o600)?;
    let mut settings = load_claude_settings()?;
    if clear_configured_token(&mut settings) {
        commit_settings(
            &settings,
            Some(CredentialEvent {
                at: Utc::now().timestamp(),
                account: None,
                fingerprint: None,
                expires_at: None,
                kind: "pause".into(),
            }),
        )?;
    }
    let restored = restore_login_shadow()?;
    append_log(if restored {
        "rotation paused; removed settings OAuth token and restored saved /login credential"
    } else {
        "rotation paused; removed settings OAuth token and restored /login precedence"
    });
    Ok(())
}

pub async fn resume(config: &Config) -> Result<RotationOutcome> {
    {
        let _lock = RotationLock::acquire()?;
        let path = pause_path()?;
        if path.exists() {
            std::fs::remove_file(&path)
                .with_context(|| format!("failed to remove {}", path.display()))?;
        }
        append_log("rotation resumed");
    }
    rotate(
        config,
        RotateOptions {
            // Preserve an explicitly selected healthy credential. `resume`
            // should restart monitoring, not unexpectedly rebalance sessions.
            force: false,
            ..RotateOptions::default()
        },
    )
    .await
}

/// Per-account retry schedule for refreshes that failed transiently. Held in
/// the daemon's memory: a restart retrying at once is the right behavior.
#[derive(Debug, Default)]
pub struct RefreshBackoff {
    accounts: BTreeMap<String, (u32, f64)>,
}

impl RefreshBackoff {
    fn allows(&self, account: &str, now: f64) -> bool {
        self.accounts
            .get(account)
            .is_none_or(|(_, next_attempt)| now >= *next_attempt)
    }

    /// Record a failure; returns true for the first in a streak.
    fn fail(&mut self, account: &str, now: f64) -> bool {
        let entry = self.accounts.entry(account.to_owned()).or_insert((0, 0.0));
        entry.0 = entry.0.saturating_add(1);
        entry.1 = now + (60.0 * 2f64.powi(entry.0.min(4) as i32 - 1)).min(600.0);
        entry.0 == 1
    }

    fn clear(&mut self, account: &str) {
        self.accounts.remove(account);
    }
}

/// Renew every login grant that is close to expiry, then make sure the
/// default account's settings entry offers its newest access token.
///
/// Refresh tokens are single-use, so this runs under the rotation lock and
/// re-reads each account under the config lock immediately before redeeming:
/// whichever tokeman process gets there first wins, and the others see a
/// fresh token and skip it. The new grant is saved before anything else
/// happens, because losing it would lose the account.
pub async fn refresh_logins(backoff: &mut RefreshBackoff) -> Result<usize> {
    let now = now_epoch();
    let now_ms = Utc::now().timestamp_millis();
    let due: Vec<String> = Config::load()?
        .tokens
        .iter()
        .filter(|token| token.needs_refresh(now_ms))
        .filter(|token| backoff.allows(&token.name, now))
        .map(|token| token.name.clone())
        .collect();
    if due.is_empty() {
        return Ok(0);
    }

    let _lock = RotationLock::acquire()?;
    let mut changed = 0;
    for account in due {
        let (mut config, config_lock) = Config::load_locked()?;
        let Some(token) = config.tokens.iter_mut().find(|token| token.name == account) else {
            continue;
        };
        if !token.needs_refresh(Utc::now().timestamp_millis()) {
            continue;
        }
        let Some(refresh_token) = token.refresh_token.clone() else {
            continue;
        };
        match crate::claude_login::refresh(&refresh_token).await {
            Ok(bundle) => {
                token.apply_login(bundle, Utc::now().timestamp_millis());
                config.save(&config_lock)?;
                backoff.clear(&account);
                changed += 1;
            }
            Err(error) if crate::claude_login::is_permanent_refresh_failure(&error) => {
                let fallback = if token.setup_key().is_some() {
                    "its setup token"
                } else {
                    "nothing"
                };
                token.login_error = Some(compact_error(&format!("{error:#}")));
                config.save(&config_lock)?;
                changed += 1;
                append_log(&format!(
                    "ERROR login for {account} was refused permanently ({}); it falls back to {fallback} until `tokeman login {account}`",
                    compact_error(&format!("{error:#}")),
                ));
            }
            Err(error) => {
                if backoff.fail(&account, now) {
                    append_log(&format!(
                        "login refresh for {account} failed; retrying with backoff ({})",
                        compact_error(&format!("{error:#}"))
                    ));
                }
            }
        }
    }

    if changed > 0 {
        let config = Config::load()?;
        let mut settings = load_claude_settings()?;
        let default = configured_token(&settings)
            .and_then(|value| token_name_for_value(&config.tokens, Some(value)))
            .and_then(|account| config.tokens.iter().find(|token| token.name == account));
        if let Some(token) = default
            && let Some(kind) = reinstall_reason(&settings, token)
        {
            let event = install_credential(&mut settings, token, kind)?;
            commit_settings(&settings, Some(event))?;
            if kind != "refresh" && kind != "repair" {
                append_log(&format!(
                    "{kind}: {} now offers {}",
                    token.name,
                    describe_credential(token)
                ));
            }
        }
    }
    Ok(changed)
}

/// Persistent service loop. launchd supervises this one process instead of
/// spawning a fresh helper every 20 seconds (which can be delayed by xpcproxy).
/// Configuration is reloaded on every wake so policy/token edits take effect
/// without restarting the service.
pub async fn daemon() -> Result<()> {
    let startup_executable = executable_identity();
    let mut wake = tokio::time::interval(std::time::Duration::from_secs(20));
    wake.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut next_codex_sweep = std::time::Instant::now();
    let mut refresh_backoff = RefreshBackoff::default();
    let mut codex_needs_login = std::collections::BTreeSet::new();
    loop {
        wake.tick().await;
        if startup_executable.is_some()
            && executable_identity().is_some_and(|current| Some(current) != startup_executable)
        {
            append_log("daemon executable was replaced; exiting so launchd loads the new binary");
            return Ok(());
        }
        if !pause_path().is_ok_and(|path| path.exists())
            && let Err(error) = refresh_logins(&mut refresh_backoff).await
        {
            append_log(&format!("ERROR login refresh sweep failed: {error:#}"));
        }
        let config = match Config::load() {
            Ok(config) => config,
            Err(error) => {
                append_log(&format!("ERROR config reload failed: {error:#}"));
                eprintln!("tokeman config reload failed: {error:#}");
                continue;
            }
        };
        let scheduled = RotateOptions {
            scheduled: true,
            ..RotateOptions::default()
        };
        match rotate(&config, scheduled).await {
            Ok(outcome) if outcome.changed || outcome.action == "no-viable-token" => {
                println!("{}", outcome.message);
            }
            Ok(_) => {}
            Err(error) => {
                append_log(&format!("ERROR rotation cycle failed: {error:#}"));
                eprintln!("tokeman rotation cycle failed: {error:#}");
            }
        }

        if std::time::Instant::now() >= next_codex_sweep {
            next_codex_sweep = std::time::Instant::now() + CODEX_SWEEP_INTERVAL;
            codex_maintenance(&config, &mut codex_needs_login).await;
        }
    }
}

/// Codex accounts only need attention on the order of days, so sweeping every
/// half hour is ample and keeps the request rate far below anything the edge
/// would treat as abusive.
const CODEX_SWEEP_INTERVAL: std::time::Duration = std::time::Duration::from_secs(30 * 60);

/// Keep every configured Codex profile refreshed.
///
/// This exists because a parked profile rots: once its refresh token ages out
/// the account is dead and needs an interactive re-login, and the failure is
/// silent — `codex login status` still says "Logged in using ChatGPT" for a
/// profile the backend rejects. Refreshing on a schedule is what keeps a
/// standby account actually available when rotation reaches for it.
///
/// Never propagates errors: Codex upkeep must not be able to take down the
/// Claude rotation loop this daemon primarily exists to run.
async fn codex_maintenance(config: &Config, needs_login: &mut std::collections::BTreeSet<String>) {
    use crate::openai::refresh;
    if config.codex_accounts.is_empty() {
        return;
    }

    // Age alone is not a sufficient trigger. A session revoked elsewhere — the
    // user signing out on another device — leaves `last_refresh` looking recent
    // while the backend already rejects the token, so we ask the backend rather
    // than trusting the clock.
    let probes = crate::openai::probe::probe_all(&config.codex_accounts).await;
    let rejected: std::collections::HashSet<&str> = probes
        .iter()
        .filter(|result| result.unauthorized)
        .map(|result| result.account_name.as_str())
        .collect();

    for account in &config.codex_accounts {
        // An access-token-only account has no refresh material; attempting a
        // refresh would just fail noisily. It dies when its token expires and
        // that is expected, not a fault to log every half hour.
        if account.access_only {
            continue;
        }
        let home = account.home();
        let force = rejected.contains(account.name.as_str());
        let outcome = refresh::refresh(&home, force, refresh::DEFAULT_MAX_AGE_DAYS).await;
        // A permanent failure is a standing condition: say it once when it
        // starts and once when it clears, not every half hour until then.
        let permanent = outcome
            .as_ref()
            .err()
            .and_then(refresh::failure_kind)
            .filter(|failure| failure.is_permanent());
        if permanent.is_none() && needs_login.remove(&account.name) {
            append_log(&format!("codex: {} is usable again", account.name));
        }
        match (outcome, permanent) {
            (Ok(Some(_)), _) => append_log(&format!("codex: refreshed {}", account.name)),
            (Ok(None), _) => {}
            (Err(_), Some(failure)) => {
                if needs_login.insert(account.name.clone()) {
                    append_log(&format!(
                        "codex: {} needs an interactive `CODEX_HOME={} codex login` ({})",
                        account.name,
                        home.path().display(),
                        failure.label(),
                    ));
                }
            }
            (Err(error), None) => append_log(&format!(
                "codex: refresh failed for {}: {error:#}",
                account.name
            )),
        }
    }
}

/// Explicitly make one configured token Claude Code's shared credential.
///
/// Existing processes may retain their launch credential. The token never
/// appears in terminal commands, scrollback, or AppleScript source.
pub fn activate_token(config: &Config, token_name: &str) -> Result<()> {
    let _lock = RotationLock::acquire()?;
    let token = config
        .tokens
        .iter()
        .find(|token| token.name == token_name)
        .with_context(|| format!("token '{token_name}' is not configured"))?;
    if suppress_login_shadow(false)? {
        append_log("suppressed short-lived /login Keychain auth before explicit activation");
    }
    let mut settings = load_claude_settings()?;
    let event = install_credential(&mut settings, token, "activate")?;
    commit_settings(&settings, Some(event))?;
    append_log(&format!("explicitly activated {token_name}"));
    Ok(())
}

fn remove_observer_hook(settings: &mut Map<String, Value>) -> bool {
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
                .is_some_and(admission::is_observer_hook)
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

fn install_observer_hook(settings: &mut Map<String, Value>, binary: &Path) {
    let _ = remove_observer_hook(settings);
    if !settings.get("hooks").is_some_and(Value::is_object) {
        settings.insert("hooks".into(), Value::Object(Map::new()));
    }
    let hooks = settings
        .get_mut("hooks")
        .and_then(Value::as_object_mut)
        .expect("hooks was just initialized");
    if !hooks.get("SessionStart").is_some_and(Value::is_array) {
        hooks.insert("SessionStart".into(), Value::Array(Vec::new()));
    }
    hooks
        .get_mut("SessionStart")
        .and_then(Value::as_array_mut)
        .expect("SessionStart was just initialized")
        .push(json!({
            "hooks": [{
                "type": "command",
                "command": admission::hook_command(binary),
                "timeout": 5
            }]
        }));
}

pub fn install_service() -> Result<PathBuf> {
    #[cfg(not(target_os = "macos"))]
    bail!("automatic rotation service installation is currently supported on macOS");

    #[cfg(target_os = "macos")]
    {
        // Stop the old daemon before waiting for the rotation lock. A probe owns
        // that lock for up to its network timeout, and a 20-second service loop
        // can otherwise reacquire it often enough to starve an installer.
        let plist_path = launch_agent_path()?;
        let domain = launchctl_domain()?;
        if plist_path.exists() {
            let _ = Command::new("/bin/launchctl")
                .args(["bootout", &domain, &plist_path.to_string_lossy()])
                .status();
        }
        let _lock = RotationLock::acquire()?;
        let binary = std::env::current_exe()?.canonicalize()?;
        let home = dirs::home_dir().context("could not find home directory")?;
        let launcher_path = daemon_launcher_path()?;
        let log = launchd_log_path()?;
        ensure_private_parent(&plist_path)?;
        ensure_private_parent(&launcher_path)?;
        ensure_private_parent(&log)?;
        // `cargo install` copies/strips the linker-signed Mach-O after linking.
        // On newer macOS releases that can invalidate the ad-hoc signature and
        // taskgated SIGKILLs the replacement before Rust's main() runs. Keep
        // launchd pointed at this stable shell shim so every daemon restart
        // repairs an invalid ad-hoc signature before executing the new binary.
        let launcher = r#"#!/bin/sh
set -eu
binary=$1
if ! /usr/bin/codesign --verify --strict "$binary" >/dev/null 2>&1; then
  /usr/bin/codesign --force --sign - --timestamp=none "$binary"
fi
exec "$binary" rotate daemon
"#;
        write_atomic(&launcher_path, launcher.as_bytes(), 0o700)?;

        let config = Config::load()?;
        if suppress_login_shadow(false)? {
            append_log("suppressed short-lived /login Keychain auth during service install");
        }
        let mut settings = load_claude_settings()?;
        let managed = configured_token(&settings)
            .and_then(|value| token_name_for_value(&config.tokens, Some(value)))
            .and_then(|account| config.tokens.iter().find(|token| token.name == account));
        let event = match managed {
            Some(token) if token.credential(Utc::now().timestamp_millis()).is_some() => {
                Some(install_credential(&mut settings, token, "install")?)
            }
            _ => None,
        };
        install_observer_hook(&mut settings, &binary);
        commit_settings(&settings, event)?;

        let plist = format!(
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
  <key>ProcessType</key><string>Background</string>
  <key>LowPriorityIO</key><true/>
  <key>StandardOutPath</key><string>{log}</string>
  <key>StandardErrorPath</key><string>{log}</string>
  <key>Umask</key><integer>63</integer>
</dict>
</plist>
"#,
            label = SERVICE_LABEL,
            launcher = xml_escape(&launcher_path.to_string_lossy()),
            binary = xml_escape(&binary.to_string_lossy()),
            home = xml_escape(&home.to_string_lossy()),
            path = xml_escape(&format!(
                "{}/.cargo/bin:/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin",
                home.display()
            )),
            log = xml_escape(&log.to_string_lossy()),
        );
        write_atomic(&plist_path, plist.as_bytes(), 0o644)?;
        let plist_status = Command::new("/usr/bin/plutil")
            .args(["-lint", &plist_path.to_string_lossy()])
            .status()?;
        if !plist_status.success() {
            bail!("generated LaunchAgent plist failed validation");
        }

        run_launchctl(["bootstrap", &domain, &plist_path.to_string_lossy()])?;
        run_launchctl(["enable", &format!("{domain}/{SERVICE_LABEL}")])?;
        run_launchctl(["kickstart", "-k", &format!("{domain}/{SERVICE_LABEL}")])?;
        append_log(&format!(
            "installed rotation service using {}",
            binary.display()
        ));
        Ok(plist_path)
    }
}

pub fn uninstall_service() -> Result<()> {
    #[cfg(not(target_os = "macos"))]
    bail!("automatic rotation service installation is currently supported on macOS");

    #[cfg(target_os = "macos")]
    {
        let _lock = RotationLock::acquire()?;
        let plist = launch_agent_path()?;
        if plist.exists() {
            let domain = launchctl_domain()?;
            let _ = Command::new("/bin/launchctl")
                .args(["bootout", &domain, &plist.to_string_lossy()])
                .status();
            std::fs::remove_file(&plist)
                .with_context(|| format!("failed to remove {}", plist.display()))?;
        }
        let launcher = daemon_launcher_path()?;
        if launcher.exists() {
            std::fs::remove_file(&launcher)
                .with_context(|| format!("failed to remove {}", launcher.display()))?;
        }
        let mut settings = load_claude_settings()?;
        if remove_observer_hook(&mut settings) {
            save_claude_settings(&settings)?;
        }
        if restore_login_shadow()? {
            append_log("restored saved /login credential during service uninstall");
        }
        append_log("uninstalled rotation service");
        Ok(())
    }
}

#[cfg(target_os = "macos")]
fn launchctl_domain() -> Result<String> {
    let output = Command::new("/usr/bin/id").arg("-u").output()?;
    if !output.status.success() {
        bail!("failed to determine user id for launchctl");
    }
    Ok(format!("gui/{}", String::from_utf8(output.stdout)?.trim()))
}

#[cfg(target_os = "macos")]
fn run_launchctl<const N: usize>(args: [&str; N]) -> Result<()> {
    let status = Command::new("/bin/launchctl").args(args).status()?;
    if status.success() {
        Ok(())
    } else {
        Err(anyhow!("launchctl exited with {status}"))
    }
}

#[cfg(target_os = "macos")]
fn xml_escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

#[cfg(test)]
mod tests {
    use chrono::Utc;

    use super::*;
    use crate::probe::{ModelUsage, RateLimits, UnifiedQuota, Window};

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

        assert_eq!(split_claude_login(&mut document).unwrap(), Some(login));
        assert!(document.get("claudeAiOauth").is_none());
        assert_eq!(
            document.pointer("/mcpOAuth/linear/accessToken"),
            Some(&json!("mcp-secret"))
        );
        assert_eq!(document.pointer("/futureField/value"), Some(&json!(7)));
        assert_eq!(split_claude_login(&mut document).unwrap(), None);
    }

    #[test]
    fn keychain_restore_never_overwrites_a_newer_manual_login() {
        let mut without_login = json!({"mcpOAuth": {"linear": {}}});
        assert!(restore_claude_login(&mut without_login, json!({"accessToken": "saved"})).unwrap());
        assert_eq!(
            without_login.pointer("/claudeAiOauth/accessToken"),
            Some(&json!("saved"))
        );
        assert!(
            !restore_claude_login(&mut without_login, json!({"accessToken": "older"})).unwrap()
        );
        assert_eq!(
            without_login.pointer("/claudeAiOauth/accessToken"),
            Some(&json!("saved"))
        );
    }

    #[test]
    fn keychain_transform_rejects_non_object_documents() {
        let mut document = json!(["not", "an", "object"]);
        assert!(split_claude_login(&mut document).is_err());
        assert!(restore_claude_login(&mut document, json!({})).is_err());
    }

    fn result(name: &str, five_used: f64, seven_used: f64) -> ProbeResult {
        ProbeResult {
            token_name: name.into(),
            probed_at: Utc::now(),
            quota: Some(UnifiedQuota {
                status: "allowed".into(),
                reset: 0,
                representative_claim: "five_hour".into(),
                fallback: None,
                session: Some(Window {
                    utilization: five_used,
                    reset: 0,
                }),
                weekly: Some(Window {
                    utilization: seven_used,
                    reset: 0,
                }),
                overage_status: None,
                overage: None,
                overage_disabled_reason: None,
                fable: None,
                overage_in_use: false,
            }),
            model_usage: None,
            model_usage_error: None,
            rate_limits: RateLimits {
                requests_limit: None,
                requests_remaining: None,
                input_tokens_limit: None,
                input_tokens_remaining: None,
                output_tokens_limit: None,
                output_tokens_remaining: None,
            },
            error: None,
        }
    }

    fn result_with_resets(
        name: &str,
        five_used: f64,
        seven_used: f64,
        five_reset_delay: i64,
        seven_reset_delay: i64,
    ) -> ProbeResult {
        let mut result = result(name, five_used, seven_used);
        let probed_at = 1_800_000_000;
        result.probed_at = chrono::DateTime::from_timestamp(probed_at, 0).unwrap();
        let quota = result.quota.as_mut().unwrap();
        quota.session.as_mut().unwrap().reset = probed_at + five_reset_delay;
        quota.weekly.as_mut().unwrap().reset = probed_at + seven_reset_delay;
        result
    }

    fn result_with_opus(
        name: &str,
        five_used: f64,
        seven_used: f64,
        opus_used: f64,
    ) -> ProbeResult {
        let mut result = result(name, five_used, seven_used);
        result.model_usage = Some(ModelUsage {
            opus_weekly: Some(Window {
                utilization: opus_used,
                reset: 0,
            }),
            sonnet_weekly: None,
            opus_source: Some(ModelUsageSource::Profile),
            sonnet_source: None,
            scoped_weekly: Vec::new(),
        });
        result
    }

    fn result_with_scoped_bucket(
        name: &str,
        bucket_key: &str,
        bucket_label: &str,
        used: f64,
    ) -> ProbeResult {
        let mut result = result(name, 0.0, 0.0);
        result.model_usage = Some(ModelUsage {
            scoped_weekly: vec![ModelQuotaBucket {
                key: bucket_key.into(),
                label: bucket_label.into(),
                window: Window {
                    utilization: used,
                    reset: 0,
                },
                source: ModelUsageSource::Profile,
            }],
            ..ModelUsage::default()
        });
        result
    }

    #[test]
    fn exact_normal_floor_is_not_viable() {
        let policy = RotationSettings::default();
        assert!(!is_viable(
            &result("at-floor", 0.90, 0.0),
            RotationMode::Normal,
            &policy,
            Some("opus")
        ));
        assert!(!is_viable(
            &result("weekly-floor", 0.0, 0.95),
            RotationMode::Normal,
            &policy,
            Some("opus")
        ));
    }

    #[test]
    fn known_opus_floor_is_part_of_shared_default_viability() {
        let policy = RotationSettings::default();
        // Derive the boundary from the policy rather than pinning it to a
        // literal, so this keeps testing the boundary when the floor moves.
        let floor = policy.normal_min_7d_remaining;
        assert!(!is_viable(
            &result_with_opus("opus-wall", 0.0, 0.0, 1.0 - floor),
            RotationMode::Normal,
            &policy,
            Some("opus")
        ));
        assert!(is_viable(
            &result_with_opus("opus-room", 0.0, 0.0, 1.0 - floor - 0.01),
            RotationMode::Normal,
            &policy,
            Some("opus")
        ));
    }

    #[test]
    fn exhausted_opus_48_bucket_does_not_poison_opus_5() {
        let policy = RotationSettings::default();
        let result = result_with_scoped_bucket("separate-models", "opus48", "Opus 4.8", 1.0);
        let viable = |model| is_viable(&result, RotationMode::Normal, &policy, Some(model));
        assert!(viable("opus"));
        assert!(!viable("claude-opus-4-8"));
        // Fable is its own family with its own limit, not Opus 4.8.
        assert!(viable("claude-fable-5-1"));
    }

    #[test]
    fn the_fable_limit_gates_fable_sessions_only() {
        let policy = RotationSettings::default();
        let viable = |result: &ProbeResult, model| {
            is_viable(result, RotationMode::Normal, &policy, Some(model))
        };
        // Profile row: a major-less "Fable" bucket covers every Fable release.
        let profile = result_with_scoped_bucket("profile", "fable", "Fable", 1.0);
        assert!(!viable(&profile, "claude-fable-5-1"));
        assert!(!viable(&profile, "fable"));
        assert!(viable(&profile, "opus"));

        // Header form (`7d_oi`) on an account without profile usage.
        let mut headers = result("headers", 0.1, 0.1);
        headers.quota.as_mut().unwrap().fable = Some(Window {
            utilization: 1.0,
            reset: 0,
        });
        assert!(!viable(&headers, "claude-fable-5-1"));
        assert!(viable(&headers, "claude-opus-5"));
    }

    #[test]
    fn configured_opus_alias_obeys_opus_5_bucket() {
        let policy = RotationSettings::default();
        let result = result_with_scoped_bucket("opus-five", "claudeopus5", "Opus 5", 0.95);
        assert!(!is_viable(
            &result,
            RotationMode::Normal,
            &policy,
            Some("opus[1m]")
        ));
        assert_eq!(
            family_version("claude-opus-5-20251001")
                .and_then(|model| model.version)
                .as_deref(),
            Some("5")
        );
    }

    #[test]
    fn unrelated_family_bucket_does_not_affect_selection() {
        let policy = RotationSettings::default();
        let mut result = result_with_opus("family-split", 0.0, 0.0, 0.10);
        let usage = result.model_usage.as_mut().unwrap();
        usage.sonnet_weekly = Some(Window {
            utilization: 1.0,
            reset: 0,
        });
        usage.sonnet_source = Some(ModelUsageSource::Profile);
        assert!(is_viable(
            &result,
            RotationMode::Normal,
            &policy,
            Some("opus")
        ));
        assert!(!is_viable(
            &result,
            RotationMode::Normal,
            &policy,
            Some("sonnet")
        ));
    }

    #[test]
    fn replacement_landing_score_includes_known_opus_capacity() {
        let policy = RotationSettings::default();
        let results = vec![
            result_with_opus("general-room-opus-tight", 0.0, 0.0, 0.80),
            result_with_opus("balanced-for-opus", 0.10, 0.10, 0.10),
        ];
        assert_eq!(
            choose_best(&results, RotationMode::Normal, &policy, Some("opus"))
                .map(|result| result.token_name.as_str()),
            Some("balanced-for-opus")
        );
    }

    #[test]
    fn sip_mode_unlocks_the_reserve_band() {
        let policy = RotationSettings::default();
        let results = vec![
            result("five-hour-scrap", 0.91, 0.94),
            result("weekly-scrap", 0.97, 0.96),
        ];
        assert_eq!(
            mode_for(&results, &policy, Some("opus")),
            RotationMode::SipAndDrain
        );
        assert_eq!(
            choose_best(&results, RotationMode::SipAndDrain, &policy, Some("opus"),)
                .map(|result| result.token_name.as_str()),
            Some("five-hour-scrap")
        );
    }

    #[test]
    fn exhausted_fleet_stays_in_fast_sip_mode() {
        let policy = RotationSettings::default();
        let results = vec![
            result("weekly-empty", 0.20, 1.0),
            result("five-hour-empty", 1.0, 0.20),
        ];
        assert_eq!(
            mode_for(&results, &policy, Some("opus")),
            RotationMode::SipAndDrain
        );
        assert!(choose_best(&results, RotationMode::SipAndDrain, &policy, Some("opus")).is_none());
        assert_eq!(
            interval_secs(mode_for(&results, &policy, Some("opus")), &policy),
            policy.sip_probe_interval_secs
        );
    }

    #[test]
    fn sip_floors_are_hard_boundaries() {
        let policy = RotationSettings::default();
        assert!(!is_viable(
            &result("five-hour-floor", 0.98, 0.50),
            RotationMode::SipAndDrain,
            &policy,
            Some("opus")
        ));
        assert!(!is_viable(
            &result("weekly-floor", 0.50, 0.97),
            RotationMode::SipAndDrain,
            &policy,
            Some("opus")
        ));
        assert!(is_viable(
            &result("scraps", 0.979, 0.969),
            RotationMode::SipAndDrain,
            &policy,
            Some("opus")
        ));
    }

    #[test]
    fn replacement_prefers_long_balanced_dwell() {
        let policy = RotationSettings::default();
        let results = vec![
            result("short-five", 0.70, 0.15),
            result("balanced", 0.14, 0.14),
            result("short-weekly", 0.05, 0.77),
        ];
        assert_eq!(
            choose_best(&results, RotationMode::Normal, &policy, Some("opus"))
                .map(|result| result.token_name.as_str()),
            Some("balanced")
        );
    }

    #[test]
    fn replacement_uses_soonest_limiting_reset_as_a_headroom_tiebreaker() {
        let policy = RotationSettings::default();
        let results = vec![
            result_with_resets("same-dwell-later", 0.10, 0.20, 900, 3_600),
            result_with_resets("same-dwell-first", 0.10, 0.20, 900, 600),
        ];
        assert_eq!(
            choose_best(&results, RotationMode::Normal, &policy, Some("opus"))
                .map(|result| result.token_name.as_str()),
            Some("same-dwell-first")
        );
    }

    #[test]
    fn replacement_headroom_beats_an_earlier_reset_for_cold_cache_landing() {
        let policy = RotationSettings::default();
        let results = vec![
            result_with_resets("roomy", 0.08, 0.01, 3_600, 86_400),
            result_with_resets("soon-but-tight", 0.06, 0.54, 600, 600),
        ];
        assert_eq!(
            choose_best(&results, RotationMode::Normal, &policy, Some("opus"))
                .map(|result| result.token_name.as_str()),
            Some("roomy")
        );
    }

    #[test]
    fn replacement_uses_the_limiting_windows_reset() {
        let policy = RotationSettings::default();
        let results = vec![
            // Its 5h reset is near, but weekly capacity is the actual
            // constraint and does not reset for two hours.
            result_with_resets("non-limiting-reset", 0.00, 0.50, 60, 7_200),
            // Five-hour capacity is limiting and resets in ten minutes.
            result_with_resets("limiting-reset", 0.40, 0.10, 600, 9_000),
        ];
        assert_eq!(
            choose_best(&results, RotationMode::Normal, &policy, Some("opus"))
                .map(|result| result.token_name.as_str()),
            Some("limiting-reset")
        );
    }

    #[test]
    fn degraded_probe_uses_fast_retry_cadence() {
        let policy = RotationSettings::default();
        let active = result("active", 0.10, 0.10);
        assert_eq!(
            next_probe_interval_secs(RotationMode::Normal, Some(&active), false, &policy),
            policy.sip_probe_interval_secs
        );
    }

    #[test]
    fn approaching_floor_uses_fast_retry_cadence() {
        let policy = RotationSettings::default();
        let active = result("active", 0.80, 0.20);
        assert_eq!(
            next_probe_interval_secs(RotationMode::Normal, Some(&active), true, &policy),
            policy.sip_probe_interval_secs
        );
    }

    #[test]
    fn healthy_complete_probe_uses_normal_cadence() {
        let policy = RotationSettings::default();
        let active = result("active", 0.20, 0.20);
        assert_eq!(
            next_probe_interval_secs(RotationMode::Normal, Some(&active), true, &policy),
            policy.normal_probe_interval_secs
        );
    }

    #[test]
    fn error_result_is_not_an_authoritative_quota_reading() {
        let mut failed = result("failed", 0.0, 0.0);
        failed.error = Some("transient transport failure".into());
        assert!(!has_quota_reading(&failed));
        assert!(!probe_is_complete(&[failed], 1));
    }

    #[test]
    fn partial_fleet_view_cannot_enter_sip_and_drain() {
        let policy = RotationSettings::default();
        let partial = vec![result("only-response", 0.95, 0.96)];
        assert_eq!(
            safe_mode_for(&partial, 2, &policy, RotationMode::Normal, Some("opus")),
            RotationMode::Normal
        );
        assert_eq!(
            safe_mode_for(
                &partial,
                2,
                &policy,
                RotationMode::SipAndDrain,
                Some("opus"),
            ),
            RotationMode::SipAndDrain
        );
    }

    #[test]
    fn missing_active_probe_forces_a_sticky_hold() {
        assert!(active_probe_is_degraded(Some("active"), None));
        assert!(!active_probe_is_degraded(None, None));
        let active = result("active", 0.20, 0.20);
        assert!(!active_probe_is_degraded(Some("active"), Some(&active)));
    }

    #[test]
    fn hard_auth_failure_is_definitive_not_transient() {
        let mut expired = result("expired", 0.0, 0.0);
        expired.quota = None;
        expired.error = Some("HTTP 401 Unauthorized: OAuth access token has expired".into());
        assert!(is_hard_auth_failure(&expired));
        assert!(probe_is_complete(std::slice::from_ref(&expired), 1));
        assert!(!active_probe_is_degraded(Some("expired"), Some(&expired)));
        assert!(!is_viable(
            &expired,
            RotationMode::Normal,
            &RotationSettings::default(),
            Some("opus")
        ));
    }

    #[test]
    fn parses_default_changes_without_treating_other_log_lines_as_events() {
        let explicit =
            parse_default_event("2026-07-26T22:23:09Z explicitly activated boot@example.com")
                .unwrap();
        assert_eq!(explicit.account.as_deref(), Some("boot@example.com"));

        let switched = parse_default_event(
            "2026-07-26T22:23:19Z switch boot@example.com -> ember@example.com; threshold reached",
        )
        .unwrap();
        assert_eq!(switched.account.as_deref(), Some("ember@example.com"));

        let paused = parse_default_event(
            "2026-07-26T22:24:00Z rotation paused; removed settings OAuth token",
        )
        .unwrap();
        assert_eq!(paused.account, None);
        assert!(parse_default_event("2026-07-26T22:25:00Z kept ember@example.com").is_none());
    }

    #[test]
    fn recognizes_only_real_claude_process_rows() {
        let session = parse_ps_session("40102 ttys016 Sun Jul 26 18:23:11 2026 claude").unwrap();
        assert_eq!(session.0, 40102);
        assert_eq!(session.1.as_deref(), Some("ttys016"));
        assert!(
            parse_ps_session("5407 ?? Sun Jul 26 18:23:11 2026 /usr/bin/claude_resident").is_none()
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

        let settings_value = Value::Object(settings.clone());
        let commands = settings_value
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
            })
            .filter_map(|hook| hook.get("command").and_then(Value::as_str))
            .collect::<Vec<_>>();
        assert_eq!(
            commands
                .iter()
                .filter(|command| admission::is_observer_hook(command))
                .count(),
            1
        );
        assert!(
            commands
                .iter()
                .any(|command| command.contains("postmark-nudge"))
        );

        assert!(remove_observer_hook(&mut settings));
        let settings_value = Value::Object(settings);
        let remaining = settings_value
            .pointer("/hooks/SessionStart")
            .and_then(Value::as_array)
            .unwrap();
        assert_eq!(remaining.len(), 1);
    }

    fn load(pairs: &[(&str, usize)]) -> BTreeMap<String, usize> {
        pairs
            .iter()
            .map(|(name, count)| ((*name).to_string(), *count))
            .collect()
    }

    #[test]
    fn bound_sessions_break_a_tie_between_equally_roomy_accounts() {
        let policy = RotationSettings::default();
        let results = vec![result("busy", 0.10, 0.10), result("idle", 0.10, 0.10)];
        // Load-blind, the tie falls to the first name alphabetically.
        assert_eq!(
            choose_best(&results, RotationMode::Normal, &policy, None)
                .unwrap()
                .token_name,
            "busy"
        );
        // Three sessions are already draining `busy`, so `idle` is the better
        // landing zone even though the probe shows identical headroom.
        assert_eq!(
            choose_best_with_load(
                &results,
                RotationMode::Normal,
                &policy,
                None,
                &load(&[("busy", 3)]),
            )
            .unwrap()
            .token_name,
            "idle"
        );
    }

    #[test]
    fn the_reserve_never_rescues_or_rejects_a_candidate() {
        let policy = RotationSettings::default();
        // `full` is past the 7d floor; no amount of idleness makes it viable.
        let results = vec![result("full", 0.10, 0.99), result("open", 0.10, 0.10)];
        assert_eq!(
            choose_best_with_load(
                &results,
                RotationMode::Normal,
                &policy,
                None,
                &load(&[("open", 40)]),
            )
            .unwrap()
            .token_name,
            "open"
        );
        // And a lone viable account stays chosen no matter how loaded it is.
        let only = vec![result("open", 0.10, 0.10)];
        assert_eq!(
            choose_best_with_load(
                &only,
                RotationMode::Normal,
                &policy,
                None,
                &load(&[("open", 99)]),
            )
            .unwrap()
            .token_name,
            "open"
        );
    }

    #[test]
    fn a_reserve_of_zero_reproduces_load_blind_selection() {
        let mut policy = RotationSettings::default();
        policy.per_session_reserve = 0.0;
        let results = vec![result("busy", 0.10, 0.10), result("idle", 0.10, 0.10)];
        assert_eq!(
            choose_best_with_load(
                &results,
                RotationMode::Normal,
                &policy,
                None,
                &load(&[("busy", 5)]),
            )
            .unwrap()
            .token_name,
            "busy"
        );
    }

    #[test]
    fn abandoned_drain_reports_only_bound_non_default_accounts_below_floor() {
        let policy = RotationSettings::default();
        let results = vec![
            result("default", 0.99, 0.99),
            result("drained", 0.99, 0.99),
            result("spare", 0.99, 0.99),
            result("healthy", 0.10, 0.10),
        ];
        let drains = abandoned_drains(
            &results,
            Some("default"),
            &load(&[("default", 2), ("drained", 3), ("healthy", 4)]),
            RotationMode::Normal,
            &policy,
            None,
        );
        // `default` is excluded (rotation handles it), `spare` has no bound
        // sessions, `healthy` is above the floor. Only `drained` qualifies.
        assert_eq!(drains.len(), 1);
        assert_eq!(drains[0].account, "drained");
        assert_eq!(drains[0].bound_sessions, 3);
        assert!(drains[0].summary().contains("3 live session(s)"));
    }

    #[test]
    fn abandoned_drain_ignores_accounts_with_no_quota_reading() {
        let policy = RotationSettings::default();
        let mut broken = result("broken", 0.99, 0.99);
        broken.quota = None;
        broken.error = Some("HTTP 401 Unauthorized".into());
        let drains = abandoned_drains(
            &results_of(vec![broken]),
            Some("default"),
            &load(&[("broken", 2)]),
            RotationMode::Normal,
            &policy,
            None,
        );
        assert!(drains.is_empty());
    }

    fn results_of(results: Vec<ProbeResult>) -> Vec<ProbeResult> {
        results
    }

    fn auth_failure(name: &str, code: &str) -> ProbeResult {
        let mut r = result(name, 0.0, 0.0);
        r.quota = None;
        r.error = Some(format!("HTTP {code} Forbidden"));
        r
    }

    #[test]
    fn hard_auth_failures_back_off_and_successes_clear() {
        let mut backoff = BTreeMap::new();
        let probed: std::collections::HashSet<String> = ["dead".to_string(), "live".to_string()]
            .into_iter()
            .collect();
        let results = vec![auth_failure("dead", "401"), result("live", 0.1, 0.1)];

        let parked = update_auth_backoff(&mut backoff, &results, &probed, 1_000.0);
        assert_eq!(parked, vec!["dead".to_string()]);
        let entry = backoff.get("dead").expect("parked");
        assert_eq!(entry.failures, 1);
        assert_eq!(entry.next_attempt_epoch, 1_000.0 + 300.0);
        assert!(!backoff.contains_key("live"));

        // Repeated refusals lengthen the wait, and only the first is announced.
        let parked = update_auth_backoff(&mut backoff, &results, &probed, 2_000.0);
        assert!(parked.is_empty());
        assert_eq!(backoff["dead"].failures, 2);
        assert_eq!(backoff["dead"].next_attempt_epoch, 2_000.0 + 600.0);

        // A later success releases it immediately.
        update_auth_backoff(&mut backoff, &[result("dead", 0.1, 0.1)], &probed, 3_000.0);
        assert!(backoff.is_empty());
    }

    #[test]
    fn transient_failures_never_back_off() {
        let mut backoff = BTreeMap::new();
        let probed: std::collections::HashSet<String> = ["flaky".to_string()].into_iter().collect();
        let mut timeout = result("flaky", 0.0, 0.0);
        timeout.quota = None;
        timeout.error = Some("error sending request for url (https://api.anthropic.com)".into());
        update_auth_backoff(&mut backoff, &[timeout], &probed, 1_000.0);
        assert!(
            backoff.is_empty(),
            "a network error is a hiccup, not a refusal -- retry it immediately"
        );
    }

    #[test]
    fn a_skipped_account_is_not_credited_with_a_failure() {
        let mut backoff = BTreeMap::new();
        backoff.insert(
            "dead".to_string(),
            AuthBackoff {
                failures: 4,
                next_attempt_epoch: 9_999.0,
                last_error: "HTTP 401 Unauthorized".into(),
            },
        );
        // Not in `probed`: this cycle carried it forward without asking.
        let probed: std::collections::HashSet<String> = ["live".to_string()].into_iter().collect();
        let carried = auth_failure("dead", "401");
        update_auth_backoff(&mut backoff, &[carried], &probed, 5_000.0);
        assert_eq!(
            backoff["dead"].failures, 4,
            "carried-forward rows must not escalate"
        );
        assert_eq!(backoff["dead"].next_attempt_epoch, 9_999.0);
    }

    #[test]
    fn backoff_schedule_climbs_then_caps_at_thirty_minutes() {
        assert_eq!(auth_backoff_secs(1), 300.0);
        assert_eq!(auth_backoff_secs(2), 600.0);
        assert_eq!(auth_backoff_secs(3), 1200.0);
        assert_eq!(auth_backoff_secs(4), 1800.0);
        assert_eq!(auth_backoff_secs(50), 1800.0);
    }

    fn login_account(name: &str) -> Token {
        let now = Utc::now().timestamp_millis();
        Token {
            name: name.into(),
            key: format!("sk-ant-oat01-setup-{name}"),
            access_token: Some(format!("sk-ant-oat01-access-{name}")),
            refresh_token: Some("sk-ant-ort01-refresh".into()),
            expires_at: Some(now + 8 * 3_600_000),
            obtained_at: Some(now),
            scopes: Some(vec!["user:inference".into(), "user:profile".into()]),
            login_error: None,
        }
    }

    #[test]
    fn installing_a_login_declares_its_scopes_and_a_401_wait() {
        let token = login_account("a");
        let mut settings = Map::new();
        let event = install_credential(&mut settings, &token, "switch").unwrap();
        let env = settings["env"].as_object().unwrap();
        assert_eq!(env[OAUTH_SETTING], "sk-ant-oat01-access-a");
        assert_eq!(env[SCOPES_SETTING], "user:inference user:profile");
        assert_eq!(env[WAIT_SETTING], WAIT_MS);
        assert_eq!(env[admission::ACCOUNT_ENV], "a");
        assert_eq!(event.account.as_deref(), Some("a"));
        assert_eq!(
            event.fingerprint.as_deref(),
            Some(credential_history::fingerprint("sk-ant-oat01-access-a").as_str())
        );
        assert!(event.expires_at.is_some());
        assert_eq!(reinstall_reason(&settings, &token), None);
    }

    #[test]
    fn falling_back_to_a_setup_token_withdraws_the_scope_claim() {
        let mut token = login_account("a");
        let mut settings = Map::new();
        install_credential(&mut settings, &token, "switch").unwrap();
        token.login_error = Some("invalid_grant".into());
        assert_eq!(reinstall_reason(&settings, &token), Some("fallback"));
        let event = install_credential(&mut settings, &token, "fallback").unwrap();
        let env = settings["env"].as_object().unwrap();
        assert_eq!(env[OAUTH_SETTING], "sk-ant-oat01-setup-a");
        assert!(!env.contains_key(SCOPES_SETTING));
        assert_eq!(event.expires_at, None);
    }

    #[test]
    fn reinstall_reasons_distinguish_upgrade_refresh_and_repair() {
        let mut token = login_account("a");
        let mut settings = Map::new();
        // Settings still hold the setup token from before `tokeman login`.
        let setup_only = Token {
            access_token: None,
            refresh_token: None,
            ..token.clone()
        };
        install_credential(&mut settings, &setup_only, "switch").unwrap();
        assert_eq!(reinstall_reason(&settings, &token), Some("upgrade"));

        install_credential(&mut settings, &token, "upgrade").unwrap();
        token.access_token = Some("sk-ant-oat01-access-a-2".into());
        assert_eq!(reinstall_reason(&settings, &token), Some("refresh"));

        install_credential(&mut settings, &token, "refresh").unwrap();
        settings
            .get_mut("env")
            .and_then(Value::as_object_mut)
            .unwrap()
            .remove(WAIT_SETTING);
        assert_eq!(reinstall_reason(&settings, &token), Some("repair"));
    }

    #[test]
    fn clearing_removes_every_managed_key() {
        let mut settings = Map::new();
        install_credential(&mut settings, &login_account("a"), "switch").unwrap();
        settings
            .get_mut("env")
            .and_then(Value::as_object_mut)
            .unwrap()
            .insert("UNRELATED".into(), Value::String("kept".into()));
        assert!(clear_configured_token(&mut settings));
        let env = settings["env"].as_object().unwrap();
        assert_eq!(env.len(), 1);
        assert_eq!(env["UNRELATED"], "kept");
    }

    #[test]
    fn degraded_summaries_group_accounts_by_cause() {
        let mut results = vec![result("ok", 0.1, 0.1)];
        for (name, error) in [
            (
                "r1",
                "HTTP 401 Unauthorized: {\"type\":\"error\",\"error\":{\"message\":\"OAuth access token has been revoked.\"}}",
            ),
            (
                "r2",
                "HTTP 401 Unauthorized: {\"error\":{\"message\":\"OAuth access token has been revoked.\"}}",
            ),
            (
                "org",
                "HTTP 403 Forbidden: {\"error\":{\"message\":\"OAuth authentication is currently not allowed for this organization.\"}}",
            ),
            (
                "net",
                "error sending request for url (https://api.anthropic.com/v1/messages)",
            ),
        ] {
            let mut failed = result(name, 0.0, 0.0);
            failed.quota = None;
            failed.error = Some(error.into());
            results.push(failed);
        }
        let summary = degraded_probe_summary(&results, 5);
        assert_eq!(
            summary,
            "1/5 quota readings; unavailable [401 revoked: r1, r2; 403 org disallows OAuth: org; network: net]"
        );
    }

    #[test]
    fn only_full_ids_of_header_only_families_are_sampled() {
        assert_eq!(
            sample_model(Some("claude-fable-5-1[1m]")),
            Some("claude-fable-5-1")
        );
        assert_eq!(
            sample_model(Some("fable")),
            None,
            "an alias names no API model"
        );
        assert_eq!(sample_model(Some("claude-opus-5")), None);
        assert_eq!(sample_model(None), None);
    }

    #[test]
    fn sampling_skips_profile_readers_and_respects_the_interval() {
        let policy = RotationSettings::default();
        let interval = policy.target_model_sample_secs as f64;
        let setup_only = Token {
            name: "a".into(),
            key: "sk-ant-oat01-setup".into(),
            ..Token::default()
        };
        let mut samples = BTreeMap::new();
        assert!(due_for_sample(&setup_only, &samples, &policy, 1_000.0));
        samples.insert(
            "a".into(),
            HeaderSample {
                sampled_at: 1_000.0,
                answered: true,
                window: None,
            },
        );
        assert!(!due_for_sample(
            &setup_only,
            &samples,
            &policy,
            1_000.0 + interval - 1.0
        ));
        assert!(due_for_sample(
            &setup_only,
            &samples,
            &policy,
            1_000.0 + interval
        ));

        let profile_reader = login_account("b");
        assert!(!due_for_sample(
            &profile_reader,
            &BTreeMap::new(),
            &policy,
            1_000.0
        ));

        let mut off = policy.clone();
        off.target_model_sample_secs = 0;
        assert!(!due_for_sample(
            &setup_only,
            &BTreeMap::new(),
            &off,
            1_000.0
        ));
    }

    #[test]
    fn samples_are_recorded_then_carried_until_reset() {
        let mut sample = result("a", 0.1, 0.1);
        sample.quota.as_mut().unwrap().fable = Some(Window {
            utilization: 0.7,
            reset: 5_000,
        });
        let mut refused = result("b", 0.0, 0.0);
        refused.quota = None;
        refused.error = Some("HTTP 429 Too Many Requests: {}".into());
        let mut cache = BTreeMap::new();
        record_header_samples(&[sample, refused], &mut cache, 1_000.0);
        assert_eq!(cache["a"].window.as_ref().unwrap().utilization, 0.7);
        // An unanswered sample is still an attempt: it waits an interval.
        assert!(!cache["b"].answered);
        assert_eq!(cache["b"].sampled_at, 1_000.0);

        // A later Haiku probe carries no 7d_oi; the sample fills it in and
        // becomes a Fable bucket.
        let mut later = [result("a", 0.1, 0.1)];
        apply_header_samples(&mut later, &cache, 2_000.0);
        let usage = later[0].model_usage.as_ref().unwrap();
        assert_eq!(usage.scoped_weekly[0].key, "fable");
        assert_eq!(usage.scoped_weekly[0].source, ModelUsageSource::Headers);

        // After the window resets, the old reading is no longer claimed.
        let mut reset = [result("a", 0.1, 0.1)];
        apply_header_samples(&mut reset, &cache, 6_000.0);
        assert!(reset[0].quota.as_ref().unwrap().fable.is_none());
    }
}
