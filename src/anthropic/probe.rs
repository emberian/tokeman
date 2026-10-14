//! The Haiku probe: one 1-token request whose rate-limit headers describe the
//! account's windows, merged with the profile endpoint's per-model buckets.
//!
//! Headers vary by the model a request used. The model-specific weekly window
//! (`7d_oi`, the Fable limit) only appears on requests to that family, which
//! is what the occasional target-model sample is for.

use std::time::Duration;

use reqwest::header::HeaderMap;
use serde_json::json;

use super::{CLAUDE_CODE_IDENTITY, Client, http_error, network_error, profile};
use crate::account::{Account, Credential};
use crate::quota::{
    Bucket, Family, ModelKey, Overage, ProbeError, Reading, Source, Verdict, Window,
};

pub const PROBE_MODEL: &str = "claude-haiku-4-5-20251001";
pub const PROBE_MODEL_LABEL: &str = "Haiku 4.5";
/// A slow-but-valid 20-40 s answer is safer than a partial batch.
pub const PROBE_TIMEOUT: Duration = Duration::from_secs(45);
/// Families whose weekly limit appears only in the headers of requests to them.
const HEADER_ONLY_FAMILIES: &[Family] = &[Family::Fable];

/// What the unified rate-limit headers of one response say.
#[derive(Debug, Clone, PartialEq)]
pub struct Quota {
    pub verdict: Verdict,
    pub claim: Option<String>,
    pub session: Option<Window>,
    pub weekly: Option<Window>,
    /// The `7d_oi` window: the requested model's own weekly limit.
    pub model_weekly: Option<Window>,
    pub overage: Option<Overage>,
}

fn header_str(headers: &HeaderMap, key: &str) -> Option<String> {
    headers
        .get(key)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned)
}

fn header_window(headers: &HeaderMap, prefix: &str) -> Option<Window> {
    let used = header_str(headers, &format!("{prefix}-utilization"))?
        .parse()
        .ok()?;
    let resets_at = header_str(headers, &format!("{prefix}-reset"))
        .and_then(|value| value.parse().ok())
        .unwrap_or(0);
    Some(Window::new(used, resets_at))
}

/// `None` without a `unified-status` header; a 429 still carries them.
pub fn parse_quota(headers: &HeaderMap) -> Option<Quota> {
    const PREFIX: &str = "anthropic-ratelimit-unified";
    let status = header_str(headers, &format!("{PREFIX}-status"))?;
    let overage_status = header_str(headers, &format!("{PREFIX}-overage-status"));
    let overage_window = header_window(headers, &format!("{PREFIX}-overage"));
    let overage = (overage_status.is_some() || overage_window.is_some()).then(|| Overage {
        status: overage_status,
        window: overage_window,
        disabled_reason: header_str(headers, &format!("{PREFIX}-overage-disabled-reason")),
        in_use: header_str(headers, &format!("{PREFIX}-overage-in-use"))
            .is_some_and(|value| value == "true"),
    });
    Some(Quota {
        verdict: Verdict::parse(&status),
        claim: header_str(headers, &format!("{PREFIX}-representative-claim"))
            .filter(|claim| !claim.is_empty()),
        session: header_window(headers, &format!("{PREFIX}-5h")),
        weekly: header_window(headers, &format!("{PREFIX}-7d")),
        model_weekly: header_window(headers, &format!("{PREFIX}-7d_oi")),
        overage,
    })
}

/// The model to sample with, when the startup model's family has a
/// header-only limit. `[1m]` is a Claude Code context option, not part of the
/// API id; a bare alias (`fable`) names no API model, so it is not sampled.
pub fn sample_model(target: Option<&str>) -> Option<&str> {
    let base = target?.split('[').next()?.trim();
    let family = Family::of(base)?;
    (base.starts_with("claude-") && HEADER_ONLY_FAMILIES.contains(&family)).then_some(base)
}

