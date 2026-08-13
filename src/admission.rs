use std::collections::{BTreeMap, HashSet};
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};
use chrono::{
    DateTime, Datelike, Duration, Local, NaiveDate, NaiveDateTime, NaiveTime, TimeZone, Utc,
};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::config::{Config, Token};
use crate::probe::{
    ModelQuotaBucket, ModelUsage, ModelUsageSource, ProbeResult, Window, normalized_bucket_key,
};

pub const ACCOUNT_ENV: &str = "TOKEMAN_ACCOUNT";
const STATE_FILE: &str = "claude-admission-state.json";
const LOCK_FILE: &str = "claude-admission-state.lock";
const SESSION_RETENTION_DAYS: i64 = 14;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ModelFamily {
    Opus,
    Sonnet,
    General,
}

impl ModelFamily {
    pub fn from_model(model: &str) -> Self {
        let lower = model.to_ascii_lowercase();
        if lower.contains("opus") {
            Self::Opus
        } else if lower.contains("sonnet") {
            Self::Sonnet
        } else {
            Self::General
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Opus => "Opus",
            Self::Sonnet => "Sonnet",
            Self::General => "general",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum LimitWindow {
    FiveHour,
    Weekly,
}

impl LimitWindow {
    pub fn label(self) -> &'static str {
        match self {
            Self::FiveHour => "5h",
            Self::Weekly => "weekly",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ObservedLimit {
    pub account: String,
    pub model: ModelFamily,
    /// Concrete Claude model when the rejection was observed (for example
    /// `claude-opus-4-8`). A missing value means the whole family is known to
    /// be constrained, as with a manual family quarantine or profile bucket.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_id: Option<String>,
    pub window: LimitWindow,
    pub observed_at: i64,
    pub reset: i64,
    pub session_id: Option<String>,
    pub source: String,
}

impl ObservedLimit {
    pub fn active_at(&self, now: i64) -> bool {
        self.reset > now
    }

    pub fn summary(&self) -> String {
        format!(
            "{} {} rejected until {}",
            self.model_id
                .as_deref()
                .unwrap_or_else(|| self.model.label()),
            self.window.label(),
            format_reset(self.reset)
        )
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct BoundSession {
    pub session_id: String,
    pub account: String,
    pub transcript_path: PathBuf,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct SessionBinding {
    session_id: String,
    account: String,
    transcript_path: PathBuf,
    #[serde(default)]
    cwd: Option<PathBuf>,
    #[serde(default)]
    source: Option<String>,
    observed_at: i64,
    #[serde(default)]
    pid: Option<u32>,
    #[serde(default)]
    process_started_at: Option<i64>,
    #[serde(default)]
    scan_offset: u64,
    #[serde(default)]
    last_model: Option<String>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct AdmissionState {
    #[serde(default)]
    sessions: BTreeMap<String, SessionBinding>,
    #[serde(default)]
    limits: Vec<ObservedLimit>,
}

#[derive(Debug, Deserialize)]
struct SessionStartInput {
    session_id: String,
    transcript_path: PathBuf,
    #[serde(default)]
    cwd: Option<PathBuf>,
    #[serde(default)]
    source: Option<String>,
}

struct StateLock {
    _file: File,
}

impl StateLock {
    fn acquire() -> Result<Self> {
        let path = lock_path()?;
        ensure_private_parent(&path)?;
        let file = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(&path)
            .with_context(|| format!("failed to open admission lock {}", path.display()))?;
        set_permissions(&path, 0o600)?;
        FileExt::lock_exclusive(&file)
            .with_context(|| format!("failed to lock {}", path.display()))?;
        Ok(Self { _file: file })
    }
}

fn state_dir() -> Result<PathBuf> {
    Config::path()?
        .parent()
        .map(Path::to_path_buf)
        .context("tokeman config path has no parent")
}

fn state_path() -> Result<PathBuf> {
    Ok(state_dir()?.join(STATE_FILE))
}

fn lock_path() -> Result<PathBuf> {
    Ok(state_dir()?.join(LOCK_FILE))
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

fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    ensure_private_parent(path)?;
    let parent = path.parent().context("admission state has no parent")?;
    let mut temp = tempfile::NamedTempFile::new_in(parent)
        .with_context(|| format!("failed to create temporary file in {}", parent.display()))?;
    temp.write_all(bytes)?;
    temp.as_file().sync_all()?;
    set_permissions(temp.path(), 0o600)?;
    temp.persist(path)
        .map_err(|error| error.error)
        .with_context(|| format!("failed to replace {}", path.display()))?;
    set_permissions(path, 0o600)?;
    Ok(())
}

fn load_state() -> AdmissionState {
    state_path()
        .ok()
        .and_then(|path| std::fs::read(path).ok())
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default()
}

fn save_state(state: &AdmissionState) -> Result<()> {
    let mut bytes = serde_json::to_vec_pretty(state)?;
    bytes.push(b'\n');
    write_atomic(&state_path()?, &bytes)
}

fn prune_state(state: &mut AdmissionState, now: i64) {
    state.limits.retain(|limit| limit.active_at(now));
    let oldest_session = now - Duration::days(SESSION_RETENTION_DAYS).num_seconds();
    state
        .sessions
        .retain(|_, session| session.observed_at >= oldest_session);
}

pub fn hook_command(binary: &Path) -> String {
    format!(
        "{} rotate observe-session --pid \"$PPID\"",
        shell_quote(&binary.to_string_lossy())
    )
}

pub fn is_observer_hook(command: &str) -> bool {
    command.contains(" rotate observe-session ")
        || command.ends_with(" rotate observe-session")
        || command.contains(" rotate observe-session --pid")
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\"'\"'"))
}

pub fn record_session_from_hook(pid: u32, input: &[u8]) -> Result<()> {
    let account = match std::env::var(ACCOUNT_ENV) {
        Ok(account) if !account.trim().is_empty() => account,
        _ => return Ok(()),
    };
    let input: SessionStartInput =
        serde_json::from_slice(input).context("invalid Claude SessionStart hook input")?;
    if input.session_id.trim().is_empty() {
        bail!("Claude SessionStart hook omitted session_id");
    }

    let now = Utc::now().timestamp();
    let scan_offset = std::fs::metadata(&input.transcript_path)
        .map(|metadata| metadata.len())
        .unwrap_or(0);
    let last_model = (pid > 0)
        .then(|| process_model(pid))
        .flatten()
        .or_else(|| infer_last_model(&input.transcript_path, scan_offset))
        .or_else(configured_default_model);
    let binding = SessionBinding {
        session_id: input.session_id.clone(),
        account,
        transcript_path: input.transcript_path,
        cwd: input.cwd,
        source: input.source,
        observed_at: now,
        pid: (pid > 0).then_some(pid),
        process_started_at: (pid > 0).then(|| process_started_at(pid)).flatten(),
        scan_offset,
        last_model,
    };

    let _lock = StateLock::acquire()?;
    let mut state = load_state();
    prune_state(&mut state, now);
    state.sessions.insert(input.session_id, binding);
    save_state(&state)
}

fn process_started_at(pid: u32) -> Option<i64> {
    let output = Command::new("/bin/ps")
        .args(["-p", &pid.to_string(), "-o", "lstart="])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let value = String::from_utf8_lossy(&output.stdout);
    let local = NaiveDateTime::parse_from_str(value.trim(), "%a %b %e %T %Y").ok()?;
    Local
        .from_local_datetime(&local)
        .single()
        .or_else(|| Local.from_local_datetime(&local).earliest())
        .map(|value| value.timestamp())
}

fn process_model(pid: u32) -> Option<String> {
    let output = Command::new("/bin/ps")
        .args(["-p", &pid.to_string(), "-o", "command="])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let command = String::from_utf8_lossy(&output.stdout);
    let fields = command.split_whitespace().collect::<Vec<_>>();
    for (index, field) in fields.iter().enumerate() {
        if let Some(model) = field.strip_prefix("--model=") {
            return (!model.is_empty()).then(|| model.to_owned());
        }
        if *field == "--model" {
            return fields
                .get(index + 1)
                .filter(|model| !model.is_empty())
                .map(|model| (*model).to_owned());
        }
    }
    None
}

fn configured_default_model() -> Option<String> {
    let path = std::env::var_os("CLAUDE_SETTINGS")
        .map(PathBuf::from)
        .or_else(|| dirs::home_dir().map(|home| home.join(".claude").join("settings.json")))?;
    let value: Value = serde_json::from_slice(&std::fs::read(path).ok()?).ok()?;
    value
        .get("model")
        .and_then(Value::as_str)
        .filter(|model| !model.is_empty())
        .map(str::to_owned)
}

fn infer_last_model(path: &Path, through: u64) -> Option<String> {
    let file = File::open(path).ok()?;
    let mut reader = BufReader::new(file);
    let mut line = String::new();
    let mut last_model = None;
    while reader.stream_position().ok()? < through {
        line.clear();
        if reader.read_line(&mut line).ok()? == 0 {
            break;
        }
        if let Ok(value) = serde_json::from_str::<Value>(&line) {
            update_last_model(&value, &mut last_model);
        }
    }
    last_model
}

fn update_last_model(value: &Value, last_model: &mut Option<String>) {
    let Some(model) = value
        .get("message")
        .and_then(|message| message.get("model"))
        .and_then(Value::as_str)
        .filter(|model| !model.is_empty() && *model != "<synthetic>")
    else {
        return;
    };
    *last_model = Some(model.to_owned());
}

fn event_timestamp(value: &Value) -> Option<i64> {
    DateTime::parse_from_rfc3339(value.get("timestamp")?.as_str()?)
        .ok()
        .map(|value| value.timestamp())
}

fn rate_limit_message(value: &Value) -> Option<String> {
    if value.get("type")?.as_str()? != "assistant" || value.get("error")?.as_str()? != "rate_limit"
    {
        return None;
    }
    let content = value
        .get("message")?
        .get("content")?
        .as_array()?
        .iter()
        .filter_map(|block| {
            (block.get("type").and_then(Value::as_str) == Some("text"))
                .then(|| block.get("text").and_then(Value::as_str))
                .flatten()
        })
        .collect::<Vec<_>>()
        .join(" ");
    (!content.is_empty()).then_some(content)
}

fn limit_from_message(
    binding: &SessionBinding,
    message: &str,
    observed_at: i64,
) -> Option<ObservedLimit> {
    let lower = message.to_ascii_lowercase();
    let window = if lower.contains("weekly limit") {
        LimitWindow::Weekly
    } else if lower.contains("session limit") {
        LimitWindow::FiveHour
    } else {
        return None;
    };
    let model_id = binding
        .last_model
        .as_deref()
        .filter(|model| !model.is_empty())
        .map(str::to_owned);
    let model = model_id
        .as_deref()
        .map(ModelFamily::from_model)
        .unwrap_or(ModelFamily::General);
    let reset = parse_reset_from_message(message, observed_at).unwrap_or_else(|| {
        observed_at
            + match window {
                LimitWindow::FiveHour => Duration::hours(6).num_seconds(),
                LimitWindow::Weekly => Duration::days(8).num_seconds(),
            }
    });
    Some(ObservedLimit {
        account: binding.account.clone(),
        model,
        // The newly discovered weekly limits can be version-specific: Opus
        // 4.8 may reject while Opus 5 on the same account succeeds. Preserve
        // the concrete model instead of poisoning the entire family bucket.
        model_id: (window == LimitWindow::Weekly && model != ModelFamily::General)
            .then_some(model_id)
            .flatten(),
        window,
        observed_at,
        reset,
        session_id: Some(binding.session_id.clone()),
        source: "observed Claude rejection".into(),
    })
}

/// Merge one rejection and report whether it describes a newly exhausted
/// quota window. Repeated prompts against the same wall produce distinct
/// transcript UUIDs, but they must not continually retrigger fleet probes or
/// erase the first-observed forensic timestamp.
fn merge_limit(limits: &mut Vec<ObservedLimit>, incoming: ObservedLimit) -> bool {
    if let Some(existing) = limits.iter_mut().find(|existing| {
        existing.account == incoming.account
            && existing.model == incoming.model
            && existing.model_id == incoming.model_id
            && existing.window == incoming.window
    }) {
        if incoming.reset != existing.reset && incoming.observed_at >= existing.observed_at {
            *existing = incoming;
            true
        } else {
            false
        }
    } else {
        limits.push(incoming);
        true
    }
}

fn scan_binding(binding: &mut SessionBinding, now: i64) -> Result<(Vec<ObservedLimit>, bool)> {
    // SessionStart is the credential boundary we can observe exactly. Claude
    // notices settings changes while running, but auth reload is not reliable
    // enough to attribute a rejection to a later default event.
    scan_binding_with_default_resolver(binding, now, |_| None)
}

fn scan_binding_with_default_resolver<F>(
    binding: &mut SessionBinding,
    now: i64,
    default_at: F,
) -> Result<(Vec<ObservedLimit>, bool)>
where
    F: Fn(i64) -> Option<Option<String>>,
{
    let Ok(file) = File::open(&binding.transcript_path) else {
        return Ok((Vec::new(), false));
    };
    let len = file.metadata().map(|metadata| metadata.len()).unwrap_or(0);
    if len < binding.scan_offset {
        // A pruning/compaction rewrite invalidated the byte cursor. Ignore the
        // rewritten history; a later SessionStart hook establishes a fresh
        // exact boundary without replaying old quota failures.
        binding.scan_offset = len;
        return Ok((Vec::new(), true));
    }
    if len == binding.scan_offset {
        return Ok((Vec::new(), false));
    }

    let mut reader = BufReader::new(file);
    reader.seek(SeekFrom::Start(binding.scan_offset))?;
    let mut line = String::new();
    let mut incoming = Vec::new();
    loop {
        let line_start = reader.stream_position()?;
        line.clear();
        let read = reader.read_line(&mut line)?;
        if read == 0 {
            break;
        }
        if !line.ends_with('\n') {
            // Do not consume a partially appended JSON record.
            reader.seek(SeekFrom::Start(line_start))?;
            break;
        }
        let Ok(value) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        update_last_model(&value, &mut binding.last_model);
        if let Some(message) = rate_limit_message(&value) {
            let observed_at = event_timestamp(&value).unwrap_or(now);
            // Forked transcripts contain historical records. The byte
            // boundary normally excludes them; the timestamp guard also
            // protects against a concurrent rewrite around hook startup.
            if observed_at + 2 >= binding.observed_at
                && let Some(mut limit) = limit_from_message(binding, &message, observed_at)
            {
                // A caller may provide stronger credential history. Without
                // it, retain the exact SessionStart launch binding.
                match default_at(observed_at) {
                    Some(Some(account)) => limit.account = account,
                    Some(None) => continue,
                    None => {}
                }
                incoming.push(limit);
            }
        }
    }
    let new_offset = reader.stream_position()?;
    let changed = new_offset != binding.scan_offset;
    binding.scan_offset = new_offset;
    Ok((incoming, changed))
}

/// Scan only bytes appended after each exact SessionStart binding.
///
/// Returns the number of new authoritative rate-limit observations. A caller
/// can use this to bypass its normal polling cadence and rotate immediately.
pub fn scan_transcripts(tokens: &[Token]) -> Result<usize> {
    let _lock = StateLock::acquire()?;
    let mut state = load_state();
    let now = Utc::now().timestamp();
    prune_state(&mut state, now);
    let configured = tokens
        .iter()
        .map(|token| token.name.as_str())
        .collect::<HashSet<_>>();
    let mut incoming = Vec::new();
    let mut state_changed = false;

    for binding in state.sessions.values_mut() {
        if !configured.contains(binding.account.as_str()) {
            continue;
        }
        let (limits, changed) = scan_binding(binding, now)?;
        incoming.extend(limits);
        state_changed |= changed;
    }

    let mut observed = 0;
    for limit in incoming {
        observed += usize::from(merge_limit(&mut state.limits, limit));
        state_changed = true;
    }
    if state_changed {
        save_state(&state)?;
    }
    Ok(observed)
}

pub fn active_limits() -> Vec<ObservedLimit> {
    let now = Utc::now().timestamp();
    let mut limits = load_state()
        .limits
        .into_iter()
        .filter(|limit| limit.active_at(now))
        .collect::<Vec<_>>();
    limits.sort_by(|a, b| {
        a.account
            .cmp(&b.account)
            .then_with(|| a.model.cmp(&b.model))
            .then_with(|| a.model_id.cmp(&b.model_id))
            .then_with(|| a.window.cmp(&b.window))
    });
    limits
}

pub fn apply_observed_limits(results: &mut [ProbeResult]) {
    let limits = active_limits();
    for result in results {
        let token_name = result.token_name.clone();
        for limit in limits.iter().filter(|limit| limit.account == token_name) {
            apply_limit(result, limit);
        }
    }
}

fn apply_limit(result: &mut ProbeResult, limit: &ObservedLimit) {
    // A concrete weekly rejection constrains exactly that model, not its whole
    // family. Preserve it as a scoped bucket so the startup model matcher can
    // act on it without making (for example) an Opus 4.8 wall poison Opus 5.
    if limit.window == LimitWindow::Weekly
        && let Some(model_id) = limit.model_id.as_deref()
    {
        let key = normalized_bucket_key(model_id);
        if key.is_empty() {
            return;
        }
        let usage = result.model_usage.get_or_insert_with(ModelUsage::default);
        let observed = ModelQuotaBucket {
            key: key.clone(),
            label: model_id.to_owned(),
            window: Window {
                utilization: 1.0,
                reset: limit.reset,
            },
            source: ModelUsageSource::ObservedRejection,
        };
        if let Some(bucket) = usage
            .scoped_weekly
            .iter_mut()
            .find(|bucket| bucket.key == key)
        {
            *bucket = observed;
        } else {
            usage.scoped_weekly.push(observed);
        }
        return;
    }
    match (limit.window, limit.model) {
        (LimitWindow::Weekly, ModelFamily::Opus) => {
            let usage = result.model_usage.get_or_insert_with(ModelUsage::default);
            usage.opus_weekly = Some(Window {
                utilization: 1.0,
                reset: limit.reset,
            });
            usage.opus_source = Some(ModelUsageSource::ObservedRejection);
        }
        (LimitWindow::Weekly, ModelFamily::Sonnet) => {
            let usage = result.model_usage.get_or_insert_with(ModelUsage::default);
            usage.sonnet_weekly = Some(Window {
                utilization: 1.0,
                reset: limit.reset,
            });
            usage.sonnet_source = Some(ModelUsageSource::ObservedRejection);
        }
        (LimitWindow::Weekly, ModelFamily::General) => {
            if let Some(quota) = result.quota.as_mut() {
                quota.weekly = Some(Window {
                    utilization: 1.0,
                    reset: limit.reset,
                });
                quota.status = "rejected".into();
                quota.representative_claim = "seven_day".into();
                quota.reset = limit.reset;
            }
        }
        (LimitWindow::FiveHour, _) => {
            if let Some(quota) = result.quota.as_mut() {
                quota.session = Some(Window {
                    utilization: 1.0,
                    reset: limit.reset,
                });
                quota.status = "rejected".into();
                quota.representative_claim = "five_hour".into();
                quota.reset = limit.reset;
            }
        }
    }
}

pub fn record_manual_limit(
    account: &str,
    model: ModelFamily,
    model_id: Option<String>,
    window: LimitWindow,
    reset: i64,
) -> Result<()> {
    if reset <= Utc::now().timestamp() {
        bail!("quarantine reset must be in the future");
    }
    let _lock = StateLock::acquire()?;
    let mut state = load_state();
    let now = Utc::now().timestamp();
    prune_state(&mut state, now);
    let _ = merge_limit(
        &mut state.limits,
        ObservedLimit {
            account: account.into(),
            model,
            model_id,
            window,
            observed_at: now,
            reset,
            session_id: None,
            source: "manual quarantine".into(),
        },
    );
    save_state(&state)
}

pub fn clear_limits(account: &str, model: Option<ModelFamily>) -> Result<usize> {
    let _lock = StateLock::acquire()?;
    let mut state = load_state();
    let before = state.limits.len();
    state.limits.retain(|limit| {
        limit.account != account || model.is_some_and(|model| limit.model != model)
    });
    let removed = before - state.limits.len();
    if removed > 0 {
        save_state(&state)?;
    }
    Ok(removed)
}

pub fn binding_for_pid(pid: u32, process_started_at: i64) -> Option<BoundSession> {
    load_state()
        .sessions
        .into_values()
        .filter(|binding| binding.pid == Some(pid))
        .filter(|binding| {
            binding
                .process_started_at
                .is_some_and(|started| (started - process_started_at).abs() <= 2)
        })
        .max_by_key(|binding| binding.observed_at)
        .map(|binding| BoundSession {
            session_id: binding.session_id,
            account: binding.account,
            transcript_path: binding.transcript_path,
        })
}

pub fn parse_reset_spec(value: &str) -> Result<i64> {
    if let Ok(epoch) = value.parse::<i64>() {
        return Ok(epoch);
    }
    DateTime::parse_from_rfc3339(value)
        .map(|value| value.timestamp())
        .with_context(|| format!("invalid reset '{value}'; use RFC3339 or a Unix timestamp"))
}

fn parse_reset_from_message(message: &str, observed_at: i64) -> Option<i64> {
    let reset_text = message.split_once("resets ")?.1;
    let reset_text = reset_text.split(" (").next().unwrap_or(reset_text).trim();
    let observed = DateTime::from_timestamp(observed_at, 0)?.with_timezone(&Local);

    if let Some((date_text, time_text)) = reset_text.split_once(" at ") {
        let time = parse_clock(time_text)?;
        for year in [observed.year(), observed.year() + 1] {
            let date =
                NaiveDate::parse_from_str(&format!("{year} {date_text}"), "%Y %b %e").ok()?;
            let local = Local
                .from_local_datetime(&date.and_time(time))
                .single()
                .or_else(|| Local.from_local_datetime(&date.and_time(time)).earliest())?;
            if local.timestamp() > observed_at {
                return Some(local.timestamp());
            }
        }
        return None;
    }

    let time = parse_clock(reset_text)?;
    let mut date = observed.date_naive();
    let mut local = Local
        .from_local_datetime(&date.and_time(time))
        .single()
        .or_else(|| Local.from_local_datetime(&date.and_time(time)).earliest())?;
    if local.timestamp() <= observed_at {
        date = date.succ_opt()?;
        local = Local
            .from_local_datetime(&date.and_time(time))
            .single()
            .or_else(|| Local.from_local_datetime(&date.and_time(time)).earliest())?;
    }
    Some(local.timestamp())
}

fn parse_clock(value: &str) -> Option<NaiveTime> {
    let normalized = value.trim().to_ascii_uppercase();
    let (clock, suffix) = normalized.split_at(normalized.len().checked_sub(2)?);
    if suffix != "AM" && suffix != "PM" {
        return None;
    }
    let (hour, minute) = clock
        .split_once(':')
        .map(|(hour, minute)| (hour.parse::<u32>().ok(), minute.parse::<u32>().ok()))
        .unwrap_or_else(|| (clock.parse::<u32>().ok(), Some(0)));
    let hour = hour?;
    let minute = minute?;
    if !(1..=12).contains(&hour) || minute >= 60 {
        return None;
    }
    let hour = match (hour, suffix) {
        (12, "AM") => 0,
        (12, "PM") => 12,
        (hour, "PM") => hour + 12,
        (hour, "AM") => hour,
        _ => return None,
    };
    NaiveTime::from_hms_opt(hour, minute, 0)
}

fn format_reset(reset: i64) -> String {
    DateTime::from_timestamp(reset, 0)
        .map(|value| {
            value
                .with_timezone(&Local)
                .format("%a %-I:%M%P")
                .to_string()
        })
        .unwrap_or_else(|| reset.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::probe::{RateLimits, UnifiedQuota};
    use serde_json::json;

    fn probe(name: &str) -> ProbeResult {
        ProbeResult {
            token_name: name.into(),
            probed_at: Utc::now(),
            quota: Some(UnifiedQuota {
                status: "allowed".into(),
                reset: 0,
                representative_claim: "five_hour".into(),
                fallback: None,
                session: Some(Window {
                    utilization: 0.1,
                    reset: 0,
                }),
                weekly: Some(Window {
                    utilization: 0.2,
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

    #[test]
    fn parses_weekly_reset_with_date() {
        let observed = DateTime::parse_from_rfc3339("2026-07-27T20:17:20Z")
            .unwrap()
            .timestamp();
        let reset = parse_reset_from_message(
            "You've hit your weekly limit · resets Aug 1 at 9am (America/New_York)",
            observed,
        )
        .unwrap();
        let local = DateTime::from_timestamp(reset, 0)
            .unwrap()
            .with_timezone(&Local);
        assert_eq!(local.format("%Y-%m-%d %-I%P").to_string(), "2026-08-01 9am");
    }

    #[test]
    fn parses_same_day_clock_or_rolls_to_tomorrow() {
        let observed = Local
            .with_ymd_and_hms(2026, 7, 27, 16, 0, 0)
            .single()
            .unwrap()
            .timestamp();
        let reset =
            parse_reset_from_message("session limit · resets 11pm (America/New_York)", observed)
                .unwrap();
        let local = DateTime::from_timestamp(reset, 0)
            .unwrap()
            .with_timezone(&Local);
        assert_eq!(
            local.format("%Y-%m-%d %-I%P").to_string(),
            "2026-07-27 11pm"
        );
    }

    #[test]
    fn observed_opus_limit_stays_separate_from_general_weekly() {
        let mut result = probe("ember");
        apply_limit(
            &mut result,
            &ObservedLimit {
                account: "ember".into(),
                model: ModelFamily::Opus,
                model_id: None,
                window: LimitWindow::Weekly,
                observed_at: 1,
                reset: 2,
                session_id: None,
                source: "test".into(),
            },
        );
        assert_eq!(
            result
                .quota
                .as_ref()
                .and_then(|quota| quota.weekly.as_ref())
                .map(|window| window.utilization),
            Some(0.2)
        );
        let usage = result.model_usage.unwrap();
        assert_eq!(usage.opus_weekly.unwrap().utilization, 1.0);
        assert_eq!(usage.opus_source, Some(ModelUsageSource::ObservedRejection));
    }

    #[test]
    fn weekly_rejection_uses_the_bound_sessions_model_family() {
        let binding = SessionBinding {
            session_id: "session".into(),
            account: "ember".into(),
            transcript_path: PathBuf::from("/tmp/transcript"),
            cwd: None,
            source: Some("resume".into()),
            observed_at: 1_785_185_840,
            pid: None,
            process_started_at: None,
            scan_offset: 0,
            last_model: Some("claude-opus-5".into()),
        };
        let limit = limit_from_message(
            &binding,
            "You've hit your weekly limit · resets Aug 1 at 9am (America/New_York)",
            1_785_185_840,
        )
        .unwrap();
        assert_eq!(limit.model, ModelFamily::Opus);
        assert_eq!(limit.model_id.as_deref(), Some("claude-opus-5"));
        assert_eq!(limit.window, LimitWindow::Weekly);
        assert_eq!(limit.account, "ember");
    }

    #[test]
    fn exact_model_rejection_becomes_a_scoped_bucket() {
        let mut result = probe("ember");
        apply_limit(
            &mut result,
            &ObservedLimit {
                account: "ember".into(),
                model: ModelFamily::Opus,
                model_id: Some("claude-opus-4-8".into()),
                window: LimitWindow::Weekly,
                observed_at: 1,
                reset: 2,
                session_id: Some("legacy-session".into()),
                source: "test".into(),
            },
        );
        let usage = result.model_usage.unwrap();
        assert!(usage.opus_weekly.is_none());
        assert_eq!(usage.scoped_weekly.len(), 1);
        assert_eq!(usage.scoped_weekly[0].key, "claudeopus48");
        assert_eq!(usage.scoped_weekly[0].window.utilization, 1.0);
        assert_eq!(
            usage.scoped_weekly[0].source,
            ModelUsageSource::ObservedRejection
        );
        assert_eq!(
            result
                .quota
                .as_ref()
                .and_then(|quota| quota.weekly.as_ref())
                .map(|window| window.utilization),
            Some(0.2)
        );
    }

    #[test]
    fn repeated_rejection_for_the_same_reset_is_not_new() {
        let first = ObservedLimit {
            account: "ember".into(),
            model: ModelFamily::Opus,
            model_id: None,
            window: LimitWindow::Weekly,
            observed_at: 100,
            reset: 1_000,
            session_id: Some("first".into()),
            source: "observed Claude rejection".into(),
        };
        let repeated = ObservedLimit {
            observed_at: 200,
            session_id: Some("retry".into()),
            ..first.clone()
        };
        let mut limits = Vec::new();
        assert!(merge_limit(&mut limits, first));
        assert!(!merge_limit(&mut limits, repeated));
        assert_eq!(limits.len(), 1);
        assert_eq!(limits[0].observed_at, 100);
        assert_eq!(limits[0].session_id.as_deref(), Some("first"));
    }

    #[test]
    fn rejection_for_a_new_reset_replaces_the_old_window() {
        let mut limits = vec![ObservedLimit {
            account: "ember".into(),
            model: ModelFamily::Opus,
            model_id: None,
            window: LimitWindow::Weekly,
            observed_at: 100,
            reset: 1_000,
            session_id: Some("old".into()),
            source: "observed Claude rejection".into(),
        }];
        let replacement = ObservedLimit {
            account: "ember".into(),
            model: ModelFamily::Opus,
            model_id: None,
            window: LimitWindow::Weekly,
            observed_at: 2_000,
            reset: 3_000,
            session_id: Some("new".into()),
            source: "observed Claude rejection".into(),
        };
        assert!(merge_limit(&mut limits, replacement));
        assert_eq!(limits[0].reset, 3_000);
        assert_eq!(limits[0].session_id.as_deref(), Some("new"));
    }

    #[test]
    fn transcript_cursor_ignores_forked_history_and_observes_only_new_failure() {
        let mut transcript = tempfile::NamedTempFile::new().unwrap();
        let observed_at = 1_785_185_840;
        let timestamp = |epoch| DateTime::from_timestamp(epoch, 0).unwrap().to_rfc3339();
        writeln!(
            transcript,
            "{}",
            json!({
                "type": "assistant",
                "error": "rate_limit",
                "timestamp": timestamp(observed_at - 60),
                "message": {
                    "model": "<synthetic>",
                    "content": [{"type": "text", "text": "weekly limit · resets Aug 1 at 9am (America/New_York)"}]
                }
            })
        )
        .unwrap();
        transcript.flush().unwrap();
        let boundary = transcript.as_file().metadata().unwrap().len();

        let mut binding = SessionBinding {
            session_id: "fork".into(),
            account: "ember".into(),
            transcript_path: transcript.path().to_owned(),
            cwd: None,
            source: Some("fork".into()),
            observed_at,
            pid: None,
            process_started_at: None,
            scan_offset: boundary,
            last_model: None,
        };
        writeln!(
            transcript,
            "{}",
            json!({
                "type": "assistant",
                "timestamp": timestamp(observed_at),
                "message": {"model": "claude-opus-5", "content": []}
            })
        )
        .unwrap();
        writeln!(
            transcript,
            "{}",
            json!({
                "type": "assistant",
                "error": "rate_limit",
                "timestamp": timestamp(observed_at + 1),
                "message": {
                    "model": "<synthetic>",
                    "content": [{"type": "text", "text": "You've hit your weekly limit · resets Aug 1 at 9am (America/New_York)"}]
                }
            })
        )
        .unwrap();
        transcript.flush().unwrap();

        let (limits, changed) =
            scan_binding_with_default_resolver(&mut binding, observed_at + 2, |_| None).unwrap();
        assert!(changed);
        assert_eq!(limits.len(), 1);
        assert_eq!(limits[0].model, ModelFamily::Opus);
        assert_eq!(limits[0].session_id.as_deref(), Some("fork"));
        assert_eq!(
            binding.scan_offset,
            transcript.as_file().metadata().unwrap().len()
        );
    }

    #[test]
    fn observer_hook_detection_does_not_match_unrelated_hooks() {
        assert!(is_observer_hook(
            "'/tmp/tokeman' rotate observe-session --pid \"$PPID\""
        ));
        assert!(!is_observer_hook("bash ~/.claude/hooks/postmark-nudge.sh"));
    }
}
