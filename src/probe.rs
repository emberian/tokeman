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

#[derive(Debug, Clone, Serialize)]
#[allow(dead_code)] // all fields populated from API response headers
pub struct UnifiedQuota {
    /// "allowed", "allowed_warning", or "rejected"
    pub status: String,
    pub reset: i64,
    /// Which claim is authoritative: "five_hour", "seven_day", "seven_day_opus", "seven_day_sonnet", "overage"
    pub representative_claim: String,
    /// "available" if fallback exists
    pub fallback: Option<String>,
    /// Per-window utilization
    pub session: Option<Window>, // 5h
    pub weekly: Option<Window>, // 7d
    /// Overage / extra usage
    pub overage_status: Option<String>,
    pub overage: Option<Window>,
    pub overage_disabled_reason: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
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
}

impl ModelUsageSource {
    pub fn label(self) -> &'static str {
        match self {
            Self::Profile => "profile",
            Self::ObservedRejection => "observed Claude rejection",
        }
    }

    pub fn short_label(self) -> &'static str {
        match self {
            Self::Profile => "profile",
            Self::ObservedRejection => "observed!",
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

#[derive(Debug, Deserialize)]
struct UsageWindowResponse {
    utilization: Option<f64>,
    resets_at: Option<String>,
}

#[derive(Debug, Deserialize)]
struct UsageResponse {
    seven_day_opus: Option<UsageWindowResponse>,
    seven_day_sonnet: Option<UsageWindowResponse>,
    #[serde(default)]
    limits: Vec<UsageLimitResponse>,
}

#[derive(Debug, Deserialize)]
struct UsageLimitResponse {
    kind: Option<String>,
    group: Option<String>,
    percent: Option<f64>,
    resets_at: Option<String>,
    is_active: Option<bool>,
    scope: Option<UsageScopeResponse>,
}

#[derive(Debug, Deserialize)]
struct UsageScopeResponse {
    model: Option<UsageModelResponse>,
    surface: Option<String>,
}

#[derive(Debug, Deserialize)]
struct UsageModelResponse {
    display_name: Option<String>,
    id: Option<String>,
    name: Option<String>,
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
        overage_status: header_str(headers, "anthropic-ratelimit-unified-overage-status"),
        overage: parse_window(headers, "anthropic-ratelimit-unified-overage"),
        overage_disabled_reason: header_str(
            headers,
            "anthropic-ratelimit-unified-overage-disabled-reason",
        ),
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
    let utilization = window.utilization?;
    let reset = DateTime::parse_from_rfc3339(window.resets_at.as_deref()?)
        .ok()?
        .timestamp();
    Some(Window {
        // The profile endpoint reports percentages; message headers report a
        // 0..1 fraction. Normalize once at the boundary.
        utilization: (utilization / 100.0).clamp(0.0, 1.0),
        reset,
    })
}

pub(crate) fn normalized_bucket_key(value: &str) -> String {
    value
        .chars()
        .filter(|ch| ch.is_ascii_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect()
}

fn dynamic_usage_bucket(limit: UsageLimitResponse) -> Option<ModelQuotaBucket> {
    if limit.kind.as_deref() != Some("weekly_scoped")
        && !(limit.group.as_deref() == Some("weekly") && limit.scope.is_some())
    {
        return None;
    }
    let percent = limit.percent?;
    if percent == 0.0 && limit.is_active == Some(false) {
        return None;
    }
    let reset = DateTime::parse_from_rfc3339(limit.resets_at.as_deref()?)
        .ok()?
        .timestamp();
    let scope = limit.scope?;
    let model = scope.model;
    let label = model
        .as_ref()
        .and_then(|model| model.display_name.clone())
        .or_else(|| scope.surface.clone())
        .or_else(|| model.as_ref().and_then(|model| model.name.clone()))
        .or_else(|| model.as_ref().and_then(|model| model.id.clone()))
        .unwrap_or_else(|| "Scoped model".into());
    let identity = model
        .as_ref()
        .and_then(|model| model.id.as_deref().or(model.name.as_deref()))
        .unwrap_or(&label);
    let key = normalized_bucket_key(identity);
    (!key.is_empty()).then_some(ModelQuotaBucket {
        key,
        label,
        window: Window {
            utilization: (percent / 100.0).clamp(0.0, 1.0),
            reset,
        },
        source: ModelUsageSource::Profile,
    })
}

fn parse_model_usage(usage: UsageResponse) -> ModelUsage {
    let opus_weekly = usage.seven_day_opus.and_then(usage_window);
    let sonnet_weekly = usage.seven_day_sonnet.and_then(usage_window);
    let mut scoped_weekly = usage
        .limits
        .into_iter()
        .filter_map(dynamic_usage_bucket)
        .collect::<Vec<_>>();
    scoped_weekly.sort_by(|a, b| a.label.cmp(&b.label).then_with(|| a.key.cmp(&b.key)));
    scoped_weekly.dedup_by(|a, b| a.key == b.key);
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
    let Some(usage_key) = token.usage_key.as_deref() else {
        return (None, None);
    };
    let response = client
        .get("https://api.anthropic.com/api/oauth/usage")
        .header("Authorization", format!("Bearer {usage_key}"))
        .header("anthropic-beta", "oauth-2025-04-20")
        .header("user-agent", "claude-code/2.1.220")
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
        key: String::new(),
        usage_key: Some(usage_key.into()),
    };
    let (usage, error) = probe_model_usage(&client, &token).await;
    usage.ok_or_else(|| error.unwrap_or_else(|| "usage data unavailable".into()))
}

pub async fn probe_token(client: &reqwest::Client, token: &Token) -> ProbeResult {
    let probed_at = Utc::now();

    let body = json!({
        "model": PROBE_MODEL,
        "max_tokens": 1,
        "messages": [{"role": "user", "content": "quota"}]
    });

    let mut req = client
        .post("https://api.anthropic.com/v1/messages")
        .header("anthropic-version", "2023-06-01")
        .header(CONTENT_TYPE, HeaderValue::from_static("application/json"));

    // OAuth tokens (sk-ant-oat01-*) use Bearer auth + beta header; API keys use x-api-key
    if token.key.starts_with("sk-ant-oat01-") {
        req = req
            .header("Authorization", format!("Bearer {}", token.key))
            .header("anthropic-beta", "oauth-2025-04-20");
    } else {
        req = req.header("x-api-key", &token.key);
    }

    let result = req.json(&body).send().await;

    let mut probe = match result {
        Ok(resp) => {
            let headers = resp.headers().clone();
            let status = resp.status();

            // Parse headers regardless of status code — even 429s include rate limit headers
            let quota = parse_unified_quota(&headers);
            let rate_limits = parse_rate_limits(&headers);

            let error = if !status.is_success() && status.as_u16() != 429 {
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
            rate_limits: RateLimits {
                requests_limit: None,
                requests_remaining: None,
                input_tokens_limit: None,
                input_tokens_remaining: None,
                output_tokens_limit: None,
                output_tokens_remaining: None,
            },
            error: Some(e.to_string()),
        },
    };
    let (model_usage, model_usage_error) = probe_model_usage(client, token).await;
    probe.model_usage = model_usage;
    probe.model_usage_error = model_usage_error;
    probe
}

pub async fn probe_all(tokens: &[Token]) -> Vec<ProbeResult> {
    probe_all_with_timeout(tokens, std::time::Duration::from_secs(45)).await
}

pub async fn probe_all_with_timeout(
    tokens: &[Token],
    timeout: std::time::Duration,
) -> Vec<ProbeResult> {
    let client = reqwest::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(8).min(timeout))
        .timeout(timeout)
        .build()
        .unwrap_or_else(|_| reqwest::Client::new());
    let futures: Vec<_> = tokens
        .iter()
        .map(|token| probe_token(&client, token))
        .collect();
    futures::future::join_all(futures).await
}

#[cfg(test)]
mod tests {
    use super::{UsageResponse, UsageWindowResponse, parse_model_usage, usage_window};

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
                        "scope": {"surface": "dormant"}
                    }
                ]
            }"#,
        )
        .expect("dynamic usage response");

        let usage = parse_model_usage(usage);
        assert_eq!(usage.scoped_weekly.len(), 2);
        assert_eq!(usage.scoped_weekly[0].key, "claudeopus48");
        assert_eq!(usage.scoped_weekly[0].label, "Opus 4.8");
        assert_eq!(usage.scoped_weekly[0].window.utilization, 1.0);
        assert_eq!(usage.scoped_weekly[1].key, "claudeopus5");
        assert_eq!(usage.scoped_weekly[1].window.utilization, 0.2);
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
