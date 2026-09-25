use chrono::{DateTime, Utc};
use reqwest::header::{CONTENT_TYPE, HeaderMap, HeaderValue};
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::config::Token;

pub const PROBE_MODEL: &str = "claude-haiku-4-5-20251001";
pub const PROBE_MODEL_LABEL: &str = "Haiku 4.5";

/// Utilization data for a single rate limit window (5h, 7d, or overage).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Window {
    pub utilization: f64,
    pub reset: i64,
}

impl Window {
    /// Fraction of the window left (unclamped).
    pub fn remaining(&self) -> f64 {
        1.0 - self.utilization
    }
}

/// Health bucket for a remaining fraction, shared by every UI's color scheme.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    Ok,
    Low,
    Critical,
}

pub fn level(remaining: f64) -> Level {
    if remaining > 0.50 {
        Level::Ok
    } else if remaining > 0.20 {
        Level::Low
    } else {
        Level::Critical
    }
}

#[derive(Debug, Clone, Serialize)]
#[allow(dead_code)] // all fields populated from API response headers
pub struct UnifiedQuota {
    /// "allowed", "allowed_warning", or "rejected"
    pub status: String,
    pub reset: i64,
    /// Which claim is authoritative: "five_hour", "seven_day",
    /// "seven_day_overage_included" (the Fable limit), "seven_day_opus",
    /// "seven_day_sonnet", or "overage"
    pub representative_claim: String,
    /// "available" if fallback exists
    pub fallback: Option<String>,
    /// Per-window utilization
    pub session: Option<Window>, // 5h
    pub weekly: Option<Window>, // 7d
    /// The Fable weekly limit (`7d_oi`, "seven day, overage included"): the
    /// share of the weekly allowance Fable models may use before they need
    /// usage credits. Headers vary by the model a request used, so a probe on
    /// another model may not carry it; the profile endpoint's per-model rows do.
    pub fable: Option<Window>,
    /// Overage / extra usage
    pub overage_status: Option<String>,
    pub overage: Option<Window>,
    pub overage_disabled_reason: Option<String>,
    /// Requests are currently being paid for with usage credits.
    pub overage_in_use: bool,
}

