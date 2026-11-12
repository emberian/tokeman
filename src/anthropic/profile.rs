//! `/api/oauth/usage`: per-model weekly limits and reset offers, read the way
//! Claude Code reads them, behind an on-disk cache.
//!
//! The endpoint answers 429 under a 20-second probe cadence and the figures
//! move over hours, so each account's answer is reused for [`MAX_AGE_SECS`].
//! A 429 backs off the account that drew it, and once two accounts have
//! drawn one, the whole fleet: the budget is shared by every account from
//! one address, but one account can also be refused on its own. While a
//! backoff lasts, each held account's last good answer is served. This
//! module is the only writer of the cache file.

use std::collections::BTreeMap;
use std::path::PathBuf;

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};

use super::{Client, describe, one_line, retry_after_secs};
use crate::account::Account;
use crate::fs::write_atomic;
use crate::paths;
use crate::quota::{Limit, ModelKey, Reading, Scope, SessionLimit, Source, Span, Window};

/// How long an answer is reused before the endpoint is asked again.
pub const MAX_AGE_SECS: i64 = 300;
const FIRST_BACKOFF_SECS: i64 = 600;
const MAX_BACKOFF_SECS: i64 = 3600;
/// After an error that is not a rate limit (network, 5xx).
const ERROR_RETRY_SECS: i64 = 60;

/// Which form of the endpoint to read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Endpoint {
    /// Windows and per-model limits.
    Usage,
    /// The same, plus the session-reset program (`juniper_tide`), as Claude
    /// Code reads it at a limit.
    AtWall,
    /// The same, plus the granted-reset program (`cedar_ember`).
    CedarEmber,
}

impl Endpoint {
    fn path(self) -> &'static str {
        match self {
            Self::Usage => "/api/oauth/usage",
            Self::AtWall => "/api/oauth/usage?at_wall=1&skip_spend=1",
            Self::CedarEmber => "/api/oauth/usage?cedar_ember=1&skip_spend=1",
        }
    }

    fn key(self, account: &str) -> String {
        match self {
            Self::Usage => format!("{account}|usage"),
            Self::AtWall => format!("{account}|at_wall"),
            Self::CedarEmber => format!("{account}|cedar_ember"),
        }
    }
}

/// One account's cached answer, and when a non-429 failure lets it be
/// asked again.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
struct Entry {
    #[serde(default)]
    fetched_at: i64,
    #[serde(default)]
    body: Option<serde_json::Value>,
    /// Epoch seconds before which this account is not asked again.
    #[serde(default)]
    retry_after: i64,
    #[serde(default)]
    last_error: Option<String>,
}

/// The fleet-wide 429 backoff.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
struct Backoff {
    /// Epoch seconds before which the accounts that drew a 429, and once
    /// two have, every account, leave the endpoint alone.
    #[serde(default)]
    retry_after: i64,
    /// Consecutive 429s, whichever account drew them.
    #[serde(default)]
    rate_limited: u32,
    #[serde(default)]
    last_error: Option<String>,
    /// The accounts that drew a 429 within this backoff.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    accounts: Vec<String>,
}

/// Distinct accounts whose 429s hold back the whole fleet. One account
/// alone can be refused for its own sake (a paused organization answered
/// 429 while another account read fine from the same address, 2026-11-11).
const FLEET_429_ACCOUNTS: usize = 2;

impl Backoff {
    /// Whether `account` must not ask before `retry_after`.
    fn holds(&self, account: &str, now: i64) -> bool {
        now < self.retry_after
            && (self.accounts.len() >= FLEET_429_ACCOUNTS
                || self.accounts.iter().any(|held| held == account))
    }

    fn rate_limited(&mut self, account: &str, now: i64, retry_after: Option<i64>, message: &str) {
        if now >= self.retry_after {
            self.accounts.clear();
        }
        if !self.accounts.iter().any(|held| held == account) {
            self.accounts.push(account.to_owned());
        }
        self.rate_limited = self.rate_limited.saturating_add(1);
        self.retry_after = self
            .retry_after
            .max(now + backoff_secs(true, self.rate_limited, retry_after));
        self.last_error = Some(message.to_owned());
    }