/// Record the `7d_oi` window as the family-wide Fable bucket, unless a
/// bucket of that family is already known (the profile endpoint's row).
pub fn attach_model_weekly(reading: &mut Reading, window: Option<Window>) {
    let Some(window) = window else {
        return;
    };
    if reading
        .buckets
        .iter()
        .any(|bucket| bucket.key.family() == Some(Family::Fable))
    {
        return;
    }
    reading.buckets.push(Bucket {
        key: ModelKey::family_wide(Family::Fable),
        label: Family::Fable.label().to_owned(),
        window,
        source: Source::Headers,
    });
}

/// One request to `/v1/messages` on `model`; the quota headers, or why not.
/// A 429 that carries them is a reading; one without is a refusal.
async fn request(
    client: &Client,
    credential: Credential<'_>,
    model: &str,
    timeout: Duration,
) -> Result<Option<Quota>, ProbeError> {
    let body = json!({
        "model": model,
        "max_tokens": 1,
        "system": [{"type": "text", "text": CLAUDE_CODE_IDENTITY}],
        "messages": [{"role": "user", "content": "quota"}]
    });
    let response = client
        .messages(credential)
        .timeout(timeout)
        .json(&body)
        .send()
        .await
        .map_err(|error| network_error(&error))?;
    let status = response.status();
    let quota = parse_quota(response.headers());
    if !status.is_success() && (status.as_u16() != 429 || quota.is_none()) {
        let text = response.text().await.unwrap_or_default();
        return Err(http_error(status, text));
    }
    // Drain the body so the connection is reused.
    let _ = response.text().await;
    Ok(quota)
}

/// A reading of the shared windows; the model's own window is attached by
/// the caller once it knows what the profile endpoint said.
fn reading_of(account: &str, now: i64, quota: &Option<Quota>) -> Reading {
    let mut reading = Reading {
        account: account.to_owned(),
        at: now,
        verdict: Verdict::Unknown,
        claim: None,
        session: None,
        weekly: None,
        buckets: Vec::new(),
        overage: None,
        error: None,
        profile_error: None,
    };
    if let Some(quota) = quota {
        reading.verdict = quota.verdict;
        reading.claim = quota.claim.clone();
        reading.session = quota.session;
        reading.weekly = quota.weekly;
        reading.overage = quota.overage.clone();
    }
    reading
}

fn model_weekly(quota: &Option<Quota>) -> Option<Window> {
    quota.as_ref().and_then(|quota| quota.model_weekly)
}

/// Probe `account` with `model` and fold in the profile endpoint (through
/// `cache`). `now` is epoch seconds.
pub async fn probe_with(
    client: &Client,
    cache: &profile::Cache,
    account: &Account,
    model: &str,
    timeout: Duration,
    now: i64,
) -> Reading {
    let Some(credential) = account.credential(now * 1000) else {
        return Reading::failed(&account.name, now, ProbeError::NoCredential);
    };
    let (answer, usage) = futures::join!(
        request(client, credential, model, timeout),
        profile::usage(client, cache, account, now),
    );
    let mut reading = match &answer {
        Ok(quota) => reading_of(&account.name, now, quota),
        Err(error) => Reading::failed(&account.name, now, error.clone()),
    };
    profile::merge(&mut reading, usage);
    if let Ok(quota) = &answer {
        attach_model_weekly(&mut reading, model_weekly(quota));
    }
    reading
}

/// Probe one account with the default cache and timeout.
pub async fn probe(client: &Client, account: &Account, model: &str, now: i64) -> Reading {
    probe_with(
        client,
        &profile::Cache::default(),
        account,
        model,
        PROBE_TIMEOUT,
        now,
    )
    .await
}

/// Probe every account concurrently with `model`, each bounded by `timeout`.
pub async fn probe_all<'a>(
    client: &Client,
    accounts: impl IntoIterator<Item = &'a Account>,
    model: &str,
    timeout: Duration,
    now: i64,
) -> Vec<Reading> {
    let cache = profile::Cache::default();
    futures::future::join_all(
        accounts
            .into_iter()
            .map(|account| probe_with(client, &cache, account, model, timeout, now)),
    )
    .await
}