impl UnifiedQuota {
    /// Session, weekly and overage windows that are present, paired with the
    /// caller's label for each (in that order).
    pub fn windows(
        &self,
        labels: [&'static str; 3],
    ) -> impl Iterator<Item = (&'static str, &Window)> {
        labels
            .into_iter()
            .zip([&self.session, &self.weekly, &self.overage])
            .filter_map(|(label, window)| Some((label, window.as_ref()?)))
    }
}

#[derive(Default, Debug, Clone, Serialize)]
pub struct RateLimits {
    pub requests_limit: Option<i64>,
    pub requests_remaining: Option<i64>,
    pub input_tokens_limit: Option<i64>,
    pub input_tokens_remaining: Option<i64>,
    pub output_tokens_limit: Option<i64>,
    pub output_tokens_remaining: Option<i64>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ProbeResult {
    pub token_name: String,
    pub probed_at: DateTime<Utc>,
    pub quota: Option<UnifiedQuota>,
    pub model_usage: Option<ModelUsage>,
    pub model_usage_error: Option<String>,
    pub rate_limits: RateLimits,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ModelUsageSource {
    Profile,
    ObservedRejection,
    /// A rate-limit header window, e.g. the `7d_oi` Fable limit.
    Headers,
}

impl ModelUsageSource {
    pub fn label(self) -> &'static str {
        match self {
            Self::Profile => "profile",
            Self::ObservedRejection => "observed Claude rejection",
            Self::Headers => "rate-limit headers",
        }
    }

    pub fn short_label(self) -> &'static str {
        match self {
            Self::Profile => "profile",
            Self::ObservedRejection => "observed!",
            Self::Headers => "headers",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelQuotaBucket {
    /// Stable, normalized provider identifier used for model matching.
    pub key: String,
    /// Provider display label, for example "Opus 5" or "Fable".
    pub label: String,
    pub window: Window,
    pub source: ModelUsageSource,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ModelUsage {
    /// Legacy family-wide fields retained for history/chart compatibility.
    pub opus_weekly: Option<Window>,
    pub sonnet_weekly: Option<Window>,
    pub opus_source: Option<ModelUsageSource>,
    pub sonnet_source: Option<ModelUsageSource>,
    /// Dynamically discovered provider-scoped weekly buckets.
    #[serde(default)]
    pub scoped_weekly: Vec<ModelQuotaBucket>,
}

impl ModelUsage {
    /// All known model buckets, with legacy family-wide fields represented as
    /// ordinary buckets when the dynamic response did not already provide one.
    pub fn buckets(&self) -> Vec<ModelQuotaBucket> {
        let mut buckets = self.scoped_weekly.clone();
        for (key, label, window, source) in [
            ("opus", "Opus", self.opus_weekly.as_ref(), self.opus_source),
            (
                "sonnet",
                "Sonnet",
                self.sonnet_weekly.as_ref(),
                self.sonnet_source,
            ),
        ] {
            if let Some(window) = window
                && !buckets.iter().any(|bucket| bucket.key == key)
            {
                buckets.push(ModelQuotaBucket {
                    key: key.into(),
                    label: label.into(),
                    window: window.clone(),
                    source: source.unwrap_or(ModelUsageSource::Profile),
                });
            }
        }
        buckets
    }
}

// The `/api/oauth/usage` response, as Claude Code's own client reads it.
// Every window is `{utilization: percent 0-100 | null, resets_at: ISO 8601 |
// null}`. `limits` is the server's list of meters, meant to be rendered
// verbatim; it is null from older servers.
#[derive(Debug, Deserialize)]
struct UsageWindowResponse {
    utilization: Option<f64>,
    resets_at: Option<String>,
}

#[derive(Debug, Deserialize)]
struct UsageResponse {
    seven_day_opus: Option<UsageWindowResponse>,
    seven_day_sonnet: Option<UsageWindowResponse>,
    /// Kept as raw values so one unexpected row cannot fail the whole parse.
    limits: Option<Vec<serde_json::Value>>,
}

#[derive(Debug, Deserialize)]
struct UsageLimitResponse {
    /// Classify rows on this: "session", "weekly_all", "weekly_scoped", ...
    kind: String,
    /// 0-100.
    percent: f64,
    resets_at: Option<String>,
    scope: Option<UsageScopeResponse>,
}

#[derive(Debug, Deserialize)]
struct UsageScopeResponse {
    model: Option<DisplayName>,
}

#[derive(Debug, Deserialize)]
struct DisplayName {
    display_name: String,
}

fn header_str(headers: &HeaderMap, key: &str) -> Option<String> {
    headers
        .get(key)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string())
}

fn header_f64(headers: &HeaderMap, key: &str) -> Option<f64> {
    header_str(headers, key).and_then(|s| s.parse().ok())
}

fn header_i64(headers: &HeaderMap, key: &str) -> Option<i64> {
    header_str(headers, key).and_then(|s| s.parse().ok())
}

fn parse_window(headers: &HeaderMap, prefix: &str) -> Option<Window> {
    let utilization = header_f64(headers, &format!("{prefix}-utilization"))?;
    Some(Window {
        utilization,
        reset: header_i64(headers, &format!("{prefix}-reset")).unwrap_or(0),
    })
}

fn parse_unified_quota(headers: &HeaderMap) -> Option<UnifiedQuota> {
    let status = header_str(headers, "anthropic-ratelimit-unified-status")?;
    Some(UnifiedQuota {
        status,
        reset: header_i64(headers, "anthropic-ratelimit-unified-reset").unwrap_or(0),
        representative_claim: header_str(
            headers,
            "anthropic-ratelimit-unified-representative-claim",
        )
        .unwrap_or_default(),
        fallback: header_str(headers, "anthropic-ratelimit-unified-fallback"),
        session: parse_window(headers, "anthropic-ratelimit-unified-5h"),
        weekly: parse_window(headers, "anthropic-ratelimit-unified-7d"),
        fable: parse_window(headers, "anthropic-ratelimit-unified-7d_oi"),
        overage_status: header_str(headers, "anthropic-ratelimit-unified-overage-status"),
        overage: parse_window(headers, "anthropic-ratelimit-unified-overage"),
        overage_disabled_reason: header_str(
            headers,
            "anthropic-ratelimit-unified-overage-disabled-reason",
        ),
        overage_in_use: header_str(headers, "anthropic-ratelimit-unified-overage-in-use")
            .is_some_and(|value| value == "true"),
    })
}

fn parse_rate_limits(headers: &HeaderMap) -> RateLimits {
    RateLimits {
        requests_limit: header_i64(headers, "anthropic-ratelimit-requests-limit"),
        requests_remaining: header_i64(headers, "anthropic-ratelimit-requests-remaining"),
        input_tokens_limit: header_i64(headers, "anthropic-ratelimit-input-tokens-limit"),
        input_tokens_remaining: header_i64(headers, "anthropic-ratelimit-input-tokens-remaining"),
        output_tokens_limit: header_i64(headers, "anthropic-ratelimit-output-tokens-limit"),
        output_tokens_remaining: header_i64(headers, "anthropic-ratelimit-output-tokens-remaining"),
    }
}

fn usage_window(window: UsageWindowResponse) -> Option<Window> {
    Some(Window {
        // The profile endpoint reports percentages; message headers report a
        // 0..1 fraction. Normalize once at the boundary.
        utilization: (window.utilization? / 100.0).clamp(0.0, 1.0),
        reset: parse_reset(window.resets_at.as_deref()),
    })
}

/// Canonical identity for a model or a per-model bucket, so a profile row
/// labeled "Opus 4.8" and a rejection naming `claude-opus-4-8` land on the
/// same key: lowercase alphanumerics, without the `claude` prefix.
pub(crate) fn model_key(value: &str) -> String {
    let key: String = value
        .chars()
        .filter(|ch| ch.is_ascii_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect();
    match key.strip_prefix("claude") {
        Some(rest) if !rest.is_empty() => rest.to_owned(),
        _ => key,
    }
}

fn parse_reset(resets_at: Option<&str>) -> i64 {
    // An unused bucket may have no reset yet; 0 renders as "--".
    resets_at
        .and_then(|value| DateTime::parse_from_rfc3339(value).ok())
        .map_or(0, |time| time.timestamp())
}

/// A per-model weekly row, identified the way Claude Code identifies it: a
/// `weekly_scoped` meter whose scope names a model. Surface-scoped rows are
/// not models. Every such row counts, including idle ones; `is_active` only
/// marks the server's headline row.
fn scoped_model_bucket(row: serde_json::Value) -> Option<ModelQuotaBucket> {
    let row: UsageLimitResponse = serde_json::from_value(row).ok()?;
    if row.kind != "weekly_scoped" {
        return None;
    }
    let label = row.scope?.model?.display_name;
    let key = model_key(&label);
    (!key.is_empty()).then(|| ModelQuotaBucket {
        key,
        label,
        window: Window {
            utilization: (row.percent / 100.0).clamp(0.0, 1.0),
            reset: parse_reset(row.resets_at.as_deref()),
        },
        source: ModelUsageSource::Profile,
    })
}

fn parse_model_usage(usage: UsageResponse) -> ModelUsage {
    let opus_weekly = usage.seven_day_opus.and_then(usage_window);
    let sonnet_weekly = usage.seven_day_sonnet.and_then(usage_window);
    let mut scoped_weekly = usage
        .limits
        .unwrap_or_default()
        .into_iter()
        .filter_map(scoped_model_bucket)
        .collect::<Vec<_>>();
    scoped_weekly.sort_by(|a, b| a.key.cmp(&b.key));
    scoped_weekly.dedup_by(|a, b| a.key == b.key);
    scoped_weekly.sort_by(|a, b| a.label.cmp(&b.label));
    ModelUsage {
        opus_source: opus_weekly.as_ref().map(|_| ModelUsageSource::Profile),
        sonnet_source: sonnet_weekly.as_ref().map(|_| ModelUsageSource::Profile),
        opus_weekly,
        sonnet_weekly,
        scoped_weekly,
    }
}

async fn probe_model_usage(
    client: &reqwest::Client,
    token: &Token,
) -> (Option<ModelUsage>, Option<String>) {
    let Some(usage_key) = token.usage_credential(Utc::now().timestamp_millis()) else {
        return (None, None);
    };
    let response = client
        .get("https://api.anthropic.com/api/oauth/usage")
        .header("Authorization", format!("Bearer {usage_key}"))
        .header("anthropic-beta", "oauth-2025-04-20")
        .header("user-agent", client_user_agent())
        .send()
        .await;
    match response {
        Ok(response) if response.status().is_success() => {
            match response.json::<UsageResponse>().await {
                Ok(usage) => (Some(parse_model_usage(usage)), None),
                Err(error) => (None, Some(format!("usage response parse failed: {error}"))),
            }
        }
        Ok(response) => {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            (None, Some(format!("usage HTTP {status}: {body}")))
        }
        Err(error) => (None, Some(format!("usage request failed: {error}"))),
    }
}

pub async fn validate_usage_key(usage_key: &str) -> Result<ModelUsage, String> {
    let client = reqwest::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(8))
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .map_err(|error| error.to_string())?;
    let token = Token {
        name: "validation".into(),
        access_token: Some(usage_key.into()),
        ..Token::default()
    };
    let (usage, error) = probe_model_usage(&client, &token).await;
    usage.ok_or_else(|| error.unwrap_or_else(|| "usage data unavailable".into()))
}

/// The user agent Claude Code's API client sends,
/// `claude-cli/<version> (external, cli)`, for the installed version. The
/// server derives the requesting surface from it: under any other agent the
/// usage-reset programs answer `ineligible_reason: "surface"`. Read once per
/// process; falls back to a recent version if `claude` cannot be run.
pub fn client_user_agent() -> &'static str {
    static AGENT: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    AGENT.get_or_init(|| {
        let version = std::process::Command::new(
            std::env::var("TOKEMAN_CLAUDE_BIN").unwrap_or_else(|_| "claude".into()),
        )
        .arg("--version")
        .output()
        .ok()
        .filter(|output| output.status.success())
        .and_then(|output| {
            String::from_utf8_lossy(&output.stdout)
                .split_whitespace()
                .next()
                .filter(|version| version.chars().all(|c| c.is_ascii_digit() || c == '.'))
                .map(str::to_owned)
        });
        format!(
            "claude-cli/{} (external, cli)",
            version.as_deref().unwrap_or("2.1.282")
        )
    })
}

/// The system prompt Claude Code opens every request with.
const CLAUDE_CODE_IDENTITY: &str = "You are Claude Code, Anthropic's official CLI for Claude.";

/// Error reported for an account with neither a live login nor a setup token.
/// Rotation treats it like a refused credential: it is an answer, not a
/// network hiccup worth retrying on the fast cadence.
pub const NO_CREDENTIAL: &str = "no usable credential";

/// Probe one token with a 1-token request to `model`.
///
/// Rate-limit headers vary by the model a request used: some windows (the
/// Fable limit) only appear on requests to that family, which is what an
/// occasional target-model sample is for.
pub async fn probe_token(client: &reqwest::Client, token: &Token, model: &str) -> ProbeResult {
    let probed_at = Utc::now();
    let Some(credential) = token.credential(probed_at.timestamp_millis()) else {
        return ProbeResult {
            token_name: token.name.clone(),
            probed_at,
            quota: None,
            model_usage: None,
            model_usage_error: None,
            rate_limits: RateLimits::default(),
            error: Some(format!(
                "{NO_CREDENTIAL}: run `tokeman login {}`",
                token.name
            )),
        };
    };

    // Subscription OAuth tokens are only admitted to premium models (Fable)
    // for requests that identify as Claude Code; without it the API answers a
    // bare 429 with no quota headers. Haiku happens to be lenient, but every
    // probe identifies the same way so all of them measure the same thing.
    let body = json!({
        "model": model,
        "max_tokens": 1,
        "system": [{"type": "text", "text": CLAUDE_CODE_IDENTITY}],
        "messages": [{"role": "user", "content": "quota"}]
    });

    let mut req = client
        .post("https://api.anthropic.com/v1/messages")
        .header("anthropic-version", "2023-06-01")
        .header(CONTENT_TYPE, HeaderValue::from_static("application/json"));

    // OAuth tokens (sk-ant-oat01-*) use Bearer auth + beta header; API keys use x-api-key
    if credential.value.starts_with("sk-ant-oat01-") {
        req = req
            .header("Authorization", format!("Bearer {}", credential.value))
            .header("anthropic-beta", "claude-code-20250219,oauth-2025-04-20")
            .header("user-agent", client_user_agent());
    } else {
        req = req.header("x-api-key", credential.value);
    }

    let result = req.json(&body).send().await;

    let mut probe = match result {
        Ok(resp) => {
            let headers = resp.headers().clone();
            let status = resp.status();

            // Parse headers regardless of status code — even 429s include rate limit headers
            let quota = parse_unified_quota(&headers);
            let rate_limits = parse_rate_limits(&headers);

            // A 429 still carries the quota headers that explain it; one
            // without them is a refusal like any other status.
            let error = if !status.is_success() && (status.as_u16() != 429 || quota.is_none()) {
                let body_text = resp.text().await.unwrap_or_default();
                Some(format!("HTTP {status}: {body_text}"))
            } else {
                // Consume body to free connection
                let _ = resp.text().await;
                None
            };

            ProbeResult {
                token_name: token.name.clone(),
                probed_at,
                quota,
                model_usage: None,
                model_usage_error: None,
                rate_limits,
                error,
            }
        }
        Err(e) => ProbeResult {
            token_name: token.name.clone(),
            probed_at,
            quota: None,
            model_usage: None,
            model_usage_error: None,
            rate_limits: RateLimits::default(),
            error: Some(e.to_string()),
        },
    };
    let (model_usage, model_usage_error) = probe_model_usage(client, token).await;
    probe.model_usage = model_usage;
    probe.model_usage_error = model_usage_error;
    attach_header_buckets(&mut probe);
    probe
}

/// Record the header form of the Fable limit as an ordinary per-model bucket,
/// unless the profile endpoint already reported one. Rotation, history, charts
/// and every dashboard then treat it like any other model bucket, including on
/// accounts whose credential cannot read the profile endpoint.
pub fn attach_header_buckets(probe: &mut ProbeResult) {
    let Some(window) = probe.quota.as_ref().and_then(|quota| quota.fable.clone()) else {
        return;
    };
    let usage = probe.model_usage.get_or_insert_with(ModelUsage::default);
    if usage
        .scoped_weekly
        .iter()
        .any(|bucket| bucket.key.starts_with("fable"))
    {
        return;
    }
    usage.scoped_weekly.push(ModelQuotaBucket {
        key: "fable".into(),
        label: "Fable".into(),
        window,
        source: ModelUsageSource::Headers,
    });
}

pub async fn probe_all(tokens: &[Token]) -> Vec<ProbeResult> {
    probe_all_with_timeout(tokens, std::time::Duration::from_secs(45)).await
}

pub async fn probe_all_with_timeout(
    tokens: &[Token],
    timeout: std::time::Duration,
) -> Vec<ProbeResult> {
    probe_all_with_model(tokens, timeout, PROBE_MODEL).await
}

/// Probe every token concurrently with `model`.
pub async fn probe_all_with_model(
    tokens: &[Token],
    timeout: std::time::Duration,
    model: &str,
) -> Vec<ProbeResult> {
    let client = reqwest::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(8).min(timeout))
        .timeout(timeout)
        .build()
        .unwrap_or_else(|_| reqwest::Client::new());
    let futures: Vec<_> = tokens
        .iter()
        .map(|token| probe_token(&client, token, model))
        .collect();
    futures::future::join_all(futures).await
}

#[cfg(test)]
mod tests {
    use super::{UsageResponse, UsageWindowResponse, model_key, parse_model_usage, usage_window};

