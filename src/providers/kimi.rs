//! Kimi Code: `GET /coding/v1/usages` with `Authorization: Bearer <key>`.
//!
//! The `usages` map holds one entry per window as a used ratio and an RFC 3339
//! reset: `limit_5h`, `limit_7d` (legacy plans), and `limit_month_total` with
//! `limit_month_code`, Kimi Code's share of it (new plans). The top-level
//! `usage` and `limits[]` repeat the same numbers as 0-100 strings and are not
//! read. The booster wallet (spelled `booster_wallet` by the server,
//! `boosterWallet` by older clients) is paid extra usage in fixed point,
//! 1e6 units per cent. The plan name comes from `/coding/v1/me`, of which
//! only `user_level_name` is read. See research/zai-kimi.md.

use std::collections::BTreeMap;

use serde::Deserialize;
use serde_json::Value;

use super::{Balance, Meter, Service, ServiceAccount, Usage};
use crate::quota::Window;

pub const BASE: &str = "https://api.kimi.com/coding/v1";

const FIXED_POINT_PER_CENT: f64 = 1e6;

/// The windows Kimi reports, in display order, with their labels and lengths.
const KNOWN: [(&str, &str, Option<i64>); 4] = [
    ("limit_5h", "5h", Some(5 * 3600)),
    ("limit_7d", "weekly", Some(7 * 86_400)),
    ("limit_month_total", "monthly", None),
    ("limit_month_code", "monthly (Kimi Code share)", None),
];

pub async fn read(client: &reqwest::Client, account: &ServiceAccount, now: i64) -> Usage {
    read_from(client, BASE, account, now).await
}

pub(crate) async fn read_from(
    client: &reqwest::Client,
    base: &str,
    account: &ServiceAccount,
    now: i64,
) -> Usage {
    let failed = |error: String| Usage::failed(Service::Kimi, &account.name, now, error);
    let key = match account.api_key() {
        Ok(key) => key,
        Err(error) => return failed(format!("{error:#}")),
    };
    let base = base.trim_end_matches('/');
    let (usages_url, me_url) = (format!("{base}/usages"), format!("{base}/me"));
    let (usages, me) = futures::join!(get(client, &usages_url, &key), get(client, &me_url, &key),);
    let parsed = usages.and_then(|body| parse_usages(&body));
    match parsed {
        Ok((meters, balances)) => Usage {
            service: Service::Kimi,
            account: account.name.clone(),
            at: now,
            // The plan is optional: a failed `/me` costs only the name.
            plan: me.ok().and_then(|body| parse_plan(&body)),
            meters,
            balances,
            error: None,
        },
        Err(error) => failed(error),
    }
}

async fn get(client: &reqwest::Client, url: &str, key: &str) -> Result<String, String> {
    let response = client
        .get(url)
        .bearer_auth(key)
        .header(reqwest::header::ACCEPT, "application/json")
        .send()
        .await
        .map_err(|error| format!("request failed: {error}"))?;
    let status = response.status();
    let body = response
        .text()
        .await
        .map_err(|error| format!("reading the response failed: {error}"))?;
    if status.is_success() {
        return Ok(body);
    }
    let message = serde_json::from_str::<Value>(&body)
        .ok()
        .and_then(|value| value["error"]["message"].as_str().map(str::to_owned))
        .unwrap_or_else(|| super::snippet(&body));
    Err(match status.as_u16() {
        401 => format!("Kimi refused the key (HTTP 401): {message}"),
        404 => format!("no usage endpoint for this key (HTTP 404): {message}"),
        code => format!("HTTP {code}: {message}"),
    })
}

#[derive(Deserialize)]
struct UsagesBody {
    #[serde(default)]
    usages: Option<BTreeMap<String, Pool>>,
    #[serde(default, alias = "boosterWallet")]
    booster_wallet: Option<Wallet>,
}