/// The target-model sample: the same request on `model`, without the
/// profile read, so its `7d_oi` window lands as a `Source::Headers` bucket.
/// An answer without quota headers is a reading with an error.
pub async fn sample(
    client: &Client,
    account: &Account,
    model: &str,
    timeout: Duration,
    now: i64,
) -> Reading {
    let Some(credential) = account.credential(now * 1000) else {
        return Reading::failed(&account.name, now, ProbeError::NoCredential);
    };
    match request(client, credential, model, timeout).await {
        Ok(quota) => {
            let mut reading = reading_of(&account.name, now, &quota);
            attach_model_weekly(&mut reading, model_weekly(&quota));
            reading
        }
        Err(error) => Reading::failed(&account.name, now, error),
    }
}

pub async fn sample_all<'a>(
    client: &Client,
    accounts: impl IntoIterator<Item = &'a Account>,
    model: &str,
    timeout: Duration,
    now: i64,
) -> Vec<Reading> {
    futures::future::join_all(
        accounts
            .into_iter()
            .map(|account| sample(client, account, model, timeout, now)),
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::anthropic::testing::serve;
    use axum::Router;
    use axum::http::{HeaderMap as AxumHeaders, StatusCode};
    use axum::routing::{get, post};
    use reqwest::header::HeaderValue;

    fn headers(pairs: &[(&'static str, &str)]) -> HeaderMap {
        let mut map = HeaderMap::new();
        for (key, value) in pairs {
            map.insert(*key, HeaderValue::from_str(value).unwrap());
        }
        map
    }

    #[test]
    fn unified_headers_become_a_quota_and_their_absence_none() {
        assert!(parse_quota(&headers(&[("x", "y")])).is_none());
        let quota = parse_quota(&headers(&[
            ("anthropic-ratelimit-unified-status", "allowed_warning"),
            (
                "anthropic-ratelimit-unified-representative-claim",
                "five_hour",
            ),
            ("anthropic-ratelimit-unified-5h-utilization", "0.8"),
            ("anthropic-ratelimit-unified-5h-reset", "1700000000"),
            ("anthropic-ratelimit-unified-7d-utilization", "0.25"),
            ("anthropic-ratelimit-unified-7d_oi-utilization", "0.5"),
            ("anthropic-ratelimit-unified-7d_oi-reset", "1700100000"),
            ("anthropic-ratelimit-unified-overage-status", "disabled"),
            (
                "anthropic-ratelimit-unified-overage-disabled-reason",
                "org_level_disabled",
            ),
        ]))
        .unwrap();
        assert_eq!(quota.verdict, Verdict::AllowedWarning);
        assert_eq!(quota.claim.as_deref(), Some("five_hour"));
        assert_eq!(quota.session, Some(Window::new(0.8, 1_700_000_000)));
        assert_eq!(
            quota.weekly,
            Some(Window::new(0.25, 0)),
            "no reset reads as 0"
        );
        assert_eq!(quota.model_weekly, Some(Window::new(0.5, 1_700_100_000)));
        let overage = quota.overage.unwrap();
        assert_eq!(overage.status.as_deref(), Some("disabled"));
        assert_eq!(
            overage.disabled_reason.as_deref(),
            Some("org_level_disabled")
        );
        assert!(!overage.in_use);
        assert!(overage.window.is_none());

        let bare = parse_quota(&headers(&[
            ("anthropic-ratelimit-unified-status", "rejected"),
            ("anthropic-ratelimit-unified-representative-claim", ""),
            ("anthropic-ratelimit-unified-5h-utilization", "not a number"),
        ]))
        .unwrap();
        assert_eq!(bare.verdict, Verdict::Rejected);
        assert_eq!(bare.claim, None);
        assert_eq!(bare.session, None);
        assert_eq!(bare.overage, None);
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
    fn the_model_weekly_window_becomes_a_fable_bucket_unless_the_profile_has_one() {
        let mut reading = reading_of("a", 0, &None);
        attach_model_weekly(&mut reading, None);
        assert!(reading.buckets.is_empty());
        attach_model_weekly(&mut reading, Some(Window::new(0.7, 5_000)));
        assert_eq!(reading.buckets.len(), 1);
        let fable = reading
            .bucket(&ModelKey::family_wide(Family::Fable))
            .unwrap();
        assert_eq!(fable.label, "Fable");
        assert_eq!(fable.source, Source::Headers);
        assert_eq!(fable.window, Window::new(0.7, 5_000));
        assert!(ModelKey::new("claude-fable-5-1").is_limited_by(&fable.key));

        let mut with_profile = reading_of("a", 0, &None);
        with_profile.buckets.push(Bucket {
            key: ModelKey::new("Fable 5"),
            label: "Fable 5".into(),
            window: Window::new(0.1, 1),
            source: Source::Profile,
        });
        attach_model_weekly(&mut with_profile, Some(Window::new(0.7, 5_000)));
        assert_eq!(with_profile.buckets.len(), 1);
        assert_eq!(with_profile.buckets[0].source, Source::Profile);
    }

    #[tokio::test]
    async fn an_account_without_a_credential_is_refused_without_a_request() {
        let account = Account::new("empty");
        let reading = probe(
            &Client::with_base("http://127.0.0.1:9", "http://127.0.0.1:9/t"),
            &account,
            PROBE_MODEL,
            7,
        )
        .await;
        assert_eq!(reading.error, Some(ProbeError::NoCredential));
        assert!(reading.is_refused());
        assert_eq!(reading.at, 7);
    }

    fn quota_headers(status: &str, model_weekly: Option<&str>) -> axum::http::HeaderMap {
        let mut headers = AxumHeaders::new();
        headers.insert(
            "anthropic-ratelimit-unified-status",
            status.parse().unwrap(),
        );
        headers.insert(
            "anthropic-ratelimit-unified-5h-utilization",
            "0.3".parse().unwrap(),
        );
        headers.insert(
            "anthropic-ratelimit-unified-5h-reset",
            "100".parse().unwrap(),
        );
        headers.insert(
            "anthropic-ratelimit-unified-7d-utilization",
            "0.6".parse().unwrap(),
        );
        headers.insert(
            "anthropic-ratelimit-unified-7d-reset",
            "200".parse().unwrap(),
        );
        if let Some(used) = model_weekly {
            headers.insert(
                "anthropic-ratelimit-unified-7d_oi-utilization",
                used.parse().unwrap(),
            );
        }
        headers
    }

    /// `/v1/messages` answers by model: Haiku allowed with headers, Fable 5.1
    /// a 429 with headers (limited), Fable 5.2 a bare 429 (refused), Opus a
    /// 401; `/api/oauth/usage` names Fable 5 at 90%.
    fn fake_api() -> Router {
        Router::new()
            .route(
                "/v1/messages",
                post(|headers: AxumHeaders, body: String| async move {
                    let body: serde_json::Value = serde_json::from_str(&body).unwrap();
                    assert_eq!(body["max_tokens"], 1);
                    assert_eq!(body["system"][0]["text"], CLAUDE_CODE_IDENTITY);
                    assert!(headers.contains_key("anthropic-version"));
                    assert!(
                        headers
                            .get("user-agent")
                            .and_then(|v| v.to_str().ok())
                            .is_some_and(|ua| ua.starts_with("claude-cli/"))
                    );
                    let model = body["model"].as_str().unwrap_or_default();
                    let (status, headers) = match model {
                        "claude-opus-4-8" => (StatusCode::UNAUTHORIZED, AxumHeaders::new()),
                        "claude-fable-5-2" => (StatusCode::TOO_MANY_REQUESTS, AxumHeaders::new()),
                        "claude-fable-5-1" => (
                            StatusCode::TOO_MANY_REQUESTS,
                            quota_headers("rejected", Some("1.0")),
                        ),
                        _ => (StatusCode::OK, quota_headers("allowed", None)),
                    };
                    (
                        status,
                        headers,
                        r#"{"type":"error","error":{"message":"revoked"}}"#,
                    )
                }),
            )
            .route(
                "/api/oauth/usage",
                get(|| async {
                    axum::Json(serde_json::json!({
                        "limits": [{"kind": "weekly_scoped", "percent": 90.0,
                                    "scope": {"model": {"display_name": "Fable 5"}}}]
                    }))
                }),
            )
    }

    fn setup_account(name: &str) -> Account {
        let mut account = Account::new(name);
        account.setup_key = Some("sk-ant-oat01-setup".into());
        account
    }

    #[tokio::test]
    async fn a_probe_reads_the_headers_and_classifies_refusals() {
        let client = serve(fake_api()).await;
        let dir = tempfile::tempdir().unwrap();
        let cache = profile::Cache::at(dir.path().join("reads.json"));
        let account = setup_account("a");
        let probe = |model| probe_with(&client, &cache, &account, model, PROBE_TIMEOUT, 50);

        let haiku = probe(PROBE_MODEL).await;
        assert_eq!(haiku.error, None);
        assert_eq!(haiku.verdict, Verdict::Allowed);
        assert_eq!(haiku.session, Some(Window::new(0.3, 100)));
        assert_eq!(haiku.weekly, Some(Window::new(0.6, 200)));
        assert!(haiku.buckets.is_empty(), "a setup key reads no profile");
        assert_eq!(haiku.profile_error, None);
        assert!(haiku.has_quota());

        // A 429 with quota headers is a reading, and the 7d_oi window a bucket.
        let fable = probe("claude-fable-5-1").await;
        assert_eq!(fable.error, None);
        assert_eq!(fable.verdict, Verdict::Rejected);
        assert!(fable.has_quota());
        let bucket = fable.bucket(&ModelKey::family_wide(Family::Fable)).unwrap();
        assert_eq!(bucket.source, Source::Headers);
        assert_eq!(bucket.window.used, 1.0);

        let opus = probe("claude-opus-4-8").await;
        assert_eq!(opus.verdict, Verdict::Unknown);
        assert!(opus.is_refused());
        assert_eq!(opus.error.as_ref().unwrap().reason(), "401 revoked");
    }

    #[tokio::test]
    async fn a_bare_429_is_a_refusal_and_the_profile_row_outranks_the_headers() {
        let client = serve(fake_api()).await;
        let dir = tempfile::tempdir().unwrap();
        let cache = profile::Cache::at(dir.path().join("reads.json"));

        let bare = sample(
            &client,
            &setup_account("k"),
            "claude-fable-5-2",
            PROBE_TIMEOUT,
            1,
        )
        .await;
        assert!(
            matches!(bare.error, Some(ProbeError::Http { status: 429, .. })),
            "{bare:?}"
        );
        assert!(!bare.is_refused(), "a 429 is not definitive");
        assert!(bare.buckets.is_empty());

        let mut login = setup_account("l");
        login.capture_profile_token("profile-token".into());
        let reading = probe_with(
            &client,
            &cache,
            &login,
            "claude-fable-5-1",
            PROBE_TIMEOUT,
            1,
        )
        .await;
        assert_eq!(reading.buckets.len(), 1, "{:?}", reading.buckets);
        assert_eq!(reading.buckets[0].key.as_str(), "fable5");
        assert_eq!(reading.buckets[0].source, Source::Profile);
        assert_eq!(reading.buckets[0].window.used, 0.9);

        let all = probe_all(
            &client,
            &[login.clone(), Account::new("none")],
            PROBE_MODEL,
            PROBE_TIMEOUT,
            2,
        )
        .await;
        assert_eq!(all.len(), 2);
        assert_eq!(all[0].account, "l");
        assert_eq!(all[1].error, Some(ProbeError::NoCredential));
        let samples = sample_all(&client, &[login], "claude-fable-5-1", PROBE_TIMEOUT, 3).await;
        assert_eq!(
            samples[0].buckets[0].source,
            Source::Headers,
            "a sample skips the profile"
        );
    }

    #[tokio::test]
    async fn a_timeout_is_a_network_error() {
        let router = Router::new().route(
            "/v1/messages",
            post(|| async {
                tokio::time::sleep(Duration::from_secs(5)).await;
                StatusCode::OK
            }),
        );
        let client = serve(router).await;
        let reading = sample(
            &client,
            &setup_account("slow"),
            PROBE_MODEL,
            Duration::from_millis(200),
            0,
        )
        .await;
        assert!(
            matches!(reading.error, Some(ProbeError::Network { .. })),
            "{reading:?}"
        );
        assert!(!reading.is_refused());
    }
}