    #[test]
    fn profile_usage_percent_is_normalized_to_fraction() {
        let window = usage_window(UsageWindowResponse {
            utilization: Some(67.5),
            resets_at: Some("2026-07-31T12:34:56Z".into()),
        })
        .expect("valid profile usage window");

        assert!((window.utilization - 0.675).abs() < f64::EPSILON);
        assert_eq!(window.reset, 1_785_501_296);
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

        let opus = usage
            .seven_day_opus
            .and_then(usage_window)
            .expect("Opus bucket");
        assert!((opus.utilization - 0.81).abs() < f64::EPSILON);
        assert!(usage.seven_day_sonnet.is_none());
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

        let usage = parse_model_usage(usage);
        // The surface-scoped row is a meter, but not a model.
        assert_eq!(usage.scoped_weekly.len(), 2);
        assert_eq!(usage.scoped_weekly[0].key, "opus48");
        assert_eq!(usage.scoped_weekly[0].label, "Opus 4.8");
        assert_eq!(usage.scoped_weekly[0].window.utilization, 1.0);
        assert_eq!(usage.scoped_weekly[1].key, "opus5");
        assert_eq!(usage.scoped_weekly[1].window.utilization, 0.2);
    }

    #[test]
    fn idle_null_reset_and_odd_rows_do_not_hide_model_buckets() {
        let usage: UsageResponse = serde_json::from_str(
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
        .expect("a malformed row must not fail the whole response");
        let usage = parse_model_usage(usage);
        assert_eq!(usage.scoped_weekly.len(), 1);
        assert_eq!(usage.scoped_weekly[0].key, "fable");
        assert_eq!(usage.scoped_weekly[0].window.reset, 0);
        assert_eq!(usage.opus_weekly.as_ref().map(|w| w.reset), Some(0));

        let older: UsageResponse = serde_json::from_str(r#"{"limits": null}"#).unwrap();
        assert!(parse_model_usage(older).scoped_weekly.is_empty());
    }

    #[test]
    fn model_keys_agree_between_ids_and_display_names() {
        assert_eq!(model_key("claude-opus-4-8"), model_key("Opus 4.8"));
        assert_eq!(model_key("claude-fable-5-1"), "fable51");
        assert_eq!(model_key("Claude"), "claude");
    }

    #[test]
    fn incomplete_profile_window_stays_unknown() {
        assert!(
            usage_window(UsageWindowResponse {
                utilization: None,
                resets_at: None,
            })
            .is_none()
        );
    }
}