#[derive(Deserialize)]
struct Pool {
    /// A number, or a number in a string.
    #[serde(default)]
    used_ratio: Option<Value>,
    #[serde(default)]
    reset_time: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Wallet {
    #[serde(default)]
    balance: Option<WalletBalance>,
    #[serde(default)]
    monthly_charge_limit: Option<Money>,
    #[serde(default)]
    monthly_used: Option<Money>,
    #[serde(default)]
    topup_limit: Option<Money>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct WalletBalance {
    #[serde(default, rename = "type")]
    kind: Option<String>,
    #[serde(default)]
    amount: Option<Value>,
    #[serde(default)]
    amount_left: Option<Value>,
}

#[derive(Deserialize)]
struct Money {
    #[serde(default)]
    currency: Option<String>,
}

fn number(value: Option<&Value>) -> Option<f64> {
    match value? {
        Value::Number(number) => number.as_f64(),
        Value::String(text) => text.trim().parse().ok(),
        _ => None,
    }
    .filter(|n: &f64| n.is_finite())
}

fn fixed_point_to_cents(value: f64) -> i64 {
    let cents = value / FIXED_POINT_PER_CENT;
    if cents > 0.0 && cents < 1.0 {
        1
    } else {
        cents.round() as i64
    }
}

fn parse_reset(text: Option<&str>) -> i64 {
    text.and_then(|text| chrono::DateTime::parse_from_rfc3339(text).ok())
        .map_or(0, |at| at.timestamp())
}

fn meter(label: String, span_secs: Option<i64>, pool: &Pool) -> Option<Meter> {
    Some(Meter {
        group: None,
        label,
        window: Window::new(
            number(pool.used_ratio.as_ref())?,
            parse_reset(pool.reset_time.as_deref()),
        ),
        span_secs,
        amount: None,
    })
}

fn parse_usages(body: &str) -> Result<(Vec<Meter>, Vec<Balance>), String> {
    let parsed: UsagesBody = serde_json::from_str(body)
        .map_err(|error| format!("unreadable answer ({error}): {}", super::snippet(body)))?;
    let mut pools = parsed.usages.unwrap_or_default();
    let mut meters: Vec<Meter> = KNOWN
        .iter()
        .filter_map(|(key, label, span)| meter((*label).into(), *span, &pools.remove(*key)?))
        .collect();
    meters.extend(pools.iter().filter_map(|(key, pool)| {
        meter(key.strip_prefix("limit_").unwrap_or(key).into(), None, pool)
    }));
    if meters.is_empty() {
        return Err("the answer has no usage windows".into());
    }
    let balances = parsed
        .booster_wallet
        .as_ref()
        .and_then(booster)
        .into_iter()
        .collect();
    Ok((meters, balances))
}

fn booster(wallet: &Wallet) -> Option<Balance> {
    let balance = wallet.balance.as_ref()?;
    if balance.kind.as_deref() != Some("BOOSTER") {
        return None;
    }
    let total = number(balance.amount.as_ref()).filter(|&amount| amount > 0.0)?;
    let left = number(balance.amount_left.as_ref()).unwrap_or(0.0);
    let currency = [
        &wallet.monthly_charge_limit,
        &wallet.monthly_used,
        &wallet.topup_limit,
    ]
    .into_iter()
    .flatten()
    .filter_map(|money| money.currency.as_deref())
    .find(|currency| !currency.is_empty())
    .unwrap_or("USD");
    Some(Balance {
        label: "extra usage".into(),
        left_minor: fixed_point_to_cents(left),
        total_minor: Some(fixed_point_to_cents(total)),
        currency: currency.to_owned(),
    })
}

/// `user_level_name` and nothing else: `/me` also carries personal details.
fn parse_plan(body: &str) -> Option<String> {
    #[derive(Deserialize)]
    struct Me {
        #[serde(default)]
        user_level_name: Option<String>,
    }
    serde_json::from_str::<Me>(body)
        .ok()?
        .user_level_name
        .filter(|name| !name.trim().is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{Router, http::HeaderMap, http::StatusCode, routing::get};

    /// 2026-11-11 20:30 UTC, when the body below was captured (ids redacted).
    const NOW: i64 = 1_794_429_051;

    const LIVE: &str = r#"{"usage":{"limit":"100","used":"21","remaining":"79","resetTime":"2026-11-12T21:40:19Z"},
        "limits":[{"window":{"duration":300,"timeUnit":"TIME_UNIT_MINUTE"},"detail":{"limit":"100","used":"1","remaining":"99","resetTime":"2026-11-11T23:40:19Z"}}],
        "booster_wallet":{"id":"redacted","userId":"redacted","balance":{"id":"redacted","feature":"FEATURE_OMNI","type":"BOOSTER","amount":"5000000000","amountLeft":"2195500648","unit":"UNIT_CURRENCY"},
          "status":"STATUS_ACTIVE","allowTopup":true,"topupLimit":{"currency":"USD","priceInCents":"100000"},
          "monthlyChargeLimitEnabled":true,"monthlyChargeLimit":{"currency":"USD","priceInCents":"5000"},"monthlyUsed":{"currency":"USD","priceInCents":"0"}},
        "usages":{"limit_5h":{"used_ratio":0.0057,"reset_time":"2026-11-11T23:40:17Z"},"limit_7d":{"used_ratio":0.2119,"reset_time":"2026-11-12T21:40:18Z"}}}"#;

    const ME: &str = r#"{"user_id":"redacted","nickname":"redacted","phone":{"number":"redacted"},"status":"USER_STATUS_NORMAL","region":"REGION_OVERSEA","user_level":30,"user_level_name":"Vivace"}"#;

    fn account() -> ServiceAccount {
        ServiceAccount {
            key: Some("sk-kimi-test".into()),
            ..ServiceAccount::with_key_file(Service::Kimi, "kimi", "unused")
        }
    }

    #[test]
    fn the_usages_map_becomes_meters_and_the_legacy_fields_are_ignored() {
        let (meters, balances) = parse_usages(LIVE).unwrap();
        let summary: Vec<_> = meters
            .iter()
            .map(|m| (m.label.as_str(), m.window.used, m.span_secs))
            .collect();
        assert_eq!(
            summary,
            [
                ("5h", 0.0057, Some(5 * 3600)),
                ("weekly", 0.2119, Some(7 * 86_400))
            ]
        );
        assert_eq!(
            meters[0].window.resets_at,
            chrono::DateTime::parse_from_rfc3339("2026-11-11T23:40:17Z")
                .unwrap()
                .timestamp()
        );
        assert_eq!(
            balances,
            [Balance {
                label: "extra usage".into(),
                left_minor: 2196,
                total_minor: Some(5000),
                currency: "USD".into(),
            }]
        );
    }

    #[test]
    fn new_plan_monthly_pools_and_the_camel_case_wallet_are_read() {
        let body = r#"{"usages":{
            "limit_month_code":{"used_ratio":"0.1","reset_time":"2026-12-01T15:00:00Z"},
            "limit_5h":{"used_ratio":0.5},
            "limit_month_total":{"used_ratio":0.3,"reset_time":"2026-12-01T15:00:00Z"},
            "limit_future":{"used_ratio":0.9},
            "limit_broken":{"used_ratio":null}},
          "boosterWallet":{"balance":{"type":"BOOSTER","amount":2000000,"amountLeft":400000}}}"#;
        let (meters, balances) = parse_usages(body).unwrap();
        let labels: Vec<_> = meters.iter().map(|m| m.label.as_str()).collect();
        assert_eq!(
            labels,
            ["5h", "monthly", "monthly (Kimi Code share)", "future"]
        );
        assert_eq!(meters[0].window.resets_at, 0, "no reset_time, no reset");
        assert!((meters[2].window.used - 0.1).abs() < 1e-12);
        // 0.4 cents left still shows as a cent.
        assert_eq!(balances[0].left_minor, 1);
        assert_eq!(balances[0].total_minor, Some(2));
    }

