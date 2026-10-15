//! `/api/oauth/usage`: per-model weekly limits and reset offers, read the way
//! Claude Code reads them, behind an on-disk cache.
//!
//! The endpoint answers 429 under a 20-second probe cadence and the figures
//! move over hours, so each account's answer is reused for [`MAX_AGE_SECS`]
//! and a 429 backs the account off; while it lasts the last good answer is
//! served. This module is the only writer of the cache file.

use std::collections::BTreeMap;
use std::path::PathBuf;

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};

use super::{Client, describe, one_line, retry_after_secs};
use crate::account::Account;
use crate::fs::write_atomic;
use crate::paths;
use crate::quota::{Bucket, Family, ModelKey, Reading, Source, Window};

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
    /// The same, plus the reset programs, as Claude Code reads it at a limit.
    AtWall,
}

impl Endpoint {
    fn path(self) -> &'static str {
        match self {
            Self::Usage => "/api/oauth/usage",
            Self::AtWall => "/api/oauth/usage?at_wall=1&skip_spend=1",
        }
    }

    fn key(self, account: &str) -> String {
        match self {
            Self::Usage => format!("{account}|usage"),
            Self::AtWall => format!("{account}|at_wall"),
        }
    }
}

/// One cached answer and the state of its backoff.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
struct Entry {
    #[serde(default)]
    fetched_at: i64,
    #[serde(default)]
    body: Option<serde_json::Value>,
    /// Epoch seconds before which the endpoint is not asked again.
    #[serde(default)]
    retry_after: i64,
    /// Consecutive 429s.
    #[serde(default)]
    rate_limited: u32,
    #[serde(default)]
    last_error: Option<String>,
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

    fn load(&self) -> BTreeMap<String, Entry> {
        std::fs::read(&self.path)
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default()
    }

    /// Best-effort: a lost update costs one extra read later. The file holds
    /// usage figures, never credentials.
    fn store(&self, key: &str, entry: &Entry) {
        let mut cache = self.load();
        cache.insert(key.to_owned(), entry.clone());
        if let Ok(bytes) = serde_json::to_vec_pretty(&cache) {
            let _ = write_atomic(&self.path, &bytes, 0o600);
        }
    }

