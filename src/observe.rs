//! What Claude Code itself reports about quota: the sessions the SessionStart
//! hook binds to an account, and the rate-limit rejections found in their
//! transcripts. The admission state file is written only here.
//!
//! A rejection is charged to the account settings offered when it happened
//! (the credential timeline), since a live process spends the current
//! default; the hook's binding is the fallback when the timeline is empty.

use std::collections::{BTreeMap, HashSet};
use std::fs::File;
use std::io::{BufRead, BufReader, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use chrono::{
    DateTime, Datelike, Duration as ChronoDuration, Local, NaiveDate, NaiveDateTime, NaiveTime,
    TimeZone, Utc,
};
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::Value;

use crate::account::Account;
use crate::claude::sessions;
use crate::claude::settings::{self, ACCOUNT as ACCOUNT_ENV};
use crate::fs::{FileLock, read_if_present, write_atomic};
use crate::history;
use crate::paths;
use crate::quota::{self, Bucket, ModelKey, Reading, Source, Verdict, Window};

/// Declares whether a session spends this machine's Anthropic subscription,
/// overriding the base-URL check: 1 for a session reaching Anthropic through
/// a gateway, 0 to keep a session out of the admission state.
pub const SPENDS_ENV: &str = "TOKEMAN_SESSION_SPENDS";
const BASE_URL_ENV: &str = "ANTHROPIC_BASE_URL";
const SESSION_RETENTION: ChronoDuration = ChronoDuration::days(14);
/// A rejection without a parsable reset is assumed to last this long.
const SESSION_FALLBACK: ChronoDuration = ChronoDuration::hours(6);
const WEEKLY_FALLBACK: ChronoDuration = ChronoDuration::days(8);
/// The hook has five seconds in total; its two `ps` lookups run together.
const HOOK_PS_TIMEOUT: Duration = Duration::from_secs(3);

/// Which limit a rejection names. `General` is the account-wide window.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Family {
    Opus,
    Sonnet,
    /// Metered by its own weekly limit (`7d_oi`).
    Fable,
    General,
}

impl Family {
    pub fn of(model: &str) -> Self {
        quota::Family::of(model).map_or(Self::General, Self::from)
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Opus => "Opus",
            Self::Sonnet => "Sonnet",
            Self::Fable => "Fable",
            Self::General => "general",
        }
    }

    /// The per-model weekly bucket family, if this is one.
    pub fn metered(self) -> Option<quota::Family> {
        match self {
            Self::Opus => Some(quota::Family::Opus),
            Self::Sonnet => Some(quota::Family::Sonnet),
            Self::Fable => Some(quota::Family::Fable),
            Self::General => None,
        }
    }
}

