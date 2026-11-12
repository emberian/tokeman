//! The z.ai GLM Coding Plan: `GET /api/monitor/usage/quota/limit` with the
//! API key as the `Authorization` value.
//!
//! The endpoint answers HTTP 200 for failures too; only `success: true` with
//! `code: 200` is a reading. Each `limits[]` entry is one window: `usage` is
//! its limit (not what was used), `currentValue` what was used, and
//! `unit`/`number` its length. The 5h window opens at the first spend and
//! resets to zero; while it is idle the entry carries no `nextResetTime`.
//! See research/zai-kimi.md.

use serde::Deserialize;

use super::{ServiceAccount, failed};
use crate::quota::{Amount, Limit, Reading, Service, Span, Window};

pub const BASE: &str = "https://api.z.ai";
const QUOTA_PATH: &str = "/api/monitor/usage/quota/limit";

const MINUTE: i64 = 60;
const HOUR: i64 = 3600;
const DAY: i64 = 24 * HOUR;
/// What a `TIME_LIMIT` entry's `unit: 5, number: 1` marker means.
const MONTH: i64 = 30 * DAY;

pub async fn read(client: &reqwest::Client, account: &ServiceAccount, now: i64) -> Reading {
    read_from(client, BASE, account, now).await
}

pub(crate) async fn read_from(
    client: &reqwest::Client,
    base: &str,
    account: &ServiceAccount,
    now: i64,
) -> Reading {
    let failed = |error: String| failed(Service::Zai, &account.name, now, error);
    let key = match account.api_key() {
        Ok(key) => key,
        Err(error) => return failed(format!("{error:#}")),
    };
    let response = match client
        .get(format!("{}{QUOTA_PATH}", base.trim_end_matches('/')))
        .header(reqwest::header::AUTHORIZATION, key)
        .header(reqwest::header::ACCEPT_LANGUAGE, "en-US,en")
        .send()
        .await
    {
        Ok(response) => response,
        Err(error) => return failed(format!("request failed: {error}")),
    };
    let status = response.status();
    let body = match response.text().await {
        Ok(body) => body,
        Err(error) => return failed(format!("reading the response failed: {error}")),
    };
    if !status.is_success() {
        return failed(format!(
            "HTTP {}: {}",
            status.as_u16(),
            super::snippet(&body)
        ));
    }
    match parse(&body, now) {
        Ok((plan, limits)) => Reading {
            plan,
            limits,
            ..Reading::empty(Service::Zai, &account.name, now)
        },
        Err(error) => failed(error),
    }
}

#[derive(Deserialize)]
struct Envelope {
    #[serde(default)]
    code: Option<i64>,
    #[serde(default)]
    msg: Option<String>,
    #[serde(default)]
    success: bool,
    #[serde(default)]
    data: Option<Quota>,
}

#[derive(Deserialize)]
struct Quota {
    #[serde(default)]
    limits: Vec<RawLimit>,
    #[serde(default)]
    level: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
enum Kind {
    /// Credit plans: model and MCP use both cost credits.
    #[serde(rename = "CREDIT_LIMIT")]
    Credit,
    /// Older token plans' 5h window.
    #[serde(rename = "TOKENS_LIMIT")]
    Tokens,
    /// Older plans' monthly MCP tool-call quota.
    #[serde(rename = "TIME_LIMIT")]
    Time,
    #[serde(other)]
    Other,
}

#[derive(Deserialize)]
struct RawLimit {
    #[serde(rename = "type")]
    kind: Kind,
    #[serde(default)]
    unit: Option<i64>,
    #[serde(default)]
    number: Option<i64>,
    /// The window's limit.
    #[serde(default)]
    usage: Option<f64>,
    #[serde(default, rename = "currentValue")]
    current: Option<f64>,
    #[serde(default)]
    remaining: Option<f64>,
    /// Whole percent used, rounded up.
    #[serde(default)]
    percentage: Option<f64>,
    /// Epoch milliseconds; absent while the window is idle.
    #[serde(default, rename = "nextResetTime")]
    next_reset_ms: Option<i64>,
}

impl RawLimit {
    fn span_secs(&self) -> Option<i64> {
        let number = self.number.filter(|&n| n > 0)?;
        if self.kind == Kind::Time && self.unit == Some(5) && number == 1 {
            return Some(MONTH);
        }
        let unit = match self.unit? {
            1 => DAY,
            3 => HOUR,
            5 => MINUTE,
            6 => 7 * DAY,
            _ => return None,
        };
        Some(number * unit)
    }

