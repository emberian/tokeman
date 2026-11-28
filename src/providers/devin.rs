//! Devin (Cognition): `POST <api server>/exa.seat_management_pb.SeatManagementService/GetUserStatus`,
//! Connect JSON, with the Devin CLI's own key in the body (`metadata.apiKey`).
//!
//! The key and the API server come from the file the CLI keeps its login in,
//! `$XDG_DATA_HOME/devin/credentials.toml` (`windsurf_api_key`,
//! `api_server_url`): a static key, read on every poll and never written.
//! `userStatus.planStatus` holds the quota as percentages *remaining* and Unix
//! reset times, one pair per window (`daily...`, `weekly...`), and the
//! extra-usage balance in millionths of a dollar. The body is proto3 JSON, so
//! a zero is omitted: a window whose reset is present and whose percentage is
//! not has nothing left. A plan with `hideDailyQuota` shows no daily window in
//! Devin's own clients and none is read here. See research/devin-grok.md.

use std::path::PathBuf;

use serde::Deserialize;
use serde_json::{Value, json};

use super::{ServiceAccount, credential, decode, limit, network};
use crate::drift::Unrecognized;
use crate::paths;
use crate::quota::{Balance, Limit, ReadError, Reading, Service, Span, Window};
use crate::util::one_line;

/// The API server of a login whose file names none.
pub const BASE: &str = "https://server.codeium.com";

const GET_USER_STATUS: &str = "/exa.seat_management_pb.SeatManagementService/GetUserStatus";

/// How the request names its client. The server refuses a body without these
/// (HTTP 400 `invalid_argument`); this is the CLI the shape was read from.
const CLIENT_NAME: &str = "devin-cli";
const CLIENT_VERSION: &str = "3000.15.14";

const MICROS_PER_CENT: i64 = 10_000;

/// Where the Devin CLI keeps its login: `$XDG_DATA_HOME/devin/credentials.toml`,
/// `~/.local/share/devin/credentials.toml` without the variable (macOS too).
pub fn credentials_file() -> PathBuf {
    std::env::var_os("XDG_DATA_HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .unwrap_or_else(|| paths::home().join(".local/share"))
        .join("devin/credentials.toml")
}

/// What is read of the CLI's credentials file.
#[derive(Deserialize)]
struct Credentials {
    #[serde(default)]
    windsurf_api_key: Option<String>,
    #[serde(default)]
    api_server_url: Option<String>,
}

/// The key and the API server of `account`: an inline key on [`BASE`], else
/// the CLI's credentials file the account names.
fn login(account: &ServiceAccount) -> Result<(String, String), ReadError> {
    if account
        .key
        .as_deref()
        .is_some_and(|key| !key.trim().is_empty())
    {
        let key = account
            .api_key()
            .map_err(|error| credential(format!("{error:#}")))?;
        return Ok((key, BASE.to_owned()));
    }
    let Some(file) = account.key_file.as_deref() else {
        return Err(ReadError::NoCredential);
    };
    let path = paths::expand_tilde(file);
    let text = std::fs::read_to_string(&path).map_err(|error| {
        credential(format!(
            "cannot read Devin's login file ({error}): run `devin auth login` ({})",
            paths::tilde(&path)
        ))
    })?;
    parse_credentials(&text).map_err(|why| {
        credential(format!(
            "Devin's login file {why}: run `devin auth login` ({})",
            paths::tilde(&path)
        ))
    })
}

/// The key and API server in the text of a credentials file; what is wrong
/// with it otherwise, never its content.
fn parse_credentials(text: &str) -> Result<(String, String), &'static str> {
    let parsed: Credentials = toml::from_str(text).map_err(|_| "is not readable TOML")?;
    let key = parsed
        .windsurf_api_key
        .map(|key| key.trim().to_owned())
        .filter(|key| !key.is_empty())
        .ok_or("holds no key (`windsurf_api_key`)")?;
    if key.chars().any(char::is_control) {
        return Err("holds a key with control characters");
    }
    let base = parsed
        .api_server_url
        .map(|url| url.trim().trim_end_matches('/').to_owned())
        .filter(|url| !url.is_empty())
        .unwrap_or_else(|| BASE.to_owned());
    // The key goes in the request body: it is sent to an https origin only.
    if !base.starts_with("https://") {
        return Err("names an API server that is not https");
    }
    Ok((key, base))
}