    /// The endpoint's JSON for `account`: from the cache when younger than
    /// [`MAX_AGE_SECS`] (unless `fresh`) or while the account is backed off,
    /// otherwise fetched with `bearer`. `now` is epoch seconds.
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
        let mut entry = self.load().remove(&key).unwrap_or_default();
        let young = now - entry.fetched_at < MAX_AGE_SECS;
        if let Some(body) = &entry.body
            && young
            && !fresh
        {
            return Ok(body.clone());
        }
        if now < entry.retry_after {
            return match &entry.body {
                Some(body) if !fresh => Ok(body.clone()),
                _ => bail!(
                    "{} (backing off for {}s)",
                    entry.last_error.as_deref().unwrap_or("recently refused"),
                    entry.retry_after - now
                ),
            };
        }
        match fetch(client, bearer, endpoint).await {
            Ok(body) => {
                entry = Entry {
                    fetched_at: now,
                    body: Some(body.clone()),
                    ..Entry::default()
                };
                self.store(&key, &entry);
                Ok(body)
            }
            Err(failure) => {
                if failure.rate_limited {
                    entry.rate_limited = entry.rate_limited.saturating_add(1);
                }
                entry.retry_after = now
                    + backoff_secs(
                        failure.rate_limited,
                        entry.rate_limited,
                        failure.retry_after,
                    );
                entry.last_error = Some(failure.message.clone());
                self.store(&key, &entry);
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

// The response as Claude Code's own client reads it. Every window is
// `{utilization: percent 0-100 | null, resets_at: ISO 8601 | null}`;
// `limits` is the server's list of meters and is null from older servers.

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UsageWindow {
    pub utilization: Option<f64>,
    pub resets_at: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct UsageResponse {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seven_day_opus: Option<UsageWindow>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seven_day_sonnet: Option<UsageWindow>,
    /// Raw rows: each parses on its own, so one odd row cannot hide the rest.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limits: Option<Vec<serde_json::Value>>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UsageLimit {
    /// `session`, `weekly_all`, `weekly_scoped`, ...
    pub kind: String,
    /// 0-100.
    pub percent: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resets_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<UsageScope>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct UsageScope {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<DisplayName>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DisplayName {
    pub display_name: String,
}

fn parse_reset(resets_at: Option<&str>) -> i64 {
    // An unused bucket may have no reset yet; 0 renders as "--".
    resets_at
        .and_then(|value| chrono::DateTime::parse_from_rfc3339(value).ok())
        .map_or(0, |time| time.timestamp())
}

/// Percent to fraction, once, at the boundary.
fn fraction(percent: f64) -> f64 {
    (percent / 100.0).clamp(0.0, 1.0)
}

fn usage_window(window: &UsageWindow) -> Option<Window> {
    Some(Window::new(
        fraction(window.utilization?),
        parse_reset(window.resets_at.as_deref()),
    ))
}

/// A per-model weekly row as Claude Code identifies it: a `weekly_scoped`
/// meter whose scope names a model. Surface-scoped rows are not models.
/// Idle rows count too; `is_active` only marks the server's headline row.
fn scoped_model_bucket(row: &serde_json::Value) -> Option<Bucket> {
    let row: UsageLimit = serde_json::from_value(row.clone()).ok()?;
    if row.kind != "weekly_scoped" {
        return None;
    }
    let label = row.scope?.model?.display_name;
    let key = ModelKey::new(&label);
    (!key.is_empty()).then(|| Bucket {
        key,
        label,
        window: Window::new(fraction(row.percent), parse_reset(row.resets_at.as_deref())),
        source: Source::Profile,
    })
}

/// Every model bucket in the response: the scoped rows, sorted by label and
/// unique by key, then the legacy family-wide Opus and Sonnet windows when no
/// scoped row already carries their key.
pub fn buckets(usage: &UsageResponse) -> Vec<Bucket> {
    let mut buckets: Vec<Bucket> = usage
        .limits
        .iter()
        .flatten()
        .filter_map(scoped_model_bucket)
        .collect();
    buckets.sort_by(|a, b| a.key.cmp(&b.key));
    buckets.dedup_by(|a, b| a.key == b.key);
    buckets.sort_by(|a, b| a.label.cmp(&b.label));
    for (family, window) in [
        (Family::Opus, usage.seven_day_opus.as_ref()),
        (Family::Sonnet, usage.seven_day_sonnet.as_ref()),
    ] {
        let key = ModelKey::family_wide(family);
        if let Some(window) = window.and_then(usage_window)
            && !buckets.iter().any(|bucket| bucket.key == key)
        {
            buckets.push(Bucket {
                key,
                label: family.label().to_owned(),
                window,
                source: Source::Profile,
            });
        }
    }
    buckets
}

pub fn parse(body: &serde_json::Value) -> std::result::Result<Vec<Bucket>, serde_json::Error> {
    serde_json::from_value::<UsageResponse>(body.clone()).map(|usage| buckets(&usage))
}

/// The account's profile buckets, through the cache. `None` when the account
/// has no profile credential; otherwise the buckets or why there are none.
pub async fn usage(
    client: &Client,
    cache: &Cache,
    account: &Account,
    now: i64,
) -> Option<std::result::Result<Vec<Bucket>, String>> {
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

/// Fold the profile's answer into a reading: its buckets replace any with the
/// same key, or its failure is recorded.
pub fn merge(reading: &mut Reading, outcome: Option<std::result::Result<Vec<Bucket>, String>>) {
    match outcome {
        Some(Ok(buckets)) => {
            for bucket in buckets {
                reading.set_bucket(bucket);
            }
        }
        Some(Err(error)) => reading.profile_error = Some(error),
        None => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::anthropic::testing::{serve, temp_cache};
    use crate::testing::{bucket, profile_only_account, setup_account};
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
    fn profile_usage_buckets_are_independent_and_optional() {
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
        assert!(usage.seven_day_sonnet.is_none());
        let buckets = buckets(&usage);
        assert_eq!(buckets.len(), 1);
        assert_eq!(buckets[0].key, ModelKey::family_wide(Family::Opus));
        assert_eq!(buckets[0].label, "Opus");
        assert_eq!(buckets[0].source, Source::Profile);
        assert!((buckets[0].window.used - 0.81).abs() < f64::EPSILON);
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
        let buckets = buckets(&usage);
        // The surface-scoped row is a meter, but not a model.
        assert_eq!(buckets.len(), 2);
        assert_eq!(buckets[0].key.as_str(), "opus48");
        assert_eq!(buckets[0].label, "Opus 4.8");
        assert_eq!(buckets[0].window.used, 1.0);
        assert_eq!(buckets[1].key.as_str(), "opus5");
        assert_eq!(buckets[1].window.used, 0.2);
    }

    #[test]
    fn idle_null_reset_and_odd_rows_do_not_hide_model_buckets() {
        let body: serde_json::Value = serde_json::from_str(
            r#"{
                "seven_day_opus": {"utilization": 0.0, "resets_at": null},
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
        let buckets = parse(&body).expect("a malformed row must not fail the whole response");
        assert_eq!(buckets.len(), 2);
        assert_eq!(buckets[0].key.as_str(), "fable");
        assert_eq!(buckets[0].window.resets_at, 0);
        assert_eq!(buckets[1].key.as_str(), "opus");
        assert_eq!(buckets[1].window.resets_at, 0);

        let older = parse(&serde_json::json!({"limits": null})).unwrap();
        assert!(older.is_empty());
    }

    #[test]
    fn a_scoped_family_row_wins_over_the_legacy_field() {
        let body = serde_json::json!({
            "seven_day_opus": {"utilization": 50.0, "resets_at": null},
            "limits": [
                {"kind": "weekly_scoped", "percent": 75.0,
                 "scope": {"model": {"display_name": "Opus"}}},
                {"kind": "weekly_scoped", "percent": 10.0,
                 "scope": {"model": {"display_name": "Opus"}}}
            ]
        });
        let buckets = parse(&body).unwrap();
        assert_eq!(buckets.len(), 1, "duplicate keys collapse to one");
        assert_eq!(buckets[0].window.used, 0.75);
    }

    #[test]
    fn response_shapes_round_trip() {
        let response = UsageResponse {
            seven_day_opus: Some(UsageWindow {
                utilization: Some(12.5),
                resets_at: Some("2026-08-01T00:00:00+00:00".into()),
            }),
            seven_day_sonnet: Some(UsageWindow {
                utilization: None,
                resets_at: None,
            }),
            limits: Some(vec![
                serde_json::to_value(UsageLimit {
                    kind: "weekly_scoped".into(),
                    percent: 3.0,
                    resets_at: None,
                    scope: Some(UsageScope {
                        model: Some(DisplayName {
                            display_name: "Fable".into(),
                        }),
                    }),
                })
                .unwrap(),
            ]),
        };
        let json = serde_json::to_string(&response).unwrap();
        let back: UsageResponse = serde_json::from_str(&json).unwrap();
        assert_eq!(back, response);
        let limit: UsageLimit = serde_json::from_value(back.limits.unwrap().remove(0)).unwrap();
        assert_eq!(limit.scope.unwrap().model.unwrap().display_name, "Fable");
        let buckets = buckets(&response);
        assert_eq!(buckets.len(), 2);
        assert_eq!(buckets[0].key.as_str(), "fable");
        assert_eq!(buckets[1].key.as_str(), "opus");
    }

    #[test]
    fn cache_entries_round_trip_and_tolerate_missing_fields() {
        let entry = Entry {
            fetched_at: 5,
            body: Some(serde_json::json!({"limits": []})),
            retry_after: 9,
            rate_limited: 2,
            last_error: Some("x".into()),
        };
        let json = serde_json::to_string(&entry).unwrap();
        assert_eq!(serde_json::from_str::<Entry>(&json).unwrap(), entry);
        let sparse: Entry = serde_json::from_str("{}").unwrap();
        assert_eq!(sparse, Entry::default());
    }

    #[test]
    fn merging_replaces_same_keyed_buckets_or_records_the_failure() {
        let mut reading = Reading::failed("a", 0, crate::quota::ProbeError::NoCredential);
        reading
            .buckets
            .push(bucket("Opus 5", "Opus 5", 0.1, Source::Carried));
        merge(
            &mut reading,
            Some(Ok(vec![bucket("Opus 5", "Opus 5", 0.4, Source::Profile)])),
        );
        assert_eq!(reading.buckets.len(), 1);
        assert_eq!(reading.buckets[0].source, Source::Profile);
        merge(&mut reading, Some(Err("usage nope".into())));
        assert_eq!(reading.profile_error.as_deref(), Some("usage nope"));
        merge(&mut reading, None);
        assert_eq!(reading.buckets.len(), 1);
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
    async fn a_young_answer_is_reused_and_a_429_serves_the_last_good_one() {
        let good = serde_json::json!({"seven_day_opus": {"utilization": 30.0, "resets_at": null}});
        let (router, calls) = scripted(vec![
            (StatusCode::OK, None, good.clone()),
            (
                StatusCode::TOO_MANY_REQUESTS,
                Some(7200),
                serde_json::json!({}),
            ),
        ]);
        let client = serve(router).await;
        let (_dir, cache) = temp_cache();
        let read = |now, fresh| cache.read(&client, "acct", "tok", Endpoint::Usage, fresh, now);

        assert_eq!(read(1000, false).await.unwrap(), good);
        assert_eq!(read(1000 + MAX_AGE_SECS - 1, false).await.unwrap(), good);
        assert_eq!(calls.load(Ordering::SeqCst), 1, "a young answer is reused");

        // Stale: asked again, refused, last good answer served.
        assert_eq!(read(1000 + MAX_AGE_SECS, false).await.unwrap(), good);
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        let entry = cache.load().remove("acct|usage").unwrap();
        assert_eq!(entry.rate_limited, 1);
        assert_eq!(
            entry.retry_after,
            1000 + MAX_AGE_SECS + 7200,
            "Retry-After wins"
        );
        assert!(entry.last_error.unwrap().contains("HTTP 429"));

        // Backed off: no request, the cache answers; a fresh read refuses.
        assert_eq!(read(1000 + MAX_AGE_SECS + 10, false).await.unwrap(), good);
        let refused = read(1000 + MAX_AGE_SECS + 10, true).await.unwrap_err();
        assert!(refused.to_string().contains("backing off"), "{refused}");
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn first_failures_without_a_good_answer_are_errors_and_the_file_is_private() {
        let (router, calls) = scripted(vec![
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                None,
                serde_json::json!("boom"),
            ),
            (StatusCode::OK, None, serde_json::json!({"limits": []})),
        ]);
        let client = serve(router).await;
        let (dir, cache) = temp_cache();
        let read = |now| cache.read(&client, "acct", "tok", Endpoint::Usage, false, now);
        let error = read(100).await.unwrap_err();
        assert!(error.to_string().contains("HTTP 500"), "{error}");
        let entry = cache.load().remove("acct|usage").unwrap();
        assert_eq!(entry.retry_after, 100 + ERROR_RETRY_SECS);
        assert_eq!(entry.rate_limited, 0);
        // Still inside the minute: no request.
        assert!(read(150).await.is_err());
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        // After it: fetched, and the entry's backoff state is cleared.
        read(161).await.unwrap();
        let entry = cache.load().remove("acct|usage").unwrap();
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