impl From<quota::Family> for Family {
    fn from(family: quota::Family) -> Self {
        match family {
            quota::Family::Opus => Self::Opus,
            quota::Family::Sonnet => Self::Sonnet,
            quota::Family::Fable => Self::Fable,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum LimitWindow {
    #[serde(rename = "five-hour")]
    Session,
    #[serde(rename = "weekly")]
    Weekly,
}

impl LimitWindow {
    pub fn label(self) -> &'static str {
        match self {
            Self::Session => "5h",
            Self::Weekly => "weekly",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum LimitSource {
    #[serde(rename = "observed Claude rejection")]
    Observed,
    #[serde(rename = "manual quarantine")]
    Manual,
}

/// One exhausted window, as a transcript or a quarantine reported it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ObservedLimit {
    pub account: String,
    #[serde(rename = "model")]
    pub family: Family,
    /// The concrete model, for a weekly rejection; `None` constrains the
    /// whole family.
    #[serde(
        rename = "model_id",
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "normalized_key"
    )]
    pub model: Option<ModelKey>,
    pub window: LimitWindow,
    pub observed_at: i64,
    #[serde(rename = "reset")]
    pub resets_at: i64,
    pub session_id: Option<String>,
    pub source: LimitSource,
}

/// Older files hold the raw model id (`claude-opus-4-8`); read it as a key.
fn normalized_key<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<ModelKey>, D::Error> {
    Ok(Option::<String>::deserialize(deserializer)?
        .map(|model| ModelKey::new(&model))
        .filter(|key| !key.is_empty()))
}

impl ObservedLimit {
    pub fn active_at(&self, now: i64) -> bool {
        self.resets_at > now
    }

    /// `Opus 4.8`, or the family for a family-wide limit.
    pub fn subject(&self) -> String {
        match &self.model {
            Some(model) => model_label(model),
            None => self.family.label().to_owned(),
        }
    }

    pub fn summary(&self) -> String {
        format!(
            "{} {} rejected until {}",
            self.subject(),
            self.window.label(),
            format_reset(self.resets_at)
        )
    }
}

/// `opus48` reads as `Opus 4.8`, the spelling the profile endpoint uses.
fn model_label(key: &ModelKey) -> String {
    match (key.family(), key.version()) {
        (Some(family), Some(version)) if key.as_str() != family.name() => {
            let (major, minor) = version.split_at(1);
            if minor.is_empty() {
                format!("{} {major}", family.label())
            } else {
                format!("{} {major}.{minor}", family.label())
            }
        }
        (Some(family), _) => family.label().to_owned(),
        (None, _) => key.to_string(),
    }
}

/// What the SessionStart hook recorded for one process.
#[derive(Debug, Clone, Serialize)]
pub struct Binding {
    pub session_id: String,
    pub account: String,
    pub transcript_path: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct SessionBinding {
    session_id: String,
    account: String,
    transcript_path: PathBuf,
    #[serde(default)]
    cwd: Option<PathBuf>,
    /// The hook's `source`: startup, resume, clear, compact.
    #[serde(default)]
    source: Option<String>,
    observed_at: i64,
    #[serde(default)]
    pid: Option<u32>,
    #[serde(default)]
    process_started_at: Option<i64>,
    /// Transcript bytes already read; everything before the hook fired is
    /// history and never scanned.
    #[serde(default)]
    scan_offset: u64,
    /// The last model the transcript named, which a rejection belongs to.
    #[serde(default)]
    last_model: Option<String>,
}

#[derive(Debug, Default, PartialEq, Serialize, Deserialize)]
struct AdmissionState {
    #[serde(default)]
    sessions: BTreeMap<String, SessionBinding>,
    #[serde(default)]
    limits: Vec<ObservedLimit>,
}

#[derive(Debug, Deserialize)]
struct HookInput {
    session_id: String,
    transcript_path: PathBuf,
    #[serde(default)]
    cwd: Option<PathBuf>,
    #[serde(default)]
    source: Option<String>,
}

fn load_state() -> AdmissionState {
    read_if_present(&paths::admission_state())
        .ok()
        .flatten()
        .and_then(|contents| serde_json::from_str(&contents).ok())
        .unwrap_or_default()
}

fn save_state(state: &AdmissionState) -> Result<()> {
    let mut bytes = serde_json::to_vec_pretty(state)?;
    bytes.push(b'\n');
    write_atomic(&paths::admission_state(), &bytes, 0o600)
}

fn lock() -> Result<FileLock> {
    FileLock::acquire_announced(&paths::admission_lock(), "the admission state lock")
}

fn prune(state: &mut AdmissionState, now: i64) {
    state.limits.retain(|limit| limit.active_at(now));
    let oldest = now - SESSION_RETENTION.num_seconds();
    state
        .sessions
        .retain(|_, session| session.observed_at >= oldest);
}

// Which sessions count.

fn parse_flag(raw: &str) -> Option<bool> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Some(true),
        "0" | "false" | "no" | "off" => Some(false),
        _ => None,
    }
}

fn env_flag(name: &str) -> Option<bool> {
    std::env::var(name).ok().as_deref().and_then(parse_flag)
}

fn is_anthropic_base_url(url: &str) -> bool {
    let rest = url.split_once("://").map_or(url, |(_, rest)| rest);
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    let host_port = authority
        .rsplit_once('@')
        .map_or(authority, |(_, host)| host);
    // A bracketed IPv6 literal keeps its colons; only a trailing :port goes.
    let host = match host_port.rfind(':') {
        Some(index) if !host_port[index + 1..].contains(']') => &host_port[..index],
        _ => host_port,
    };
    let host = host
        .trim_matches(['[', ']'])
        .trim_end_matches('.')
        .to_ascii_lowercase();
    host == "anthropic.com" || host.ends_with(".anthropic.com")
}

/// Whether the session the hook fired for spends the subscription. Hooks
/// inherit the session's environment: a Bedrock, Vertex or third-party
/// endpoint session must not be bound to an Anthropic account, or its own
/// provider's limit messages would be charged to one.
fn spends_anthropic_quota() -> bool {
    if let Some(explicit) = env_flag(SPENDS_ENV) {
        return explicit;
    }
    if ["CLAUDE_CODE_USE_BEDROCK", "CLAUDE_CODE_USE_VERTEX"]
        .into_iter()
        .any(|flag| env_flag(flag).unwrap_or(false))
    {
        return false;
    }
    match std::env::var(BASE_URL_ENV) {
        Ok(url) if !url.trim().is_empty() => is_anthropic_base_url(url.trim()),
        _ => true,
    }
}

// The SessionStart hook.

/// Record the session described by the hook's stdin for Claude process `pid`
/// (0 when unknown).
pub async fn record_session_from_hook(pid: u32, input: &[u8]) -> Result<()> {
    if !spends_anthropic_quota() {
        return Ok(());
    }
    let now = Utc::now().timestamp();
    let account = match history::account_at(&history::timeline(), now) {
        Some(Some(account)) => account,
        Some(None) => return Ok(()),
        None => match std::env::var(ACCOUNT_ENV) {
            Ok(account) if !account.trim().is_empty() => account,
            _ => return Ok(()),
        },
    };
    let input: HookInput =
        serde_json::from_slice(input).context("invalid Claude SessionStart hook input")?;
    if input.session_id.trim().is_empty() {
        bail!("Claude SessionStart hook omitted session_id");
    }

    let scan_offset = std::fs::metadata(&input.transcript_path)
        .map(|metadata| metadata.len())
        .unwrap_or(0);
    let (started_at, model) = if pid > 0 {
        process_facts(pid).await
    } else {
        (None, None)
    };
    let last_model = model
        .or_else(|| infer_last_model(&input.transcript_path, scan_offset))
        .or_else(configured_model);
    let binding = SessionBinding {
        session_id: input.session_id.clone(),
        account,
        transcript_path: input.transcript_path,
        cwd: input.cwd,
        source: input.source,
        observed_at: now,
        pid: (pid > 0).then_some(pid),
        process_started_at: started_at,
        scan_offset,
        last_model,
    };

    let _lock = lock()?;
    let mut state = load_state();
    prune(&mut state, now);
    state.sessions.insert(input.session_id, binding);
    save_state(&state)
}

/// When the process started and the `--model` on its command line.
async fn process_facts(pid: u32) -> (Option<i64>, Option<String>) {
    let pid = pid.to_string();
    let Ok(row) = sessions::ps(&["-p", &pid, "-o", "lstart=,command="], HOOK_PS_TIMEOUT).await
    else {
        return (None, None);
    };
    let fields: Vec<&str> = row.split_whitespace().collect();
    if fields.len() < 5 {
        return (None, None);
    }
    let started_at = sessions::parse_lstart(&fields[..5].join(" ")).map(|at| at.timestamp());
    (started_at, model_argument(&fields[5..]))
}

fn model_argument(fields: &[&str]) -> Option<String> {
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

fn configured_model() -> Option<String> {
    let settings = settings::load().ok()?;
    settings::target_model(&settings).map(str::to_owned)
}

/// The last model named in the transcript's first `through` bytes.
fn infer_last_model(path: &Path, through: u64) -> Option<String> {
    let mut reader = BufReader::new(File::open(path).ok()?);
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
    if let Some(model) = value
        .get("message")
        .and_then(|message| message.get("model"))
        .and_then(Value::as_str)
        .filter(|model| !model.is_empty() && *model != "<synthetic>")
    {
        *last_model = Some(model.to_owned());
    }
}

// Transcript scanning.

fn event_timestamp(value: &Value) -> Option<i64> {
    DateTime::parse_from_rfc3339(value.get("timestamp")?.as_str()?)
        .ok()
        .map(|value| value.timestamp())
}

/// The text of a top-level assistant `rate_limit` record.
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
        LimitWindow::Session
    } else {
        return None;
    };
    let last_model = binding
        .last_model
        .as_deref()
        .filter(|model| !model.is_empty());
    let family = last_model.map_or(Family::General, Family::of);
    let resets_at = parse_reset_from_message(message, observed_at).unwrap_or_else(|| {
        observed_at
            + match window {
                LimitWindow::Session => SESSION_FALLBACK.num_seconds(),
                LimitWindow::Weekly => WEEKLY_FALLBACK.num_seconds(),
            }
    });
    Some(ObservedLimit {
        account: binding.account.clone(),
        family,
        // Weekly limits are per model: Opus 4.8 may reject while Opus 5 on the
        // same account succeeds. A session limit is account-wide.
        model: (window == LimitWindow::Weekly)
            .then(|| last_model.map(ModelKey::new))
            .flatten()
            .filter(|key| !key.is_empty()),
        window,
        observed_at,
        resets_at,
        session_id: Some(binding.session_id.clone()),
        source: LimitSource::Observed,
    })
}

