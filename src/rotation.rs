use std::cmp::Ordering;
use std::collections::BTreeMap;
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

#[cfg(target_os = "macos")]
use anyhow::anyhow;
use anyhow::{Context, Result, bail};
use chrono::{Local, NaiveDateTime, TimeZone, Utc};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use crate::admission;
use crate::config::{Config, RotationSettings, Token};
use crate::probe::{self, ModelQuotaBucket, ModelUsage, ModelUsageSource, ProbeResult};
use crate::store::Store;

const SERVICE_LABEL: &str = "com.ember.tokeman-claude-rotate";
const OAUTH_SETTING: &str = "CLAUDE_CODE_OAUTH_TOKEN";
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
    pub live_sessions: Vec<ClaudeSessionStatus>,
    pub tokens: Vec<RotationTokenStatus>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct CadenceState {
    #[serde(default)]
    last_probe_epoch: f64,
    #[serde(default)]
    mode: Option<RotationMode>,
    /// The next scheduled interval. This can be faster than the mode default
    /// while quota coverage is degraded or the default token is near a floor.
    #[serde(default)]
    probe_interval_secs: Option<u64>,
    #[serde(default)]
    consecutive_degraded_probes: u32,
}

impl Default for CadenceState {
    fn default() -> Self {
        Self {
            last_probe_epoch: 0.0,
            mode: Some(RotationMode::Normal),
            probe_interval_secs: None,
            consecutive_degraded_probes: 0,
        }
    }
}

struct RotationLock {
    _file: File,
}

impl RotationLock {
    fn acquire() -> Result<Self> {
        let path = lock_path()?;
        ensure_private_parent(&path)?;
        let file = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(&path)
            .with_context(|| format!("failed to open rotation lock {}", path.display()))?;
        set_permissions(&path, 0o600)?;
        FileExt::lock_exclusive(&file)
            .with_context(|| format!("failed to lock {}", path.display()))?;
        Ok(Self { _file: file })
    }
}

fn state_dir() -> Result<PathBuf> {
    let config_path = Config::path()?;
    config_path
        .parent()
        .map(Path::to_path_buf)
        .context("tokeman config path has no parent")
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

fn ensure_private_parent(path: &Path) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
        set_permissions(parent, 0o700)?;
    }
    Ok(())
}

#[cfg(unix)]
fn set_permissions(path: &Path, mode: u32) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))?;
    Ok(())
}

#[cfg(not(unix))]
fn set_permissions(_path: &Path, _mode: u32) -> Result<()> {
    Ok(())
}