    fn meter(&self, now: i64) -> Option<Limit> {
        let span_secs = self.span_secs();
        let limit = self.usage.filter(|&limit| limit > 0.0);
        let used_amount = limit.and_then(|limit| {
            self.current
                .or_else(|| self.remaining.map(|remaining| limit - remaining))
        });
        let used = match (used_amount, limit) {
            (Some(used), Some(limit)) => used.max(0.0) / limit,
            _ => self.percentage? / 100.0,
        };
        // A reset further away than the window is long is not this window's.
        let resets_at = self
            .next_reset_ms
            .map(|ms| ms / 1000)
            .filter(|&at| span_secs.is_none_or(|span| at <= now + span + MINUTE))
            .unwrap_or(0);
        let (group, unit) = match self.kind {
            Kind::Credit => (None, "credits"),
            Kind::Tokens => (None, "tokens"),
            Kind::Time => (Some("MCP tools".to_owned()), "calls"),
            Kind::Other => return None,
        };
        Some(super::limit(
            group,
            Span::from_seconds(span_secs).label(),
            span_secs,
            Window::new(used, resets_at),
            used_amount.zip(limit).map(|(used, limit)| Amount {
                used,
                limit,
                unit: unit.into(),
            }),
        ))
    }
}

/// The plan level and one meter per known limit, shortest window first.
fn parse(body: &str, now: i64) -> Result<(Option<String>, Vec<Limit>), String> {
    let envelope: Envelope = serde_json::from_str(body)
        .map_err(|error| format!("unreadable answer ({error}): {}", super::snippet(body)))?;
    if !envelope.success || envelope.code != Some(200) {
        return Err(format!(
            "z.ai refused: {} (code {})",
            envelope.msg.as_deref().unwrap_or("no message"),
            envelope
                .code
                .map_or_else(|| "none".to_owned(), |code| code.to_string())
        ));
    }
    let quota = envelope.data.ok_or("the answer has no data")?;
    let mut meters: Vec<Limit> = quota
        .limits
        .iter()
        .filter_map(|limit| limit.meter(now))
        .collect();
    if meters.is_empty() {
        return Err("the answer lists no limits tokeman knows".into());
    }
    meters.sort_by_key(|meter| {
        (
            meter.group().is_some(),
            meter.span.seconds().unwrap_or(i64::MAX),
        )
    });
    Ok((quota.level.filter(|level| !level.is_empty()), meters))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{Router, http::HeaderMap, routing::get};

    /// 2026-11-11 20:31 UTC, when the bodies below were captured.
    const NOW: i64 = 1_794_429_064;

    const LIVE: &str = r#"{"code":200,"msg":"Operation successful","data":{"limits":[{"type":"CREDIT_LIMIT","unit":3,"number":5,"usage":28000,"currentValue":177,"remaining":27822,"percentage":1,"nextResetTime":1794429533576},{"type":"CREDIT_LIMIT","unit":6,"number":1,"usage":140000,"currentValue":1849,"remaining":138150,"percentage":1,"nextResetTime":1794879119966}],"level":"max"},"success":true}"#;
    /// Eight minutes later, after the 5h window closed.
    const IDLE: &str = r#"{"code":200,"msg":"Operation successful","data":{"limits":[{"type":"CREDIT_LIMIT","unit":3,"number":5,"usage":28000,"currentValue":0,"remaining":28000,"percentage":0},{"type":"CREDIT_LIMIT","unit":6,"number":1,"usage":140000,"currentValue":1849,"remaining":138150,"percentage":1,"nextResetTime":1794879119966}],"level":"max"},"success":true}"#;

    fn account() -> ServiceAccount {
        ServiceAccount {
            key: Some("zai-test-key".into()),
            ..ServiceAccount::with_key_file(Service::Zai, "glm", "unused")
        }
    }

    #[test]
    fn credit_windows_read_used_over_limit_with_their_resets() {
        let (plan, meters) = parse(LIVE, NOW).unwrap();
        assert_eq!(plan.as_deref(), Some("max"));
        assert_eq!(meters.len(), 2);
        let session = &meters[0];
        assert_eq!(session.label, "5h");
        assert_eq!(session.span, Span::FiveHour);
        assert_eq!(session.scope, crate::quota::Scope::All);
        assert_eq!(session.source, crate::quota::Source::Poll);
        // 177 of 28000, not the rounded-up `percentage: 1`.
        assert!((session.window.used - 177.0 / 28000.0).abs() < 1e-12);
        assert_eq!(session.window.resets_at, 1_794_429_533);
        assert_eq!(
            session.amount,
            Some(Amount {
                used: 177.0,
                limit: 28000.0,
                unit: "credits".into()
            })
        );
        let weekly = &meters[1];
        assert_eq!(weekly.label, "weekly");
        assert_eq!(weekly.window.resets_at, 1_794_879_119);
        assert!((weekly.window.used - 1849.0 / 140000.0).abs() < 1e-12);
    }

    #[test]
    fn an_idle_window_has_no_reset_pending() {
        let (_, meters) = parse(IDLE, NOW + 500).unwrap();
        assert_eq!(meters[0].window, Window::new(0.0, 0));
    }

    #[test]
    fn a_reset_beyond_the_window_length_is_dropped() {
        let far = LIVE.replace("1794429533576", &((NOW + 10 * HOUR) * 1000).to_string());
        let (_, meters) = parse(&far, NOW).unwrap();
        assert_eq!(meters[0].window.resets_at, 0);
        assert_eq!(meters[1].window.resets_at, 1_794_879_119);
    }

    #[test]
    fn legacy_token_and_mcp_limits_are_kept_apart() {
        let body = r#"{"code":200,"success":true,"data":{"limits":[
            {"type":"TIME_LIMIT","unit":5,"number":1,"usage":1000,"currentValue":250,"remaining":750,"percentage":25,"nextResetTime":1796000000000},
            {"type":"TOKENS_LIMIT","unit":3,"number":5,"percentage":40,"nextResetTime":1794430000000},
            {"type":"SOMETHING_NEW","unit":3,"number":5,"percentage":99}]}}"#;
        let (plan, meters) = parse(body, NOW).unwrap();
        assert_eq!(plan, None);
        assert_eq!(meters.len(), 2, "an unknown limit type is skipped");
        assert_eq!(meters[0].label, "5h");
        assert_eq!(meters[0].group(), None);
        assert!((meters[0].window.used - 0.40).abs() < 1e-12);
        assert_eq!(meters[0].amount, None);
        assert_eq!(meters[1].group(), Some("MCP tools"));
        assert_eq!(meters[1].span, Span::Monthly);
        assert_eq!(meters[1].label, "monthly");
        assert_eq!(meters[1].window, Window::new(0.25, 1_796_000_000));
        assert_eq!(meters[1].amount.as_ref().unwrap().unit, "calls");
    }

    #[test]
    fn a_failure_reported_with_http_200_is_an_error() {
        let error = parse(
            r#"{"code":401,"msg":"token expired or incorrect","success":false}"#,
            NOW,
        )
        .unwrap_err();
        assert!(error.contains("token expired or incorrect"), "{error}");
        assert!(error.contains("401"), "{error}");
        assert!(parse("<html>", NOW).is_err());
    }

    #[tokio::test]
    async fn read_sends_the_raw_key_and_reports_the_body_error() {
        let app = Router::new().route(
            QUOTA_PATH,
            get(|headers: HeaderMap| async move {
                match headers.get("authorization").and_then(|v| v.to_str().ok()) {
                    Some("zai-test-key") => LIVE.to_owned(),
                    _ => r#"{"code":1001,"msg":"Authentication parameter not received in Header","success":false}"#.to_owned(),
                }
            }),
        );
        let base = crate::testing::serve(app).await;
        let client = reqwest::Client::new();
        let usage = read_from(&client, &base, &account(), NOW).await;
        assert_eq!(usage.error, None);
        assert_eq!(usage.service, Service::Zai);
        assert_eq!(usage.account, "glm");
        assert_eq!(usage.at, NOW);
        assert_eq!(usage.plan.as_deref(), Some("max"));
        assert_eq!(usage.limits.len(), 2);

        let wrong = ServiceAccount {
            key: Some("other".into()),
            ..account()
        };
        let usage = read_from(&client, &base, &wrong, NOW).await;
        assert!(usage.limits.is_empty());
        assert!(usage.error.unwrap().to_string().contains("code 1001"));
    }

    #[tokio::test]
    async fn a_missing_key_file_fails_without_a_request() {
        let missing = ServiceAccount::with_key_file(Service::Zai, "glm", "/nonexistent/zai-key");
        let usage = read_from(&reqwest::Client::new(), "http://127.0.0.1:9", &missing, NOW).await;
        assert!(
            usage
                .error
                .unwrap()
                .to_string()
                .contains("/nonexistent/zai-key")
        );
    }
}