/// Merge one rejection; `true` when it describes a newly exhausted window.
/// Repeated prompts against the same wall keep the first observation.
fn merge_limit(limits: &mut Vec<ObservedLimit>, incoming: ObservedLimit) -> bool {
    match limits.iter_mut().find(|existing| {
        existing.account == incoming.account
            && existing.family == incoming.family
            && existing.model == incoming.model
            && existing.window == incoming.window
    }) {
        Some(existing) => {
            if incoming.resets_at != existing.resets_at
                && incoming.observed_at >= existing.observed_at
            {
                *existing = incoming;
                true
            } else {
                false
            }
        }
        None => {
            limits.push(incoming);
            true
        }
    }
}

/// Read the transcript lines appended since the cursor. `account_at` says
/// which account settings offered at a moment: outer `None` keeps the
/// binding's account, inner `None` (no managed credential) drops the limit.
fn scan_binding(
    binding: &mut SessionBinding,
    now: i64,
    account_at: impl Fn(i64) -> Option<Option<String>>,
) -> Result<(Vec<ObservedLimit>, bool)> {
    let Ok(file) = File::open(&binding.transcript_path) else {
        return Ok((Vec::new(), false));
    };
    let len = file.metadata().map(|metadata| metadata.len()).unwrap_or(0);
    if len < binding.scan_offset {
        // Rewritten (compacted) transcript: skip its history; the next hook
        // sets an exact boundary again.
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
        if reader.read_line(&mut line)? == 0 {
            break;
        }
        if !line.ends_with('\n') {
            // A record still being appended is read next time.
            reader.seek(SeekFrom::Start(line_start))?;
            break;
        }
        let Ok(value) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        update_last_model(&value, &mut binding.last_model);
        let Some(message) = rate_limit_message(&value) else {
            continue;
        };
        let observed_at = event_timestamp(&value).unwrap_or(now);
        // A forked transcript carries older records past the byte boundary.
        if observed_at + 2 < binding.observed_at {
            continue;
        }
        let Some(mut limit) = limit_from_message(binding, &message, observed_at) else {
            continue;
        };
        match account_at(observed_at) {
            Some(Some(account)) => limit.account = account,
            Some(None) => continue,
            None => {}
        }
        incoming.push(limit);
    }
    let new_offset = reader.stream_position()?;
    let changed = new_offset != binding.scan_offset;
    binding.scan_offset = new_offset;
    Ok((incoming, changed))
}