pub async fn read(client: &reqwest::Client, account: &ServiceAccount, now: i64) -> Reading {
    read_from(client, None, account, now).await
}

/// [`read`], with the API server replaced by `base` when given (tests).
pub(crate) async fn read_from(
    client: &reqwest::Client,
    base: Option<&str>,
    account: &ServiceAccount,
    now: i64,
) -> Reading {
    let failed = |error: ReadError| Reading::failed(Service::Devin, &account.name, now, error);
    let (key, file_base) = match login(account) {
        Ok(login) => login,
        Err(error) => return failed(error),
    };
    let base = base.unwrap_or(&file_base).trim_end_matches('/');
    match user_status(client, base, &key)
        .await
        .and_then(|body| parse_user_status(&body))
    {
        Ok(status) => Reading {
            plan: status.plan,
            limits: status.limits,
            balances: status.balances,
            unrecognized: status.unrecognized,
            ..Reading::empty(Service::Devin, &account.name, now)
        },
        Err(error) => failed(error),
    }
}

/// The email the login at `account`'s credentials signs in as: what
/// `tokeman devin import` names the account.
pub async fn email(
    client: &reqwest::Client,
    account: &ServiceAccount,
) -> Result<String, ReadError> {
    let (key, base) = login(account)?;
    let body = user_status(client, &base, &key).await?;
    let value: Value = serde_json::from_str(&body)
        .map_err(|error| decode(format!("unreadable answer ({error})")))?;
    value["userStatus"]["email"]
        .as_str()
        .map(str::trim)
        .filter(|email| !email.is_empty())
        .map(str::to_owned)
        .ok_or_else(|| decode("the answer names no email"))
}