    #[test]
    fn a_wallet_that_is_not_a_booster_or_is_empty_is_no_balance() {
        for wallet in [
            r#"{"balance":{"type":"GIFT","amount":"5000000000","amountLeft":"1"}}"#,
            r#"{"balance":{"type":"BOOSTER","amount":"0","amountLeft":"0"}}"#,
        ] {
            let body = format!(
                r#"{{"usages":{{"limit_5h":{{"used_ratio":0.1}}}},"booster_wallet":{wallet}}}"#
            );
            assert!(parse_usages(&body).unwrap().1.is_empty(), "{wallet}");
        }
        assert!(parse_usages(r#"{"usage":{"limit":"100","used":"21"}}"#).is_err());
    }

    #[test]
    fn the_plan_is_the_level_name_only() {
        assert_eq!(parse_plan(ME).as_deref(), Some("Vivace"));
        assert_eq!(parse_plan(r#"{"user_level_name":""}"#), None);
    }

    #[tokio::test]
    async fn read_sends_bearer_and_survives_a_failed_me() {
        fn authorized(headers: &HeaderMap) -> bool {
            headers.get("authorization").and_then(|v| v.to_str().ok())
                == Some("Bearer sk-kimi-test")
        }
        let app = Router::new()
            .route(
                "/coding/v1/usages",
                get(|headers: HeaderMap| async move {
                    if authorized(&headers) {
                        (StatusCode::OK, LIVE.to_owned())
                    } else {
                        (
                            StatusCode::UNAUTHORIZED,
                            r#"{"error":{"message":"Invalid Authentication","type":"invalid_authentication_error"}}"#.to_owned(),
                        )
                    }
                }),
            )
            .route(
                "/coding/v1/me",
                get(|| async { (StatusCode::INTERNAL_SERVER_ERROR, "down") }),
            );
        let base = format!("{}/coding/v1", crate::testing::serve(app).await);
        let client = reqwest::Client::new();
        let usage = read_from(&client, &base, &account(), NOW).await;
        assert_eq!(usage.error, None);
        assert_eq!(usage.plan, None);
        assert_eq!(usage.meters.len(), 2);
        assert_eq!(usage.balances.len(), 1);

        let wrong = ServiceAccount {
            key: Some("sk-kimi-wrong".into()),
            ..account()
        };
        let usage = read_from(&client, &base, &wrong, NOW).await;
        let error = usage.error.unwrap();
        assert!(error.contains("HTTP 401"), "{error}");
        assert!(error.contains("Invalid Authentication"), "{error}");
    }

    #[tokio::test]
    async fn read_takes_the_plan_from_me() {
        let app = Router::new()
            .route("/coding/v1/usages", get(|| async { LIVE }))
            .route("/coding/v1/me", get(|| async { ME }));
        let base = format!("{}/coding/v1", crate::testing::serve(app).await);
        let usage = read_from(&reqwest::Client::new(), &base, &account(), NOW).await;
        assert_eq!(usage.plan.as_deref(), Some("Vivace"));
    }
}