/// Scan every bound session's transcript past its cursor. Returns how many
/// newly exhausted windows were seen, which a caller may rotate on at once.
pub fn scan_transcripts(accounts: &[Account]) -> Result<usize> {
    let _lock = lock()?;
    let mut state = load_state();
    let now = Utc::now().timestamp();
    prune(&mut state, now);
    let configured: HashSet<&str> = accounts
        .iter()
        .map(|account| account.name.as_str())
        .collect();
    let timeline = history::timeline();
    let mut incoming = Vec::new();
    let mut changed = false;
    for binding in state.sessions.values_mut() {
        if !configured.contains(binding.account.as_str()) {
            continue;
        }
        let (limits, scanned) =
            scan_binding(binding, now, |at| history::account_at(&timeline, at))?;
        incoming.extend(limits);
        changed |= scanned;
    }
    let mut observed = 0;
    for limit in incoming {
        observed += usize::from(merge_limit(&mut state.limits, limit));
        changed = true;
    }
    if changed {
        save_state(&state)?;
    }
    Ok(observed)
}

/// Limits whose reset is still ahead, ordered by account, family, model, window.
pub fn active_limits() -> Vec<ObservedLimit> {
    let now = Utc::now().timestamp();
    let mut limits: Vec<ObservedLimit> = load_state()
        .limits
        .into_iter()
        .filter(|limit| limit.active_at(now))
        .collect();
    limits.sort_by(|a, b| {
        a.account
            .cmp(&b.account)
            .then_with(|| a.family.cmp(&b.family))
            .then_with(|| a.model.cmp(&b.model))
            .then_with(|| a.window.cmp(&b.window))
    });
    limits
}

/// Overlay the active limits on probe readings.
pub fn apply(readings: &mut [Reading]) {
    let limits = active_limits();
    for reading in readings {
        for limit in &limits {
            if limit.account == reading.account {
                apply_limit(reading, limit);
            }
        }
    }
}

fn apply_limit(reading: &mut Reading, limit: &ObservedLimit) {
    let full = Window::new(1.0, limit.resets_at);
    match (limit.window, limit.family, &limit.model) {
        // A concrete model's wall constrains that model, not its family.
        (LimitWindow::Weekly, _, Some(model)) => reject_bucket(reading, model.clone(), full),
        (LimitWindow::Weekly, Family::General, None) => {
            if reading.error.is_none() {
                reading.weekly = Some(full);
                reading.verdict = Verdict::Rejected;
                reading.claim = Some("seven_day".into());
            }
        }
        // The same bucket the `7d_oi` header and the profile report under.
        (LimitWindow::Weekly, family, None) => {
            if let Some(family) = family.metered() {
                reject_bucket(reading, ModelKey::family_wide(family), full);
            }
        }
        (LimitWindow::Session, _, _) => {
            if reading.error.is_none() {
                reading.session = Some(full);
                reading.verdict = Verdict::Rejected;
                reading.claim = Some("five_hour".into());
            }
        }
    }
}

fn reject_bucket(reading: &mut Reading, key: ModelKey, window: Window) {
    let label = reading
        .bucket(&key)
        .map_or_else(|| model_label(&key), |bucket| bucket.label.clone());
    reading.set_bucket(Bucket {
        key,
        label,
        window,
        source: Source::Observed,
    });
}

// Quarantine.

/// Record a limit by hand. `model` must belong to `family` when given.
pub fn quarantine(
    account: &str,
    family: Family,
    model: Option<ModelKey>,
    window: LimitWindow,
    resets_at: i64,
) -> Result<()> {
    let now = Utc::now().timestamp();
    if resets_at <= now {
        bail!("quarantine reset must be in the future");
    }
    if let Some(model) = &model
        && Family::of(model.as_str()) != family
    {
        bail!(
            "model '{model}' does not belong to the {} family",
            family.label()
        );
    }
    let _lock = lock()?;
    let mut state = load_state();
    prune(&mut state, now);
    merge_limit(
        &mut state.limits,
        ObservedLimit {
            account: account.into(),
            family,
            model: model.filter(|key| !key.is_empty()),
            window,
            observed_at: now,
            resets_at,
            session_id: None,
            source: LimitSource::Manual,
        },
    );
    save_state(&state)
}

/// Remove an account's limits, or only one family's. Returns how many went.
pub fn clear_quarantine(account: &str, family: Option<Family>) -> Result<usize> {
    let _lock = lock()?;
    let mut state = load_state();
    let before = state.limits.len();
    state.limits.retain(|limit| {
        limit.account != account || family.is_some_and(|family| limit.family != family)
    });
    let removed = before - state.limits.len();
    if removed > 0 {
        save_state(&state)?;
    }
    Ok(removed)
}