async fn user_status(client: &reqwest::Client, base: &str, key: &str) -> Result<String, ReadError> {
    let response = client
        .post(format!("{base}{GET_USER_STATUS}"))
        .header(reqwest::header::ACCEPT, "application/json")
        .header("Connect-Protocol-Version", "1")
        .json(&json!({ "metadata": {
            "apiKey": key,
            "ideName": CLIENT_NAME,
            "ideVersion": CLIENT_VERSION,
            "extensionName": CLIENT_NAME,
            "extensionVersion": CLIENT_VERSION,
        }}))
        .send()
        .await
        .map_err(|error| network(format!("request failed: {}", error.without_url())))?;
    let status = response.status();
    let body = response
        .text()
        .await
        .map_err(|error| network(format!("reading the response failed: {error}")))?;
    if status.is_success() {
        return Ok(body);
    }
    // A Connect error: `{"code":"unauthenticated","message":"..."}`.
    let error = serde_json::from_str::<Value>(&body).ok();
    let field = |name: &str| {
        error
            .as_ref()
            .and_then(|value| value[name].as_str())
            .map(str::to_owned)
    };
    let message = field("message").unwrap_or_else(|| one_line(&body, 160));
    Err(ReadError::Http {
        status: status.as_u16(),
        body: match (status.as_u16(), field("code").as_deref()) {
            (401 | 403, _) | (_, Some("unauthenticated" | "permission_denied")) => {
                format!("Devin refused the key: {message}; run `devin auth login`")
            }
            _ => message,
        },
    })
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Body {
    #[serde(default)]
    user_status: Option<UserStatus>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct UserStatus {
    #[serde(default)]
    plan_status: Option<PlanStatus>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct PlanStatus {
    #[serde(default)]
    plan_info: Option<PlanInfo>,
    /// Percent of the window left, 0-100. Omitted at zero.
    #[serde(default)]
    daily_quota_remaining_percent: Option<Value>,
    #[serde(default)]
    weekly_quota_remaining_percent: Option<Value>,
    /// Unix seconds, as a string (a proto int64).
    #[serde(default)]
    daily_quota_reset_at_unix: Option<Value>,
    #[serde(default)]
    weekly_quota_reset_at_unix: Option<Value>,
    /// Millionths of a dollar, as a string; negative when extra usage is owed.
    #[serde(default)]
    overage_balance_micros: Option<Value>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct PlanInfo {
    #[serde(default)]
    plan_name: Option<String>,
    #[serde(default)]
    hide_daily_quota: Option<bool>,
}

/// A number, or a number in a string.
fn number(value: Option<&Value>) -> Option<f64> {
    match value? {
        Value::Number(number) => number.as_f64(),
        Value::String(text) => text.trim().parse().ok(),
        _ => None,
    }
    .filter(|n: &f64| n.is_finite())
}

/// One window: its percentage left and its reset. With a reset and no
/// percentage the window is spent (proto3 JSON omits a zero); with neither
/// the plan has no such window.
fn window(span_secs: i64, remaining: Option<&Value>, reset: Option<&Value>) -> Option<Limit> {
    let reset = number(reset).filter(|&at| at > 0.0).map(|at| at as i64);
    let remaining = match (number(remaining), reset) {
        (Some(percent), _) => percent,
        (None, Some(_)) => 0.0,
        (None, None) => return None,
    };
    let span = Span::from_seconds(Some(span_secs));
    Some(limit(
        None,
        span.label(),
        Some(span_secs),
        Window::new(
            ((100.0 - remaining) / 100.0).clamp(0.0, 1.0),
            reset.unwrap_or(0),
        ),
        None,
    ))
}

// The known sets of `userStatus.planStatus`: what this parser reads, then
// what has been seen and is not read (research/devin-grok.md). The rest of
// the answer describes the person, the team and the plan's feature switches
// and is not looked at.

const KNOWN_PLAN_STATUS_KEYS: &[&str] = &[
    "planInfo",
    "dailyQuotaRemainingPercent",
    "weeklyQuotaRemainingPercent",
    "dailyQuotaResetAtUnix",
    "weeklyQuotaResetAtUnix",
    "overageBalanceMicros",
    // Live 2026-11-28: the billing cycle, and -1 on a quota plan.
    "planStart",
    "planEnd",
    "availablePromptCredits",
    // In the devin 3000.15.14 binary's `PlanStatus` and not sent to a quota
    // plan: the credit plans' counters and the ACU plans' meter.
    "availableFlexCredits",
    "availableFlowCredits",
    "usedPromptCredits",
    "usedFlowCredits",
    "usedFlexCredits",
    "topUpStatus",
    "wasReducedByOrphanedUsage",
    "gracePeriodStatus",
    "acuConsumed",
    "acuLimit",
    "dollarsLimit",
];
/// `planInfo.billingStrategy`: the one seen live, and the credit plans' from
/// other clients' notes.
const KNOWN_BILLING_STRATEGIES: &[&str] = &["BILLING_STRATEGY_QUOTA", "BILLING_STRATEGY_CREDITS"];

/// What `GetUserStatus` holds for tokeman.
#[derive(Debug)]
struct Status {
    plan: Option<String>,
    limits: Vec<Limit>,
    balances: Vec<Balance>,
    unrecognized: Vec<String>,
}

fn parse_user_status(body: &str) -> Result<Status, ReadError> {
    let unreadable = |error: serde_json::Error| {
        // The answer names the person: say how it failed, not what it held.
        decode(format!("unreadable answer ({error})"))
    };
    let value: Value = serde_json::from_str(body).map_err(unreadable)?;
    let plan_status = &value["userStatus"]["planStatus"];
    if !plan_status.is_object() {
        return Err(decode("the answer has no plan status"));
    }
    let mut seen = Unrecognized::new();
    seen.keys("userStatus.planStatus", plan_status, KNOWN_PLAN_STATUS_KEYS);
    seen.value(
        "userStatus.planStatus.planInfo.billingStrategy",
        plan_status["planInfo"].get("billingStrategy"),
        KNOWN_BILLING_STRATEGIES,
    );
    let parsed: Body = serde_json::from_value(value).map_err(unreadable)?;
    let Some(status) = parsed.user_status.and_then(|user| user.plan_status) else {
        return Err(decode("the answer has no plan status"));
    };
    let info = status.plan_info.as_ref();
    let hide_daily = info.and_then(|info| info.hide_daily_quota).unwrap_or(false);
    let daily = (!hide_daily)
        .then(|| {
            window(
                Span::DAY_SECS,
                status.daily_quota_remaining_percent.as_ref(),
                status.daily_quota_reset_at_unix.as_ref(),
            )
        })
        .flatten();
    let weekly = window(
        Span::WEEK_SECS,
        status.weekly_quota_remaining_percent.as_ref(),
        status.weekly_quota_reset_at_unix.as_ref(),
    );
    let balances = number(status.overage_balance_micros.as_ref())
        .map(|micros| Balance {
            label: "extra usage balance".into(),
            left_minor: (micros / MICROS_PER_CENT as f64).round() as i64,
            total_minor: None,
            currency: "USD".into(),
        })
        .into_iter()
        .collect();
    Ok(Status {
        plan: info
            .and_then(|info| info.plan_name.as_deref())
            .map(str::trim)
            .filter(|name| !name.is_empty())
            .map(str::to_owned),
        // No window is not an error: a plan without quota (credits, ACUs)
        // still has its name and its balance. The row says what is missing.
        limits: daily.into_iter().chain(weekly).collect(),
        balances,
        unrecognized: seen.into_vec(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{Json, Router, http::StatusCode, routing::post};

    /// `GetUserStatus` for a Devin Max login, 2026-11-28 01:30 UTC, as the
    /// server sent it; the name, email and ids replaced with `redacted`.
    const LIVE: &str = include_str!("fixtures/devin-user-status.json");

    fn account() -> ServiceAccount {
        ServiceAccount {
            key: Some("devin-test-key".into()),
            ..ServiceAccount::with_key_file(Service::Devin, "devin", "unused")
        }
    }

    #[test]
    fn devin_recorded_status_becomes_the_weekly_window_the_plan_and_the_balance() {
        let status = parse_user_status(LIVE).unwrap();
        assert!(status.unrecognized.is_empty(), "{:?}", status.unrecognized);
        assert_eq!(status.plan.as_deref(), Some("Max"));
        // 98% left is 2% used; the daily window is hidden on this plan.
        assert_eq!(status.limits.len(), 1, "{:?}", status.limits);
        let weekly = &status.limits[0];
        assert_eq!(weekly.span, Span::Weekly);
        assert!(weekly.is_shared());
        assert!((weekly.window.used - 0.02).abs() < 1e-9);
        assert_eq!(weekly.window.resets_at, 1_795_939_200);
        // -5,258,321 millionths of a dollar: $5.26 owed.
        assert_eq!(
            status.balances,
            [Balance {
                label: "extra usage balance".into(),
                left_minor: -526,
                total_minor: None,
                currency: "USD".into(),
            }]
        );
    }

    #[test]
    fn devin_window_without_its_percentage_is_spent_and_a_shown_daily_window_is_read() {
        // Proto3 JSON omits a zero: the weekly reset stands alone when the
        // week is spent (robinebers/openusage#1237, Devin Max, 2026-09-08).
        let body = r#"{"userStatus":{"planStatus":{
            "planInfo":{"planName":"Pro","billingStrategy":"BILLING_STRATEGY_QUOTA"},
            "dailyQuotaRemainingPercent":50,"dailyQuotaResetAtUnix":"1788940800",
            "weeklyQuotaResetAtUnix":"1789286400"}}}"#;
        let status = parse_user_status(body).unwrap();
        let summary: Vec<_> = status
            .limits
            .iter()
            .map(|limit| (limit.span, limit.window.used, limit.window.resets_at))
            .collect();
        assert_eq!(
            summary,
            [
                (Span::Other(86_400), 0.5, 1_788_940_800),
                (Span::Weekly, 1.0, 1_789_286_400)
            ]
        );
        assert!(status.balances.is_empty());

        // Hidden means not read, whatever it holds: a hidden daily window
        // with only a reset would otherwise read as spent.
        let hidden = r#"{"userStatus":{"planStatus":{
            "planInfo":{"planName":"Max","hideDailyQuota":true},
            "dailyQuotaResetAtUnix":"1788940800",
            "weeklyQuotaRemainingPercent":7,"weeklyQuotaResetAtUnix":"1789286400"}}}"#;
        let status = parse_user_status(hidden).unwrap();
        assert_eq!(status.limits.len(), 1);
        assert!((status.limits[0].window.used - 0.93).abs() < 1e-9);
    }

    #[test]
    fn devin_plan_without_quota_windows_keeps_its_name_and_reports_no_window() {
        let body = r#"{"userStatus":{"planStatus":{"planInfo":{"planName":"Teams"},"availablePromptCredits":500}}}"#;
        let status = parse_user_status(body).unwrap();
        assert_eq!(status.plan.as_deref(), Some("Teams"));
        assert!(status.limits.is_empty());
    }

    #[test]
    fn devin_unreadable_answer_is_a_decode_error_that_quotes_nothing() {
        for body in [
            "<html>secret@example.com</html>",
            r#"{"userStatus":{"email":"secret@example.com"}}"#,
            r#"{"userStatus":{"email":"secret@example.com","planStatus":[]}}"#,
            "{}",
        ] {
            let error = parse_user_status(body).unwrap_err();
            assert!(matches!(error, ReadError::Decode { .. }), "{error:?}");
            assert!(!error.to_string().contains("secret"), "{error}");
        }
    }

    #[test]
    fn devin_plan_status_key_or_billing_strategy_it_does_not_know_is_reported_by_name() {
        let body = r#"{"userStatus":{"email":"a@example.com","planStatus":{
            "planInfo":{"planName":"Max","billingStrategy":"BILLING_STRATEGY_TOKENS","newFlag":true},
            "weeklyQuotaRemainingPercent":50,"weeklyQuotaResetAtUnix":"1789286400",
            "monthlyQuotaRemainingPercent":80}}}"#;
        let status = parse_user_status(body).unwrap();
        assert_eq!(
            status.unrecognized,
            [
                "userStatus.planStatus.monthlyQuotaRemainingPercent",
                "userStatus.planStatus.planInfo.billingStrategy=BILLING_STRATEGY_TOKENS",
            ]
        );
    }

    #[test]
    fn devin_credentials_file_gives_the_key_and_an_https_server_only() {
        let (key, base) = parse_credentials(
            "windsurf_api_key = \" k-1 \"\napi_server_url = \"https://server.example.com/\"\ndevin_api_url = \"https://api.devin.ai\"\n",
        )
        .unwrap();
        assert_eq!(
            (key.as_str(), base.as_str()),
            ("k-1", "https://server.example.com")
        );
        assert_eq!(
            parse_credentials("windsurf_api_key = \"k\"\n").unwrap().1,
            BASE
        );
        for (text, why) in [
            ("api_server_url = \"https://x\"\n", "holds no key"),
            ("windsurf_api_key = \"\"\n", "holds no key"),
            (
                "windsurf_api_key = \"k\"\napi_server_url = \"http://x\"\n",
                "not https",
            ),
            ("not toml at all [", "not readable TOML"),
        ] {
            let error = parse_credentials(text).unwrap_err();
            assert!(error.contains(why), "{text}: {error}");
        }
    }

    #[tokio::test]
    async fn devin_read_posts_the_key_in_the_body_and_names_a_refusal() {
        let app = Router::new().route(
            GET_USER_STATUS,
            post(|Json(body): Json<Value>| async move {
                let metadata = &body["metadata"];
                if metadata["ideName"] != CLIENT_NAME || metadata["extensionVersion"].is_null() {
                    (
                        StatusCode::BAD_REQUEST,
                        r#"{"code":"invalid_argument","message":"an internal error occurred"}"#
                            .to_owned(),
                    )
                } else if metadata["apiKey"] == "devin-test-key" {
                    (StatusCode::OK, LIVE.to_owned())
                } else {
                    (
                        StatusCode::UNAUTHORIZED,
                        r#"{"code":"unauthenticated","message":"invalid api key"}"#.to_owned(),
                    )
                }
            }),
        );
        let base = crate::testing::serve(app).await;
        let client = reqwest::Client::new();
        let reading = read_from(&client, Some(&base), &account(), 7).await;
        assert_eq!(reading.error, None);
        assert_eq!(reading.service, Service::Devin);
        assert_eq!(reading.plan.as_deref(), Some("Max"));
        assert_eq!(reading.limits.len(), 1);
        assert!(reading.has_quota());

        let wrong = ServiceAccount {
            key: Some("devin-wrong-key".into()),
            ..account()
        };
        let error = read_from(&client, Some(&base), &wrong, 7)
            .await
            .error
            .unwrap()
            .to_string();
        assert!(error.contains("HTTP 401"), "{error}");
        assert!(
            error.contains("Devin refused the key: invalid api key"),
            "{error}"
        );
        assert!(!error.contains("devin-wrong-key"), "{error}");

        // No key and no file: nothing to read with, and no request.
        let none = ServiceAccount {
            key: None,
            key_file: None,
            ..account()
        };
        let reading = read_from(&client, Some(&base), &none, 7).await;
        assert_eq!(reading.error, Some(ReadError::NoCredential));
    }
}