fn write_atomic(path: &Path, bytes: &[u8], mode: u32) -> Result<()> {
    ensure_private_parent(path)?;
    let parent = path.parent().context("output path has no parent")?;
    let mut temp = tempfile::NamedTempFile::new_in(parent)
        .with_context(|| format!("failed to create temporary file in {}", parent.display()))?;
    temp.write_all(bytes)?;
    temp.as_file().sync_all()?;
    set_permissions(temp.path(), mode)?;
    temp.persist(path)
        .map_err(|e| e.error)
        .with_context(|| format!("failed to replace {}", path.display()))?;
    set_permissions(path, mode)?;
    Ok(())
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

fn append_log(message: &str) {
    let Ok(path) = log_path() else {
        return;
    };
    if ensure_private_parent(&path).is_err() {
        return;
    }
    if let Ok(mut file) = OpenOptions::new().create(true).append(true).open(&path) {
        let _ = set_permissions(&path, 0o600);
        let stamp = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ");
        let _ = writeln!(file, "{stamp} {message}");
    }
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

fn configured_token(settings: &Map<String, Value>) -> Option<&str> {
    settings
        .get("env")
        .and_then(Value::as_object)
        .and_then(|env| env.get(OAUTH_SETTING))
        .and_then(Value::as_str)
        .filter(|token| !token.is_empty())
}

fn configured_account_marker(settings: &Map<String, Value>) -> Option<&str> {
    settings
        .get("env")
        .and_then(Value::as_object)
        .and_then(|env| env.get(admission::ACCOUNT_ENV))
        .and_then(Value::as_str)
        .filter(|account| !account.is_empty())
}

fn configured_target_model(settings: &Map<String, Value>) -> Option<&str> {
    settings
        .get("model")
        .and_then(Value::as_str)
        .filter(|model| !model.is_empty())
}

fn set_configured_token(settings: &mut Map<String, Value>, token: &str, account: &str) {
    if !settings.get("env").is_some_and(Value::is_object) {
        settings.insert("env".into(), Value::Object(Map::new()));
    }
    let env = settings
        .get_mut("env")
        .and_then(Value::as_object_mut)
        .expect("env was just initialized");
    env.insert(OAUTH_SETTING.into(), Value::String(token.into()));
    env.insert(admission::ACCOUNT_ENV.into(), Value::String(account.into()));
}

fn clear_configured_token(settings: &mut Map<String, Value>) -> bool {
    let Some(env) = settings.get_mut("env").and_then(Value::as_object_mut) else {
        return false;
    };
    let changed =
        env.remove(OAUTH_SETTING).is_some() | env.remove(admission::ACCOUNT_ENV).is_some();
    if env.is_empty() {
        settings.remove("env");
    }
    changed
}

fn token_name_for_value<'a>(tokens: &'a [Token], value: Option<&str>) -> Option<&'a str> {
    let value = value?;
    tokens
        .iter()
        .find(|token| token.key.as_bytes() == value.as_bytes())
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
    result
        .error
        .as_deref()
        .is_some_and(|error| error.starts_with("HTTP 401") || error.starts_with("HTTP 403"))
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
    if normalized == "fable" || normalized.contains("fable") {
        return Some(ModelDescriptor {
            family: "opus".into(),
            version: Some("48".into()),
        });
    }
    for family in ["opus", "sonnet"] {
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
            bucket.version.is_none() || target.version == bucket.version
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
    let Some(usage) = result.model_usage.as_ref() else {
        return Vec::new();
    };
    let descriptor = family_version(target_model);
    let mut windows = Vec::new();
    match descriptor
        .as_ref()
        .map(|descriptor| descriptor.family.as_str())
    {
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
    let single_line = error.split_whitespace().collect::<Vec<_>>().join(" ");
    const MAX_CHARS: usize = 160;
    if single_line.chars().count() <= MAX_CHARS {
        single_line
    } else {
        format!(
            "{}…",
            single_line.chars().take(MAX_CHARS).collect::<String>()
        )
    }
}

fn degraded_probe_summary(results: &[ProbeResult], expected: usize) -> String {
    let available = results
        .iter()
        .filter(|result| has_quota_reading(result))
        .count();
    let unavailable = results
        .iter()
        .filter(|result| !has_quota_reading(result))
        .map(|result| {
            format!(
                "{}: {}",
                result.token_name,
                result
                    .error
                    .as_deref()
                    .map(compact_error)
                    .unwrap_or_else(|| "quota headers unavailable".into())
            )
        })
        .collect::<Vec<_>>();
    if unavailable.is_empty() {
        format!("{available}/{expected} quota readings")
    } else {
        format!(
            "{available}/{expected} quota readings; unavailable [{}]",
            unavailable.join("; ")
        )
    }
}

pub fn choose_best<'a>(
    results: &'a [ProbeResult],
    mode: RotationMode,
    policy: &RotationSettings,
    target_model: Option<&str>,
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
            dwell_score(b, mode, policy, target_model),
            dwell_score(a, mode, policy, target_model),
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

async fn probe_and_store(config: &Config) -> Vec<ProbeResult> {
    // Match the dashboard's timeout. All token requests are concurrent, and a
    // slow-but-valid 20-40s response is safer than treating a partial 15s batch
    // as fleet truth. The scheduler skips missed ticks.
    let mut results =
        probe::probe_all_with_timeout(&config.tokens, std::time::Duration::from_secs(45)).await;
    admission::apply_observed_limits(&mut results);
    if let Ok(store) = Store::open() {
        for result in &results {
            let _ = store.insert(result);
        }
    }
    results
}

pub async fn rotate(config: &Config, options: RotateOptions) -> Result<RotationOutcome> {
    config.rotation.validate()?;
    if config.tokens.is_empty() {
        bail!("No tokens configured. Use `tokeman add <name> <key>` to add one.");
    }
    let _lock = RotationLock::acquire()?;

    if pause_path()?.exists() {
        return Ok(RotationOutcome {
            action: "paused".into(),
            mode: load_cadence().mode.unwrap_or(RotationMode::Normal),
            active_before: None,
            active_after: None,
            changed: false,
            probed: false,
            message: "rotation is paused".into(),
        });
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
            return Ok(RotationOutcome {
                action: "cadence-skip".into(),
                mode: previous_mode,
                active_before: None,
                active_after: None,
                changed: false,
                probed: false,
                message: format!("next probe is not due ({interval:.0}s cadence)"),
            });
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
        && configured_account_marker(&settings) != Some(account)
    {
        let token = config
            .tokens
            .iter()
            .find(|token| token.name == account)
            .context("managed Claude OAuth token disappeared from configuration")?;
        set_configured_token(&mut settings, &token.key, account);
        save_claude_settings(&settings)?;
    }
    let results = probe_and_store(config).await;
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
        let active = current_name.clone().or_else(|| {
            current_value
                .as_ref()
                .map(|_| "unmanaged OAuth token".into())
        });
        let message = format!(
            "probe failed for every token; kept {}; retrying in {}s ({})",
            active.as_deref().unwrap_or("/login"),
            next_interval,
            degraded_probe_summary(&results, config.tokens.len())
        );
        append_log(&message);
        return Ok(RotationOutcome {
            action: "probe-failed".into(),
            mode,
            active_before: active.clone(),
            active_after: active,
            changed: false,
            probed: true,
            message,
        });
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
        let default = current_name.clone().or_else(|| {
            current_value
                .as_ref()
                .map(|_| "unmanaged OAuth token".into())
        });
        let message = format!(
            "partial probe; kept default {}; retrying in {}s ({})",
            default.as_deref().unwrap_or("/login"),
            next_interval,
            degraded_probe_summary(&results, config.tokens.len())
        );
        append_log(&message);
        return Ok(RotationOutcome {
            action: "partial-probe-hold".into(),
            mode,
            active_before: default.clone(),
            active_after: default,
            changed: false,
            probed: true,
            message,
        });
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
        return Ok(RotationOutcome {
            action: "default-probe-degraded".into(),
            mode,
            active_before: current_name.clone(),
            active_after: current_name,
            changed: false,
            probed: true,
            message,
        });
    }

    let current_healthy = current_result
        .is_some_and(|result| is_viable(result, mode, &config.rotation, target_model.as_deref()));
    if current_healthy && !options.force {
        let message = format!(
            "kept {} ({})",
            current_name.as_deref().unwrap_or("default token"),
            describe(current_result)
        );
        return Ok(RotationOutcome {
            action: "kept".into(),
            mode,
            active_before: current_name.clone(),
            active_after: current_name,
            changed: false,
            probed: true,
            message,
        });
    }

    let Some(best) = choose_best(&results, mode, &config.rotation, target_model.as_deref()) else {
        let active = current_name.clone().or_else(|| {
            current_value
                .as_ref()
                .map(|_| "unmanaged OAuth token".into())
        });
        let message = format!(
            "no viable token; kept {}",
            active.as_deref().unwrap_or("/login")
        );
        append_log(&message);
        return Ok(RotationOutcome {
            action: "no-viable-token".into(),
            mode,
            active_before: active.clone(),
            active_after: active,
            changed: false,
            probed: true,
            message,
        });
    };

    let best_token = config
        .tokens
        .iter()
        .find(|token| token.name == best.token_name)
        .context("probe returned a token absent from configuration")?;
    if current_name.as_deref() == Some(best.token_name.as_str()) {
        let message = format!("kept {} ({})", best.token_name, describe(Some(best)));
        return Ok(RotationOutcome {
            action: "kept".into(),
            mode,
            active_before: current_name.clone(),
            active_after: current_name,
            changed: false,
            probed: true,
            message,
        });
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
    set_configured_token(&mut settings, &best_token.key, &best_token.name);
    save_claude_settings(&settings)?;
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
    let Ok(contents) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    let mut events = contents
        .lines()
        .filter_map(parse_default_event)
        .collect::<Vec<_>>();
    events.sort_by_key(|event| event.at);
    events
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

fn process_oauth_token(pid: u32) -> Option<String> {
    let output = Command::new("/bin/ps")
        .args(["eww", "-p", &pid.to_string(), "-o", "command="])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8_lossy(&output.stdout)
        .split_whitespace()
        .find_map(|field| field.strip_prefix("CLAUDE_CODE_OAUTH_TOKEN="))
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
}

/// Inspect live Claude processes without exposing credentials. Process-local
/// OAuth variables and SessionStart launch bindings are exact; older sessions
/// are inferred from the default event preceding process startup.
pub fn claude_sessions(config: &Config) -> Vec<ClaudeSessionStatus> {
    let Ok(output) = Command::new("/bin/ps")
        .args(["-axo", "pid=,tty=,lstart=,comm="])
        .output()
    else {
        return Vec::new();
    };
    if !output.status.success() {
        return Vec::new();
    }
    let events = default_events();
    let mut sessions = String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(parse_ps_session)
        .map(|(pid, tty, started_at)| {
            let binding = admission::binding_for_pid(pid, started_at.timestamp());
            let inherited = process_oauth_token(pid);
            let exact_account =
                token_name_for_value(&config.tokens, inherited.as_deref()).map(str::to_owned);
            let (account, credential_source, session_id) = if inherited.is_some() {
                (
                    exact_account.or_else(|| Some("unmanaged OAuth token".into())),
                    "process environment (exact)".into(),
                    binding.as_ref().map(|binding| binding.session_id.clone()),
                )
            } else if let Some(binding) = binding {
                (
                    Some(binding.account),
                    "SessionStart launch binding (exact)".into(),
                    Some(binding.session_id),
                )
            } else {
                let estimated = events
                    .iter()
                    .rev()
                    .find(|event| event.at <= started_at.timestamp())
                    .and_then(|event| event.account.clone());
                match estimated {
                    Some(account) => (
                        Some(account),
                        "default at process start (estimated)".into(),
                        None,
                    ),
                    None => (None, "unknown (predates recorded default)".into(), None),
                }
            };
            ClaudeSessionStatus {
                pid,
                tty,
                started_at,
                session_id,
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
        let prefix = if session.credential_source.contains("(exact)") {
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
    let results = probe_and_store(config).await;
    let previous_mode = load_cadence().mode.unwrap_or(RotationMode::Normal);
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
        live_sessions,
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
    println!(
        "default for newly started Claude processes: {}",
        status.default_token.as_deref().unwrap_or("/login")
    );
    println!(
        "selection model: {} (only matching model-scoped buckets affect rotation)",
        status.target_model.as_deref().unwrap_or("unknown")
    );
    println!(
        "live Claude processes: {} ({}; ~=startup estimate, unmarked=exact launch binding)",
        status.live_sessions.len(),
        session_summary(&status.live_sessions)
    );
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
                token.error.as_deref().unwrap_or("quota unavailable")
            ),
        }
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
    set_permissions(&path, 0o600)?;
    let mut settings = load_claude_settings()?;
    if clear_configured_token(&mut settings) {
        save_claude_settings(&settings)?;
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

/// Persistent service loop. launchd supervises this one process instead of
/// spawning a fresh helper every 20 seconds (which can be delayed by xpcproxy).
/// Configuration is reloaded on every wake so policy/token edits take effect
/// without restarting the service.
pub async fn daemon() -> Result<()> {
    let startup_executable = executable_identity();
    let mut wake = tokio::time::interval(std::time::Duration::from_secs(20));
    wake.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        wake.tick().await;
        if startup_executable.is_some()
            && executable_identity().is_some_and(|current| Some(current) != startup_executable)
        {
            append_log("daemon executable was replaced; exiting so launchd loads the new binary");
            return Ok(());
        }
        match Config::load() {
            Ok(config) => match rotate(
                &config,
                RotateOptions {
                    scheduled: true,
                    ..RotateOptions::default()
                },
            )
            .await
            {
                Ok(outcome) if outcome.changed || outcome.action == "no-viable-token" => {
                    println!("{}", outcome.message);
                }
                Ok(_) => {}
                Err(error) => {
                    append_log(&format!("ERROR rotation cycle failed: {error:#}"));
                    eprintln!("tokeman rotation cycle failed: {error:#}");
                }
            },
            Err(error) => {
                append_log(&format!("ERROR config reload failed: {error:#}"));
                eprintln!("tokeman config reload failed: {error:#}");
            }
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
    set_configured_token(&mut settings, &token.key, &token.name);
    save_claude_settings(&settings)?;
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
        if let Some(value) = configured_token(&settings).map(str::to_owned)
            && let Some(account) =
                token_name_for_value(&config.tokens, Some(&value)).map(str::to_owned)
        {
            set_configured_token(&mut settings, &value, &account);
        }
        install_observer_hook(&mut settings, &binary);
        save_claude_settings(&settings)?;

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
        assert!(!is_viable(
            &result_with_opus("opus-wall", 0.0, 0.0, 0.95),
            RotationMode::Normal,
            &policy,
            Some("opus")
        ));
        assert!(is_viable(
            &result_with_opus("opus-room", 0.0, 0.0, 0.94),
            RotationMode::Normal,
            &policy,
            Some("opus")
        ));
    }

    #[test]
    fn exhausted_opus_48_bucket_does_not_poison_opus_5() {
        let policy = RotationSettings::default();
        let result = result_with_scoped_bucket("separate-models", "claudeopus48", "Opus 4.8", 1.0);
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
            Some("fable")
        ));
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
}