/// The hook's record for a live process, matched by pid and start time
/// (within two seconds, the resolution `ps` reports).
pub fn binding_for_pid(pid: u32, started_at: i64) -> Option<Binding> {
    load_state()
        .sessions
        .into_values()
        .filter(|binding| binding.pid == Some(pid))
        .filter(|binding| {
            binding
                .process_started_at
                .is_some_and(|started| (started - started_at).abs() <= 2)
        })
        .max_by_key(|binding| binding.observed_at)
        .map(|binding| Binding {
            session_id: binding.session_id,
            account: binding.account,
            transcript_path: binding.transcript_path,
        })
}

// Times.

/// An epoch from `--until`: RFC3339 or Unix seconds.
pub fn parse_reset_spec(value: &str) -> Result<i64> {
    if let Ok(epoch) = value.parse::<i64>() {
        return Ok(epoch);
    }
    DateTime::parse_from_rfc3339(value)
        .map(|value| value.timestamp())
        .with_context(|| format!("invalid reset '{value}'; use RFC3339 or a Unix timestamp"))
}

/// A wall-clock time in the local zone; the earlier reading when it is
/// ambiguous, `None` when it does not exist.
pub(crate) fn local_time(naive: NaiveDateTime) -> Option<DateTime<Local>> {
    let resolved = Local.from_local_datetime(&naive);
    resolved.single().or_else(|| resolved.earliest())
}

/// The reset in `resets Aug 1 at 9am (America/New_York)` (weekly) or
/// `resets 11pm (America/New_York)` (same day, else tomorrow).
fn parse_reset_from_message(message: &str, observed_at: i64) -> Option<i64> {
    let reset_text = message.split_once("resets ")?.1;
    let reset_text = reset_text.split(" (").next().unwrap_or(reset_text).trim();
    let observed = DateTime::from_timestamp(observed_at, 0)?.with_timezone(&Local);

    if let Some((date_text, time_text)) = reset_text.split_once(" at ") {
        let time = parse_clock(time_text)?;
        for year in [observed.year(), observed.year() + 1] {
            let date =
                NaiveDate::parse_from_str(&format!("{year} {date_text}"), "%Y %b %e").ok()?;
            let local = local_time(date.and_time(time))?;
            if local.timestamp() > observed_at {
                return Some(local.timestamp());
            }
        }
        return None;
    }

    let time = parse_clock(reset_text)?;
    let date = observed.date_naive();
    let local = local_time(date.and_time(time))?;
    if local.timestamp() > observed_at {
        return Some(local.timestamp());
    }
    Some(local_time(date.succ_opt()?.and_time(time))?.timestamp())
}

