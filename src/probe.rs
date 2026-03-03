use chrono::{DateTime, Utc};
use reqwest::header::{HeaderMap, HeaderValue, CONTENT_TYPE};
use serde::Serialize;
use serde_json::json;

use crate::config::Token;

/// Utilization data for a single rate limit window (5h, 7d, or overage).
#[derive(Debug, Clone, Serialize)]
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
    pub session: Option<Window>,      // 5h
    pub weekly: Option<Window>,       // 7d
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
    pub rate_limits: RateLimits,
    pub error: Option<String>,
}

fn header_str(headers: &HeaderMap, key: &str) -> Option<String> {
    headers.get(key).and_then(|v| v.to_str().ok()).map(|s| s.to_string())
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
        representative_claim: header_str(headers, "anthropic-ratelimit-unified-representative-claim")
            .unwrap_or_default(),
        fallback: header_str(headers, "anthropic-ratelimit-unified-fallback"),
        session: parse_window(headers, "anthropic-ratelimit-unified-5h"),
        weekly: parse_window(headers, "anthropic-ratelimit-unified-7d"),
        overage_status: header_str(headers, "anthropic-ratelimit-unified-overage-status"),
        overage: parse_window(headers, "anthropic-ratelimit-unified-overage"),
        overage_disabled_reason: header_str(headers, "anthropic-ratelimit-unified-overage-disabled-reason"),
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

pub async fn probe_token(client: &reqwest::Client, token: &Token) -> ProbeResult {
    let probed_at = Utc::now();

    let body = json!({
        "model": "claude-haiku-4-5-20251001",
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

    match result {
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
                rate_limits,
                error,
            }
        }
        Err(e) => ProbeResult {
            token_name: token.name.clone(),
            probed_at,
            quota: None,
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
    }
}

pub async fn probe_all(tokens: &[Token]) -> Vec<ProbeResult> {
    let client = reqwest::Client::new();
    let futures: Vec<_> = tokens
        .iter()
        .map(|token| probe_token(&client, token))
        .collect();
    futures::future::join_all(futures).await
}