    /// A read succeeded. Reads run concurrently, so a success that was in
    /// flight when another account drew a 429 must not lift that backoff:
    /// only an expired one is cleared.
    fn succeeded(&mut self, now: i64) {
        if now >= self.retry_after {
            *self = Self::default();
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
struct CacheFile {
    #[serde(default)]
    backoff: Backoff,
    #[serde(default, deserialize_with = "crate::fs::lenient_map")]
    entries: BTreeMap<String, Entry>,
}

/// The cache file, shared by the daemon and every CLI command.
#[derive(Debug, Clone)]
pub struct Cache {
    path: PathBuf,
}

impl Default for Cache {
    fn default() -> Self {
        Self::at(paths::profile_cache())
    }
}

impl Cache {
    pub fn at(path: PathBuf) -> Self {
        Self { path }
    }

    /// The file as stored; a file in an older shape reads as empty.
    fn load(&self) -> CacheFile {
        std::fs::read(&self.path)
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default()
    }

    /// Best-effort read-modify-write: a lost update costs one extra read
    /// later. The file holds usage figures, never credentials.
    fn update(&self, change: impl FnOnce(&mut CacheFile)) {
        let mut file = self.load();
        change(&mut file);
        if let Ok(bytes) = serde_json::to_vec_pretty(&file) {
            let _ = write_atomic(&self.path, &bytes, 0o600);
        }
    }

    /// What the cache holds for `account`, however old, and when it was
    /// fetched (epoch seconds). No request is made.
    pub fn stored(&self, account: &str, endpoint: Endpoint) -> Option<(i64, serde_json::Value)> {
        let entry = self.load().entries.remove(&endpoint.key(account))?;
        Some((entry.fetched_at, entry.body?))
    }

    /// The endpoint's JSON for `account`: from the cache when younger than
    /// [`MAX_AGE_SECS`] (unless `fresh`), or while this account or the fleet
    /// is backed off; otherwise fetched with `bearer`. `now` is epoch seconds.
    pub async fn read(
        &self,
        client: &Client,
        account: &str,
        bearer: &str,
        endpoint: Endpoint,
        fresh: bool,
        now: i64,
    ) -> Result<serde_json::Value> {
        let key = endpoint.key(account);
        let file = self.load();
        let entry = file.entries.get(&key).cloned().unwrap_or_default();
        let young = now - entry.fetched_at < MAX_AGE_SECS;
        if let Some(body) = &entry.body
            && young
            && !fresh
        {
            return Ok(body.clone());
        }
        let fleet_held = file.backoff.holds(account, now);
        let scope = if file.backoff.accounts.len() >= FLEET_429_ACCOUNTS {
            " for every account"
        } else {
            ""
        };
        let held = [
            (entry.retry_after, entry.last_error.as_deref(), ""),
            (
                if fleet_held {
                    file.backoff.retry_after
                } else {
                    0
                },
                file.backoff.last_error.as_deref(),
                scope,
            ),
        ]
        .into_iter()
        .filter(|(until, _, _)| now < *until)
        .max_by_key(|(until, _, _)| *until);
        if let Some((until, error, scope)) = held {
            return match &entry.body {
                Some(body) if !fresh => Ok(body.clone()),
                _ => bail!(
                    "{} (backing off{scope} for {}s)",
                    error.unwrap_or("recently refused"),
                    until - now
                ),
            };
        }
        match fetch(client, bearer, endpoint).await {
            Ok(body) => {
                self.update(|file| {
                    file.entries.insert(
                        key,
                        Entry {
                            fetched_at: now,
                            body: Some(body.clone()),
                            ..Entry::default()
                        },
                    );
                    file.backoff.succeeded(now);
                });
                Ok(body)
            }
            Err(failure) => {
                self.update(|file| {
                    let entry = file.entries.entry(key).or_default();
                    entry.last_error = Some(failure.message.clone());
                    if failure.rate_limited {
                        file.backoff.rate_limited(
                            account,
                            now,
                            failure.retry_after,
                            &failure.message,
                        );
                    } else {
                        entry.retry_after = now + backoff_secs(false, 0, None);
                    }
                });
                match entry.body {
                    Some(body) if !fresh => Ok(body),
                    _ => bail!("{}", failure.message),
                }
            }
        }
    }
}

/// 10 minutes, doubling per consecutive 429 up to an hour, or the server's
/// `Retry-After` when that is longer; other errors retry after a minute.
fn backoff_secs(rate_limited: bool, consecutive: u32, retry_after: Option<i64>) -> i64 {
    if !rate_limited {
        return ERROR_RETRY_SECS;
    }
    let doubling = FIRST_BACKOFF_SECS << consecutive.saturating_sub(1).min(6);
    doubling.min(MAX_BACKOFF_SECS).max(retry_after.unwrap_or(0))
}

struct Failure {
    message: String,
    rate_limited: bool,
    retry_after: Option<i64>,
}

async fn fetch(
    client: &Client,
    bearer: &str,
    endpoint: Endpoint,
) -> std::result::Result<serde_json::Value, Failure> {
    let path = endpoint.path();
    let plain = |message: String| Failure {
        message,
        rate_limited: false,
        retry_after: None,
    };
    let response = client
        .oauth_get(path, bearer)
        .send()
        .await
        .map_err(|error| plain(format!("{path} request failed: {}", describe(&error))))?;
    let status = response.status();
    let retry_after = retry_after_secs(response.headers());
    let text = response.text().await.unwrap_or_default();
    if !status.is_success() {
        return Err(Failure {
            message: format!("{path} answered HTTP {status}: {}", one_line(&text, 160)),
            rate_limited: status.as_u16() == 429,
            retry_after,
        });
    }
    serde_json::from_str(&text)
        .map_err(|error| plain(format!("unreadable {path} response: {error}")))
}

// The response as Claude Code's own client reads it. `limits[]` is the
// server's list of meters and the primary source: classify a row on its
// `kind` and `scope`, never on a label. The legacy per-window fields are
// `{utilization: percent 0-100 | null, resets_at: ISO 8601 | null}` and are
// read only when `limits` is absent (older servers send null).

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UsageWindow {
    pub utilization: Option<f64>,
    pub resets_at: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct UsageResponse {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub five_hour: Option<UsageWindow>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seven_day: Option<UsageWindow>,
    /// Raw rows: each parses on its own, so one odd row cannot hide the rest,
    /// and `weekly_scoped_shares` indexes this raw list.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limits: Option<Vec<serde_json::Value>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub weekly_scoped_shares: Option<Vec<serde_json::Value>>,
}

/// What a `limits[]` row measures.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LimitKind {
    /// The 5-hour window.
    Session,
    /// The weekly limit across all models.
    WeeklyAll,
    /// A weekly limit scoped to one model or one surface.
    WeeklyScoped,
    #[serde(other)]
    Other,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UsageLimit {
    pub kind: LimitKind,
    /// 0-100, sent as an integer or a float; may exceed 100.
    pub percent: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resets_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<UsageScope>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct UsageScope {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<ScopeName>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub surface: Option<ScopeName>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ScopeName {
    pub display_name: String,
    /// The model id; null for Fable as of 2026-11.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
}

/// A scoped limit's size relative to the weekly limit.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ScopedShare {
    /// Index into the raw `limits[]`, unreadable rows included.
    pub limit_index: usize,
    pub allowance_percent_of_weekly: f64,
    #[serde(default)]
    pub used_percent_of_weekly: Option<f64>,
}

/// Everything the profile endpoint says about an account's room.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ProfileUsage {
    pub session: Option<Window>,
    pub weekly: Option<Window>,
    pub session_limit: SessionLimit,
    /// The per-model weekly limits.
    pub models: Vec<Limit>,
}

fn parse_reset(resets_at: Option<&str>) -> i64 {
    // An unused bucket may have no reset yet; 0 renders as "--".
    resets_at
        .and_then(|value| chrono::DateTime::parse_from_rfc3339(value).ok())
        .map_or(0, |time| time.timestamp())
}

/// Percent to fraction, once, at the boundary. Over 100 stays over 1.0.
fn fraction(percent: f64) -> f64 {
    percent.max(0.0) / 100.0
}

fn usage_window(window: &UsageWindow) -> Option<Window> {
    Some(Window::new(
        fraction(window.utilization?),
        parse_reset(window.resets_at.as_deref()),
    ))
}

fn limit_window(row: &UsageLimit) -> Window {
    Window::new(fraction(row.percent), parse_reset(row.resets_at.as_deref()))
}

/// A per-model weekly row: a `weekly_scoped` meter whose scope names a
/// model, keyed by its id when the server sends one. Surface-scoped rows are
/// not models. Idle rows count too; `is_active` only marks the server's
/// headline row.
fn scoped_model_limit(row: &UsageLimit, share: Option<f64>) -> Option<Limit> {
    if row.kind != LimitKind::WeeklyScoped {
        return None;
    }
    let model = row.scope.as_ref()?.model.as_ref()?;
    let key = model
        .id
        .as_deref()
        .map(ModelKey::new)
        .filter(|key| !key.is_empty())
        .unwrap_or_else(|| ModelKey::new(&model.display_name));
    (!key.is_empty()).then(|| Limit {
        share,
        ..Limit::model(key, &model.display_name, limit_window(row), Source::Profile)
    })
}

/// Allowances by raw `limits[]` index, as fractions of the weekly limit.
fn shares(usage: &UsageResponse) -> BTreeMap<usize, f64> {
    usage
        .weekly_scoped_shares
        .iter()
        .flatten()
        .filter_map(|row| serde_json::from_value::<ScopedShare>(row.clone()).ok())
        .filter(|share| share.allowance_percent_of_weekly > 0.0)
        .map(|share| (share.limit_index, share.allowance_percent_of_weekly / 100.0))
        .collect()
}

/// Read the whole response: shared windows and the 5h state from
/// `limits[]` (or the legacy fields when it is absent), and the scoped model
/// rows sorted by label and unique by key.
pub fn usage_of(usage: &UsageResponse) -> ProfileUsage {
    let shares = shares(usage);
    let rows: Vec<(usize, UsageLimit)> = usage
        .limits
        .iter()
        .flatten()
        .enumerate()
        .filter_map(|(index, row)| Some((index, serde_json::from_value(row.clone()).ok()?)))
        .collect();
    let first = |kind: LimitKind| {
        rows.iter()
            .find(|(_, row)| row.kind == kind)
            .map(|(_, row)| limit_window(row))
    };
    let mut profile = ProfileUsage::default();
    match &usage.limits {
        Some(raw) => {
            profile.session = first(LimitKind::Session);
            profile.weekly = first(LimitKind::WeeklyAll);
            // A row that names the session kind counts even if it is malformed.
            let has_session = raw
                .iter()
                .any(|row| row.get("kind").and_then(|kind| kind.as_str()) == Some("session"));
            profile.session_limit = if has_session {
                SessionLimit::Present
            } else {
                SessionLimit::Absent
            };
        }
        None => {
            profile.session = usage.five_hour.as_ref().and_then(usage_window);
            profile.weekly = usage.seven_day.as_ref().and_then(usage_window);
            if profile.session.is_some() {
                profile.session_limit = SessionLimit::Present;
            }
        }
    }
    let mut models: Vec<Limit> = rows
        .iter()
        .filter_map(|(index, row)| scoped_model_limit(row, shares.get(index).copied()))
        .collect();
    models.sort_by(|a, b| a.key().cmp(&b.key()));
    models.dedup_by(|a, b| a.key() == b.key());
    models.sort_by(|a, b| a.label.cmp(&b.label));
    profile.models = models;
    profile
}

pub fn parse(body: &serde_json::Value) -> std::result::Result<ProfileUsage, serde_json::Error> {
    serde_json::from_value::<UsageResponse>(body.clone()).map(|usage| usage_of(&usage))
}

/// The account's profile usage, through the cache. `None` when the account
/// has no profile credential; otherwise the usage or why there is none.
pub async fn usage(
    client: &Client,
    cache: &Cache,
    account: &Account,
    now: i64,
) -> Option<std::result::Result<ProfileUsage, String>> {
    let bearer = account.profile_token(now * 1000)?;
    let outcome = match cache
        .read(client, &account.name, bearer, Endpoint::Usage, false, now)
        .await
    {
        Ok(body) => parse(&body).map_err(|error| format!("usage response parse failed: {error}")),
        Err(error) => Err(format!("usage {error:#}")),
    };
    Some(outcome)
}

/// Fold the profile's answer into a reading: its model limits replace any
/// with the same key; its shared windows fill only what the headers did not
/// report (the headers are fresher); its 5h state stands unless the headers
/// saw a 5h window. A failure is recorded.
pub fn merge(reading: &mut Reading, outcome: Option<std::result::Result<ProfileUsage, String>>) {
    match outcome {
        Some(Ok(profile)) => {
            for limit in profile.models {
                reading.set_limit(limit);
            }
            // A cached answer can outlive its window; one past its reset says nothing.
            let at = reading.at;
            for (span, window) in [
                (Span::FiveHour, profile.session),
                (Span::Weekly, profile.weekly),
            ] {
                if let Some(window) = window.filter(|window| window.resets_at > at)
                    && reading.window(span, &Scope::All).is_none()
                {
                    reading.set_window(span, Scope::All, window, Source::Profile);
                }
            }
            let has_session = reading.session().is_some();
            let claude = reading.claude_mut();
            if claude.session_limit != SessionLimit::Present
                && profile.session_limit != SessionLimit::Unknown
            {
                claude.session_limit = profile.session_limit;
            }
            if has_session {
                claude.session_limit = SessionLimit::Present;
            }
        }
        Some(Err(error)) => reading.claude_mut().profile_error = Some(error),
        None => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::anthropic::testing::{serve, temp_cache};
    use crate::quota::{Family, Service};
    use crate::testing::{model_limit, profile_only_account, setup_account};
    use axum::Router;
    use axum::http::StatusCode;
    use axum::routing::get;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU32, Ordering};

    #[test]
    fn rate_limits_back_off_exponentially_and_honor_retry_after() {
        assert_eq!(backoff_secs(true, 1, None), 600);
        assert_eq!(backoff_secs(true, 2, None), 1200);
        assert_eq!(backoff_secs(true, 9, None), 3600);
        assert_eq!(backoff_secs(true, 1, Some(7200)), 7200);
        assert_eq!(backoff_secs(false, 5, Some(7200)), 60);
    }

    #[test]
    fn profile_usage_windows_normalize_percent_and_stay_unknown_when_incomplete() {
        let window = usage_window(&UsageWindow {
            utilization: Some(67.5),
            resets_at: Some("2026-07-31T12:34:56Z".into()),
        })
        .expect("valid profile usage window");
        assert!((window.used - 0.675).abs() < f64::EPSILON);
        assert_eq!(window.resets_at, 1_785_501_296);
        assert!(
            usage_window(&UsageWindow {
                utilization: None,
                resets_at: None,
            })
            .is_none()
        );
    }

    #[test]
    fn the_legacy_per_family_fields_are_not_read() {
        let usage: UsageResponse = serde_json::from_str(
            r#"{
                "five_hour": {"utilization": 10.0, "resets_at": "2026-07-26T12:00:00Z"},
                "seven_day": {"utilization": 20.0, "resets_at": "2026-08-01T12:00:00Z"},
                "seven_day_opus": {
                    "utilization": 81.0,
                    "resets_at": "2026-07-30T12:00:00Z"
                },
                "seven_day_sonnet": null
            }"#,
        )
        .expect("usage response");
        let profile = usage_of(&usage);
        assert!(profile.models.is_empty());
        assert!((profile.weekly.unwrap().used - 0.2).abs() < f64::EPSILON);
    }

    #[test]
    fn dynamic_scoped_weekly_buckets_preserve_model_identity() {
        let usage: UsageResponse = serde_json::from_str(
            r#"{
                "limits": [
                    {
                        "kind": "weekly_all",
                        "group": "weekly",
                        "percent": 25.0,
                        "resets_at": "2026-08-01T12:00:00Z"
                    },
                    {
                        "kind": "weekly_scoped",
                        "group": "weekly",
                        "percent": 100.0,
                        "resets_at": "2026-08-02T12:00:00Z",
                        "is_active": true,
                        "scope": {
                            "model": {
                                "id": "claude-opus-4-8",
                                "display_name": "Opus 4.8"
                            }
                        }
                    },
                    {
                        "kind": "weekly_scoped",
                        "group": "weekly",
                        "percent": 20.0,
                        "resets_at": "2026-08-03T12:00:00Z",
                        "is_active": true,
                        "scope": {
                            "model": {
                                "id": "claude-opus-5",
                                "display_name": "Opus 5"
                            }
                        }
                    },
                    {
                        "kind": "weekly_scoped",
                        "group": "weekly",
                        "percent": 0.0,
                        "resets_at": "2026-08-03T12:00:00Z",
                        "is_active": false,
                        "scope": {"surface": {"display_name": "Cowork"}}
                    }
                ]
            }"#,
        )
        .expect("dynamic usage response");
        let models = usage_of(&usage).models;
        // The surface-scoped row is a meter, but not a model.
        assert_eq!(models.len(), 2);
        assert_eq!(models[0].key().unwrap().as_str(), "opus48");
        assert_eq!(models[0].label, "Opus 4.8");
        assert_eq!(models[0].window.used, 1.0);
        assert_eq!(models[0].span, Span::Weekly);
        assert_eq!(models[0].source, Source::Profile);
        assert_eq!(models[1].key().unwrap().as_str(), "opus5");
        assert_eq!(models[1].window.used, 0.2);
    }

    #[test]
    fn idle_null_reset_and_odd_rows_do_not_hide_model_buckets() {
        let body: serde_json::Value = serde_json::from_str(
            r#"{
                "limits": [
                    {"kind": "weekly_scoped", "group": "weekly", "percent": 0.0,
                     "resets_at": null, "severity": "normal", "is_active": false,
                     "scope": {"model": {"display_name": "Fable"}}},
                    {"kind": "weekly_scoped", "percent": "not a number",
                     "scope": {"model": {"display_name": "Broken"}}},
                    {"kind": "some_future_kind", "percent": 5.0, "surprise": [1, 2]}
                ]
            }"#,
        )
        .unwrap();
        let models = parse(&body)
            .expect("a malformed row must not fail the whole response")
            .models;
        assert_eq!(models.len(), 1);
        assert_eq!(models[0].key().unwrap().as_str(), "fable");
        assert_eq!(models[0].window.resets_at, 0);

        let older = parse(&serde_json::json!({"limits": null})).unwrap();
        assert!(older.models.is_empty());
        assert_eq!(older.session_limit, SessionLimit::Unknown);
    }

    /// The shape of a verified Ultra account's answer (2026-11): no session
    /// row, the weekly limit, and Fable as a half-weekly scoped limit whose
    /// share indexes the raw rows, an unreadable one included.
    fn ultra_body() -> serde_json::Value {
        serde_json::json!({
            "five_hour": null,
            "seven_day": {"utilization": 80.0, "resets_at": "2026-11-13T09:00:00Z"},
            "seven_day_opus": null,
            "limits": [
                {"kind": "weekly_all", "group": "weekly", "percent": 80,
                 "severity": "warning", "resets_at": "2026-11-13T09:00:00Z",
                 "scope": null, "is_active": false},
                {"kind": "weekly_scoped", "percent": "garbled"},
                {"kind": "weekly_scoped", "group": "weekly", "percent": 102,
                 "severity": "critical", "resets_at": "2026-11-13T09:00:00Z",
                 "scope": {"model": {"id": null, "display_name": "Fable"}, "surface": null},
                 "is_active": true},
                {"kind": "weekly_scoped", "group": "weekly", "percent": 4,
                 "resets_at": null, "scope": {"surface": {"display_name": "Cowork"}}}
            ],
            "weekly_scoped_shares": [
                {"limit_index": 2, "used_percent_of_weekly": 51, "allowance_percent_of_weekly": 50},
                {"limit_index": "bad"}
            ]
        })
    }

    #[test]
    fn a_body_without_a_session_row_has_no_5h_limit_and_fable_is_a_share_of_the_weekly() {
        let usage = parse(&ultra_body()).unwrap();
        assert_eq!(usage.session_limit, SessionLimit::Absent);
        assert_eq!(usage.session, None);
        assert_eq!(usage.weekly, Some(Window::new(0.8, 1_794_560_400)));
        assert_eq!(usage.models.len(), 1, "the surface row is not a model");
        let fable = &usage.models[0];
        assert_eq!(fable.key(), Some(&ModelKey::family_wide(Family::Fable)));
        assert_eq!(fable.window.used, 1.02, "over 100% is kept");
        assert_eq!(fable.share, Some(0.5), "limit_index counts the garbled row");
        // Room for Fable: none of its own, so none at all.
        assert!(fable.room(usage.weekly.as_ref()) < 0.0);
    }

    #[test]
    fn a_session_row_marks_the_5h_limit_present_and_a_model_id_keys_the_bucket() {
        let usage = parse(&serde_json::json!({
            "limits": [
                {"kind": "session", "percent": 12.5, "resets_at": "2026-11-11T22:00:00Z"},
                {"kind": "weekly_all", "percent": 30, "resets_at": "2026-11-13T09:00:00Z"},
                {"kind": "weekly_scoped", "percent": 10,
                 "scope": {"model": {"id": "claude-mythos-5", "display_name": "Mythos"}}}
            ]
        }))
        .unwrap();
        assert_eq!(usage.session_limit, SessionLimit::Present);
        assert_eq!(usage.session, Some(Window::new(0.125, 1_794_434_400)));
        let mythos = usage.models[0].key().unwrap();
        assert_eq!(mythos.as_str(), "mythos5");
        assert_eq!(mythos.family(), Some(Family::Mythos));
        assert_eq!(usage.models[0].share, None);
        // Without limits[], the legacy fields fill the windows, and a missing
        // five_hour proves nothing.
        let legacy = parse(&serde_json::json!({
            "five_hour": null,
            "seven_day": {"utilization": 40.0, "resets_at": null}
        }))
        .unwrap();
        assert_eq!(legacy.session_limit, SessionLimit::Unknown);
        assert_eq!(legacy.weekly, Some(Window::new(0.4, 0)));
    }

    #[test]
    fn merging_fills_only_what_the_headers_left_out_and_keeps_a_seen_5h_window() {
        let at = 1_794_400_000;
        let profile = || parse(&ultra_body()).ok();
        // Headers without a 5h window: the profile says there is none.
        let mut weekly_only = crate::testing::reading("a", 0.0, 0.79);
        weekly_only.at = at;
        weekly_only.set_session(None);
        weekly_only.claude_mut().session_limit = SessionLimit::Unknown;
        merge(&mut weekly_only, profile().map(Ok));
        assert_eq!(weekly_only.session_limit(), SessionLimit::Absent);
        let weekly = weekly_only.limit(Span::Weekly, &Scope::All).unwrap();
        assert_eq!(weekly.window.used, 0.79, "headers win");
        assert_eq!(weekly.source, Source::Headers);

        // Headers that saw a 5h window outrank the profile's Absent.
        let mut both = crate::testing::reading("b", 0.2, 0.5);
        both.at = at;
        merge(&mut both, profile().map(Ok));
        assert_eq!(both.session_limit(), SessionLimit::Present);

        // Nothing from the headers: the profile's weekly fills in, unless the
        // cached answer is past its reset.
        let mut bare = Reading::empty(Service::Claude, "c", at);
        merge(&mut bare, profile().map(Ok));
        let weekly = bare.limit(Span::Weekly, &Scope::All).unwrap();
        assert_eq!(weekly.window.used, 0.8);
        assert_eq!(weekly.source, Source::Profile);
        let mut later = Reading::empty(Service::Claude, "c", 1_794_560_400);
        merge(&mut later, profile().map(Ok));
        assert_eq!(later.weekly(), None);
    }

    #[test]
    fn scoped_rows_with_the_same_key_collapse_to_the_first() {
        let body = serde_json::json!({
            "limits": [
                {"kind": "weekly_scoped", "percent": 75.0,
                 "scope": {"model": {"display_name": "Opus"}}},
                {"kind": "weekly_scoped", "percent": 10.0,
                 "scope": {"model": {"display_name": "Opus"}}}
            ]
        });
        let models = parse(&body).unwrap().models;
        assert_eq!(models.len(), 1, "duplicate keys collapse to one");
        assert_eq!(models[0].window.used, 0.75);
    }

    #[test]
    fn response_shapes_round_trip() {
        let response = UsageResponse {
            seven_day: Some(UsageWindow {
                utilization: Some(12.5),
                resets_at: Some("2026-08-01T00:00:00+00:00".into()),
            }),
            five_hour: Some(UsageWindow {
                utilization: None,
                resets_at: None,
            }),
            limits: Some(vec![
                serde_json::to_value(UsageLimit {
                    kind: LimitKind::WeeklyScoped,
                    percent: 3.0,
                    resets_at: None,
                    scope: Some(UsageScope {
                        model: Some(ScopeName {
                            display_name: "Fable".into(),
                            id: None,
                        }),
                        surface: None,
                    }),
                })
                .unwrap(),
            ]),
            ..UsageResponse::default()
        };
        let json = serde_json::to_string(&response).unwrap();
        let back: UsageResponse = serde_json::from_str(&json).unwrap();
        assert_eq!(back, response);
        let limit: UsageLimit = serde_json::from_value(back.limits.unwrap().remove(0)).unwrap();
        assert_eq!(limit.scope.unwrap().model.unwrap().display_name, "Fable");
        let models = usage_of(&response).models;
        assert_eq!(models.len(), 1);
        assert_eq!(models[0].key().unwrap().as_str(), "fable");
    }

    #[test]
    fn cache_files_round_trip_and_an_older_shape_reads_as_empty() {
        let mut file = CacheFile::default();
        file.backoff.rate_limited = 2;
        file.entries.insert(
            "a|usage".into(),
            Entry {
                fetched_at: 5,
                body: Some(serde_json::json!({"limits": []})),
                retry_after: 9,
                last_error: Some("x".into()),
            },
        );
        let json = serde_json::to_string(&file).unwrap();
        assert_eq!(serde_json::from_str::<CacheFile>(&json).unwrap(), file);
        // 0.3.x kept a bare map of entries; it starts fresh.
        let older: CacheFile =
            serde_json::from_str(r#"{"a|usage": {"fetched_at": 5, "rate_limited": 1}}"#).unwrap();
        assert_eq!(older, CacheFile::default());
    }

    #[test]
    fn merging_replaces_same_keyed_buckets_or_records_the_failure() {
        let mut reading = Reading::failed(
            Service::Claude,
            "a",
            0,
            crate::quota::ReadError::NoCredential,
        );
        reading
            .limits
            .push(model_limit("Opus 5", "Opus 5", 0.1, Source::Carried));
        merge(
            &mut reading,
            Some(Ok(ProfileUsage {
                models: vec![model_limit("Opus 5", "Opus 5", 0.4, Source::Profile)],
                ..ProfileUsage::default()
            })),
        );
        assert_eq!(reading.limits.len(), 1);
        assert_eq!(reading.limits[0].source, Source::Profile);
        merge(&mut reading, Some(Err("usage nope".into())));
        assert_eq!(
            reading.claude().unwrap().profile_error.as_deref(),
            Some("usage nope")
        );
        merge(&mut reading, None);
        assert_eq!(reading.limits.len(), 1);
    }

    /// A usage endpoint that answers from a script of statuses, one per call.
    fn scripted(
        script: Vec<(StatusCode, Option<u32>, serde_json::Value)>,
    ) -> (Router, Arc<AtomicU32>) {
        let calls = Arc::new(AtomicU32::new(0));
        let counter = calls.clone();
        let script = Arc::new(script);
        let router = Router::new().route(
            "/api/oauth/usage",
            get(move |headers: axum::http::HeaderMap| {
                let script = script.clone();
                let counter = counter.clone();
                async move {
                    assert_eq!(
                        headers.get("anthropic-beta").and_then(|v| v.to_str().ok()),
                        Some(super::super::OAUTH_BETA)
                    );
                    assert!(
                        headers
                            .get("user-agent")
                            .and_then(|v| v.to_str().ok())
                            .is_some_and(|ua| ua.starts_with("claude-cli/"))
                    );
                    let index = counter.fetch_add(1, Ordering::SeqCst) as usize;
                    let (status, retry_after, body) = script.get(index).cloned().unwrap_or((
                        StatusCode::IM_A_TEAPOT,
                        None,
                        serde_json::Value::Null,
                    ));
                    let mut response = axum::response::Response::new(body.to_string());
                    *response.status_mut() = status;
                    if let Some(secs) = retry_after {
                        response
                            .headers_mut()
                            .insert("retry-after", secs.to_string().parse().unwrap());
                    }
                    response
                }
            }),
        );
        (router, calls)
    }

    #[tokio::test]
    async fn a_young_answer_is_reused_and_a_second_accounts_429_holds_back_every_account() {
        let good = serde_json::json!({"seven_day_opus": {"utilization": 30.0, "resets_at": null}});
        let refused = (
            StatusCode::TOO_MANY_REQUESTS,
            Some(7200),
            serde_json::json!({}),
        );
        let (router, calls) = scripted(vec![
            (StatusCode::OK, None, good.clone()),
            refused.clone(),
            (StatusCode::OK, None, good.clone()),
            refused,
            (StatusCode::OK, None, good.clone()),
        ]);
        let client = serve(router).await;
        let (_dir, cache) = temp_cache();
        let read = |account, now, fresh| {
            let cache = cache.clone();
            let client = client.clone();
            async move {
                cache
                    .read(&client, account, "tok", Endpoint::Usage, fresh, now)
                    .await
            }
        };

        assert_eq!(read("acct", 1000, false).await.unwrap(), good);
        assert_eq!(
            read("acct", 1000 + MAX_AGE_SECS - 1, false).await.unwrap(),
            good
        );
        assert_eq!(calls.load(Ordering::SeqCst), 1, "a young answer is reused");

        // Stale: asked again, refused, last good answer served.
        let refused_at = 1000 + MAX_AGE_SECS;
        assert_eq!(read("acct", refused_at, false).await.unwrap(), good);
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        let file = cache.load();
        assert_eq!(file.backoff.rate_limited, 1);
        assert_eq!(
            file.backoff.retry_after,
            refused_at + 7200,
            "Retry-After wins"
        );
        assert_eq!(file.backoff.accounts, ["acct"]);

        // That account is held, and a fresh read of it refuses...
        assert_eq!(read("acct", refused_at + 10, false).await.unwrap(), good);
        let fresh = read("acct", refused_at + 10, true).await.unwrap_err();
        assert!(fresh.to_string().contains("backing off"), "{fresh}");
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        // ...but one account's 429 does not hold back another, whose success
        // leaves the live backoff alone.
        assert_eq!(read("second", refused_at + 10, false).await.unwrap(), good);
        assert_eq!(calls.load(Ordering::SeqCst), 3);
        assert_eq!(cache.load().backoff.accounts, ["acct"]);

        // A second account's 429 holds back every account, even a new one.
        assert!(read("third", refused_at + 400, false).await.is_err());
        assert_eq!(calls.load(Ordering::SeqCst), 4);
        let other = read("fourth", refused_at + 410, false).await.unwrap_err();
        assert!(other.to_string().contains("for every account"), "{other}");
        assert_eq!(calls.load(Ordering::SeqCst), 4);

        // After it, a success clears the backoff.
        let after = cache.load().backoff.retry_after;
        assert_eq!(read("fourth", after, false).await.unwrap(), good);
        assert_eq!(calls.load(Ordering::SeqCst), 5);
        assert_eq!(cache.load().backoff, Backoff::default());
    }

    #[test]
    fn a_success_in_flight_during_another_accounts_429_keeps_the_backoff() {
        let mut backoff = Backoff {
            retry_after: 1600,
            rate_limited: 2,
            last_error: Some("HTTP 429".into()),
            accounts: vec!["a".into(), "b".into()],
        };
        let held = backoff.clone();
        backoff.succeeded(1000);
        assert_eq!(backoff, held);
        backoff.succeeded(1600);
        assert_eq!(backoff, Backoff::default());
    }

    #[tokio::test]
    async fn other_failures_hold_back_only_their_account_and_the_file_is_private() {
        let (router, calls) = scripted(vec![
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                None,
                serde_json::json!("boom"),
            ),
            (StatusCode::OK, None, serde_json::json!({"limits": []})),
            (StatusCode::OK, None, serde_json::json!({"limits": []})),
        ]);
        let client = serve(router).await;
        let (dir, cache) = temp_cache();
        let read = |account, now| {
            let cache = cache.clone();
            let client = client.clone();
            async move {
                cache
                    .read(&client, account, "tok", Endpoint::Usage, false, now)
                    .await
            }
        };
        let error = read("acct", 100).await.unwrap_err();
        assert!(error.to_string().contains("HTTP 500"), "{error}");
        let file = cache.load();
        assert_eq!(
            file.entries["acct|usage"].retry_after,
            100 + ERROR_RETRY_SECS
        );
        assert_eq!(file.backoff, Backoff::default());
        // Still inside the minute: no request for it, but another account asks.
        assert!(read("acct", 150).await.is_err());
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        read("other", 150).await.unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        // After it: fetched, and the entry's backoff state is cleared.
        read("acct", 161).await.unwrap();
        let entry = cache.load().entries.remove("acct|usage").unwrap();
        assert_eq!(entry.retry_after, 0);
        assert!(entry.last_error.is_none());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(dir.path().join("reads.json"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(mode, 0o600);
        }
    }

    #[tokio::test]
    async fn usage_needs_a_profile_token_and_reports_parse_failures() {
        let (router, _) = scripted(vec![(StatusCode::OK, None, serde_json::json!([1, 2]))]);
        let client = serve(router).await;
        let (_dir, cache) = temp_cache();
        let setup_only = setup_account("acct");
        assert!(usage(&client, &cache, &setup_only, 10).await.is_none());
        let account = profile_only_account("acct", "captured");
        let outcome = usage(&client, &cache, &account, 10).await.unwrap();
        assert!(
            outcome
                .unwrap_err()
                .starts_with("usage response parse failed")
        );
    }
}