/// `9am`, `11:30PM`.
fn parse_clock(value: &str) -> Option<NaiveTime> {
    let normalized = value.trim().to_ascii_uppercase();
    let (clock, suffix) = normalized.split_at(normalized.len().checked_sub(2)?);
    if suffix != "AM" && suffix != "PM" {
        return None;
    }
    let (hour, minute) = match clock.split_once(':') {
        Some((hour, minute)) => (hour.parse::<u32>().ok()?, minute.parse::<u32>().ok()?),
        None => (clock.parse::<u32>().ok()?, 0),
    };
    if !(1..=12).contains(&hour) || minute >= 60 {
        return None;
    }
    let hour = match (hour, suffix) {
        (12, "AM") => 0,
        (12, "PM") => 12,
        (hour, "PM") => hour + 12,
        (hour, _) => hour,
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
    use serde_json::json;
    use std::io::Write;

    fn reading(name: &str) -> Reading {
        Reading {
            account: name.into(),
            at: 0,
            verdict: Verdict::Allowed,
            claim: None,
            session: Some(Window::new(0.1, 0)),
            weekly: Some(Window::new(0.2, 0)),
            buckets: Vec::new(),
            overage: None,
            error: None,
            profile_error: None,
        }
    }

    fn binding(last_model: Option<&str>) -> SessionBinding {
        SessionBinding {
            session_id: "session".into(),
            account: "ember".into(),
            transcript_path: PathBuf::from("/nonexistent/transcript"),
            cwd: None,
            source: Some("resume".into()),
            observed_at: 1_785_185_840,
            pid: None,
            process_started_at: None,
            scan_offset: 0,
            last_model: last_model.map(str::to_owned),
        }
    }

    fn limit(family: Family, model: Option<&str>, window: LimitWindow) -> ObservedLimit {
        ObservedLimit {
            account: "ember".into(),
            family,
            model: model.map(ModelKey::new),
            window,
            observed_at: 1,
            resets_at: 2,
            session_id: None,
            source: LimitSource::Manual,
        }
    }

    const WEEKLY_MESSAGE: &str =
        "You've hit your weekly limit · resets Aug 1 at 9am (America/New_York)";

    #[test]
    fn anthropic_base_urls_are_recognised() {
        for url in [
            "https://api.anthropic.com",
            "https://api.anthropic.com/v1",
            "https://api.anthropic.com:443/v1/messages",
            "HTTPS://API.ANTHROPIC.COM/",
            "https://anthropic.com",
        ] {
            assert!(is_anthropic_base_url(url), "expected Anthropic: {url}");
        }
    }

    #[test]
    fn third_party_base_urls_are_not_anthropic() {
        for url in [
            "https://api.z.ai/api/anthropic",
            "https://api.kimi.com/coding",
            "https://cli-chat-proxy.grok.com",
            "http://127.0.0.1:8317",
            "http://localhost:8317/v1",
            "https://api.anthropic.com.evil.test/v1",
            "https://notanthropic.com",
        ] {
            assert!(!is_anthropic_base_url(url), "expected third party: {url}");
        }
    }

    #[test]
    fn env_flag_parses_the_usual_spellings() {
        assert_eq!(env_flag("TOKEMAN_TEST_FLAG_UNSET_XYZ"), None);
        for (raw, want) in [
            ("1", true),
            ("true", true),
            ("YES", true),
            (" on ", true),
            ("0", false),
            ("false", false),
            ("No", false),
            ("off", false),
        ] {
            assert_eq!(parse_flag(raw), Some(want), "raw={raw:?}");
        }
        assert_eq!(parse_flag("banana"), None);
    }

    #[test]
    fn parses_weekly_reset_with_date() {
        let observed = DateTime::parse_from_rfc3339("2026-07-27T20:17:20Z")
            .unwrap()
            .timestamp();
        let reset = parse_reset_from_message(WEEKLY_MESSAGE, observed).unwrap();
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
        let reset =
            parse_reset_from_message("session limit · resets 3pm (America/New_York)", observed)
                .unwrap();
        let local = DateTime::from_timestamp(reset, 0)
            .unwrap()
            .with_timezone(&Local);
        assert_eq!(local.format("%Y-%m-%d %-I%P").to_string(), "2026-07-28 3pm");
        assert_eq!(parse_clock("12:30am"), NaiveTime::from_hms_opt(0, 30, 0));
        assert_eq!(parse_clock("13pm"), None);
    }

    #[test]
    fn observed_opus_limit_stays_separate_from_general_weekly() {
        let mut result = reading("ember");
        apply_limit(&mut result, &limit(Family::Opus, None, LimitWindow::Weekly));
        assert_eq!(result.weekly.map(|window| window.used), Some(0.2));
        assert_eq!(result.verdict, Verdict::Allowed);
        let bucket = result.bucket(&ModelKey::new("opus")).unwrap();
        assert_eq!(bucket.window.used, 1.0);
        assert_eq!(bucket.window.resets_at, 2);
        assert_eq!(bucket.source, Source::Observed);
        assert_eq!(bucket.label, "Opus");
    }

    #[test]
    fn general_and_session_rejections_mark_the_reading_rejected() {
        let mut result = reading("ember");
        apply_limit(
            &mut result,
            &limit(Family::General, None, LimitWindow::Weekly),
        );
        assert_eq!(result.verdict, Verdict::Rejected);
        assert_eq!(result.claim.as_deref(), Some("seven_day"));
        assert_eq!(result.weekly, Some(Window::new(1.0, 2)));
        assert_eq!(result.session.map(|window| window.used), Some(0.1));

        let mut result = reading("ember");
        apply_limit(
            &mut result,
            &limit(Family::Opus, Some("claude-opus-5"), LimitWindow::Session),
        );
        assert_eq!(result.verdict, Verdict::Rejected);
        assert_eq!(result.claim.as_deref(), Some("five_hour"));
        assert_eq!(result.session, Some(Window::new(1.0, 2)));
        assert!(result.buckets.is_empty());

        let mut failed = Reading::failed("ember", 0, quota::ProbeError::NoCredential);
        apply_limit(
            &mut failed,
            &limit(Family::General, None, LimitWindow::Session),
        );
        assert_eq!(failed.session, None);
        assert_eq!(failed.verdict, Verdict::Unknown);
    }

    #[test]
    fn weekly_rejection_uses_the_bound_sessions_model_family() {
        let binding = binding(Some("claude-opus-5"));
        let limit = limit_from_message(&binding, WEEKLY_MESSAGE, 1_785_185_840).unwrap();
        assert_eq!(limit.family, Family::Opus);
        assert_eq!(limit.model, Some(ModelKey::new("claude-opus-5")));
        assert_eq!(limit.window, LimitWindow::Weekly);
        assert_eq!(limit.account, "ember");
        assert_eq!(limit.source, LimitSource::Observed);
        assert_eq!(limit.session_id.as_deref(), Some("session"));

        let session = limit_from_message(
            &binding,
            "session limit · resets 11pm (America/New_York)",
            1_785_185_840,
        )
        .unwrap();
        assert_eq!(session.window, LimitWindow::Session);
        assert_eq!(session.model, None, "a session limit is account-wide");
        assert_eq!(
            limit_from_message(&binding, "something else entirely", 1_785_185_840),
            None
        );
        let unknown = limit_from_message(&binding, "weekly limit reached", 1_785_185_840).unwrap();
        assert_eq!(
            unknown.resets_at,
            1_785_185_840 + WEEKLY_FALLBACK.num_seconds()
        );
    }

    #[test]
    fn exact_model_rejection_becomes_a_scoped_bucket() {
        let mut result = reading("ember");
        apply_limit(
            &mut result,
            &limit(Family::Opus, Some("claude-opus-4-8"), LimitWindow::Weekly),
        );
        assert!(result.bucket(&ModelKey::new("opus")).is_none());
        assert_eq!(result.buckets.len(), 1);
        assert_eq!(result.buckets[0].key.as_str(), "opus48");
        assert_eq!(result.buckets[0].label, "Opus 4.8");
        assert_eq!(result.buckets[0].window.used, 1.0);
        assert_eq!(result.buckets[0].source, Source::Observed);
        assert_eq!(result.weekly.map(|window| window.used), Some(0.2));
    }

    #[test]
    fn fable_weekly_rejection_is_a_fable_bucket_not_the_general_weekly() {
        let binding = binding(Some("claude-fable-5-1"));
        let limit = limit_from_message(&binding, WEEKLY_MESSAGE, 1_785_185_840).unwrap();
        assert_eq!(limit.family, Family::Fable);
        assert_eq!(limit.model, Some(ModelKey::new("fable51")));

        let mut result = reading("ember");
        apply_limit(&mut result, &limit);
        assert_eq!(result.verdict, Verdict::Allowed);
        assert_eq!(result.weekly.map(|window| window.used), Some(0.2));
        assert_eq!(result.buckets.len(), 1);
        assert_eq!(result.buckets[0].key.as_str(), "fable51");
        assert_eq!(result.buckets[0].label, "Fable 5.1");
        assert_eq!(result.buckets[0].window.used, 1.0);
        assert_eq!(result.buckets[0].source, Source::Observed);
    }

    #[test]
    fn a_family_wide_fable_limit_lands_on_the_shared_fable_bucket() {
        let mut result = reading("ember");
        result.buckets.push(Bucket {
            key: ModelKey::new("fable"),
            label: "Fable".into(),
            window: Window::new(0.4, 1),
            source: Source::Headers,
        });
        apply_limit(
            &mut result,
            &limit(Family::Fable, None, LimitWindow::Weekly),
        );
        assert_eq!(result.buckets.len(), 1);
        assert_eq!(result.buckets[0].key.as_str(), "fable");
        assert_eq!(result.buckets[0].label, "Fable");
        assert_eq!(result.buckets[0].window, Window::new(1.0, 2));
        assert_eq!(result.buckets[0].source, Source::Observed);
        assert_eq!(result.weekly.map(|window| window.used), Some(0.2));
        assert_eq!(Family::of("claude-fable-5-1"), Family::Fable);
        assert_eq!(Family::of("claude-haiku-4-5"), Family::General);
        assert_eq!(
            Family::of(ModelKey::new("haiku45").as_str()),
            Family::General
        );
        assert_eq!(Family::of(ModelKey::new("Opus 4.8").as_str()), Family::Opus);
    }

    #[test]
    fn repeated_rejection_for_the_same_reset_is_not_new() {
        let first = ObservedLimit {
            observed_at: 100,
            resets_at: 1_000,
            session_id: Some("first".into()),
            source: LimitSource::Observed,
            ..limit(Family::Opus, None, LimitWindow::Weekly)
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
            observed_at: 100,
            resets_at: 1_000,
            session_id: Some("old".into()),
            ..limit(Family::Opus, None, LimitWindow::Weekly)
        }];
        let replacement = ObservedLimit {
            observed_at: 2_000,
            resets_at: 3_000,
            session_id: Some("new".into()),
            ..limit(Family::Opus, None, LimitWindow::Weekly)
        };
        assert!(merge_limit(&mut limits, replacement));
        assert_eq!(limits[0].resets_at, 3_000);
        assert_eq!(limits[0].session_id.as_deref(), Some("new"));
    }

    #[test]
    fn transcript_cursor_ignores_forked_history_and_observes_only_new_failure() {
        let mut transcript = tempfile::NamedTempFile::new().unwrap();
        let observed_at = 1_785_185_840;
        let timestamp = |epoch| DateTime::from_timestamp(epoch, 0).unwrap().to_rfc3339();
        let rejection = |at| {
            json!({
                "type": "assistant",
                "error": "rate_limit",
                "timestamp": timestamp(at),
                "message": {
                    "model": "<synthetic>",
                    "content": [{"type": "text", "text": WEEKLY_MESSAGE}]
                }
            })
        };
        writeln!(transcript, "{}", rejection(observed_at - 60)).unwrap();
        transcript.flush().unwrap();
        let boundary = transcript.as_file().metadata().unwrap().len();

        let mut binding = SessionBinding {
            session_id: "fork".into(),
            source: Some("fork".into()),
            observed_at,
            scan_offset: boundary,
            last_model: None,
            ..binding(None)
        };
        binding.transcript_path = transcript.path().to_owned();
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
        writeln!(transcript, "{}", rejection(observed_at + 1)).unwrap();
        transcript.flush().unwrap();

        let (limits, changed) = scan_binding(&mut binding, observed_at + 2, |_| None).unwrap();
        assert!(changed);
        assert_eq!(limits.len(), 1);
        assert_eq!(limits[0].family, Family::Opus);
        assert_eq!(limits[0].account, "ember");
        assert_eq!(limits[0].session_id.as_deref(), Some("fork"));
        assert_eq!(
            binding.scan_offset,
            transcript.as_file().metadata().unwrap().len()
        );
        assert_eq!(binding.last_model.as_deref(), Some("claude-opus-5"));

        // Nothing new: no change. A partial line is left for next time.
        let (limits, changed) = scan_binding(&mut binding, observed_at + 3, |_| None).unwrap();
        assert!(limits.is_empty() && !changed);
        write!(transcript, "{{\"type\":\"assistant\"").unwrap();
        transcript.flush().unwrap();
        let before = binding.scan_offset;
        let (_, changed) = scan_binding(&mut binding, observed_at + 3, |_| None).unwrap();
        assert!(!changed);
        assert_eq!(binding.scan_offset, before);

        // The timeline reattributes a rejection, and a pause drops it.
        writeln!(transcript, "}}").unwrap();
        writeln!(transcript, "{}", rejection(observed_at + 10)).unwrap();
        transcript.flush().unwrap();
        let (limits, _) = scan_binding(&mut binding, observed_at + 11, |_| {
            Some(Some("other".into()))
        })
        .unwrap();
        assert_eq!(limits[0].account, "other");
        writeln!(transcript, "{}", rejection(observed_at + 20)).unwrap();
        transcript.flush().unwrap();
        let (limits, _) = scan_binding(&mut binding, observed_at + 21, |_| Some(None)).unwrap();
        assert!(limits.is_empty());

        // A rewritten (shorter) transcript resets the cursor without replay.
        std::fs::write(transcript.path(), "").unwrap();
        let (limits, changed) = scan_binding(&mut binding, observed_at + 30, |_| None).unwrap();
        assert!(limits.is_empty() && changed);
        assert_eq!(binding.scan_offset, 0);
    }

    #[test]
    fn the_state_file_keeps_its_stored_spelling() {
        let json = r#"{"sessions":{"s":{"session_id":"s","account":"ember","transcript_path":"/t","observed_at":5,"pid":7,"process_started_at":3,"scan_offset":9,"last_model":"claude-opus-5"}},"limits":[{"account":"ember","model":"opus","model_id":"claude-opus-4-8","window":"five-hour","observed_at":1,"reset":2,"session_id":null,"source":"observed Claude rejection"},{"account":"ember","model":"general","window":"weekly","observed_at":1,"reset":2,"session_id":null,"source":"manual quarantine"}]}"#;
        let state: AdmissionState = serde_json::from_str(json).unwrap();
        assert_eq!(state.limits[0].family, Family::Opus);
        assert_eq!(state.limits[0].model, Some(ModelKey::new("opus48")));
        assert_eq!(state.limits[0].window, LimitWindow::Session);
        assert_eq!(state.limits[0].source, LimitSource::Observed);
        assert_eq!(state.limits[1].family, Family::General);
        assert_eq!(state.limits[1].model, None);
        assert_eq!(state.limits[1].source, LimitSource::Manual);
        assert_eq!(state.sessions["s"].cwd, None);
        let rendered = serde_json::to_string(&state).unwrap();
        assert_eq!(
            rendered,
            json.replace("\"claude-opus-4-8\"", "\"opus48\"").replace(
                "\"transcript_path\":\"/t\",",
                "\"transcript_path\":\"/t\",\"cwd\":null,\"source\":null,"
            )
        );
        assert_eq!(
            serde_json::from_str::<AdmissionState>(&rendered).unwrap(),
            state
        );
    }

    #[test]
    fn limits_describe_themselves_and_bindings_match_by_start_time() {
        let limit = limit(Family::Opus, Some("claude-opus-4-8"), LimitWindow::Weekly);
        assert!(
            limit
                .summary()
                .starts_with("Opus 4.8 weekly rejected until ")
        );
        assert_eq!(
            limit_from_message(&binding(None), WEEKLY_MESSAGE, 1)
                .unwrap()
                .subject(),
            "general"
        );
        assert_eq!(model_label(&ModelKey::new("opus")), "Opus");
        assert_eq!(model_label(&ModelKey::new("Fable 5")), "Fable 5");
        assert_eq!(model_label(&ModelKey::new("haiku45")), "haiku45");
        assert!(limit.active_at(1) && !limit.active_at(2));
        assert_eq!(parse_reset_spec("1700000000").unwrap(), 1_700_000_000);
        assert_eq!(parse_reset_spec("1970-01-02T00:00:00Z").unwrap(), 86_400);
        assert!(parse_reset_spec("tomorrow").is_err());
        assert_eq!(
            model_argument(&["node", "claude", "--model", "opus"]).as_deref(),
            Some("opus")
        );
        assert_eq!(
            model_argument(&["claude", "--model=claude-fable-5-1"]).as_deref(),
            Some("claude-fable-5-1")
        );
        assert_eq!(model_argument(&["claude", "--model"]), None);
    }
}
