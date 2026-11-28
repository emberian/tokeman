//! Grok Build (xAI): `GET /v1/billing?format=credits` on the CLI's proxy, with
//! the access token of the login the `grok` CLI keeps in `~/.grok/auth.json`.
//!
//! `config` holds one usage period (`currentPeriod`: weekly or monthly, with
//! its start and end) and `creditUsagePercent`, the share of the period's
//! included credits that is used, 0-100. The body is proto3 JSON, so a zero is
//! omitted: a period with no percentage has used nothing. Money is in cents
//! under `val`: `prepaidBalance` (bought credits) and the on-demand cap and
//! spend. The older shape (`monthlyLimit`, `used`, `billingPeriodStart`/`End`)
//! is read where the newer fields are missing.
//!
//! The access token lives six hours and its refresh token is single-use, so
//! tokeman never redeems it: only `grok` renews its login. An expired or
//! signed-out file is a credential error that says so. The plan name is not
//! in the answer; it is read from the remote settings `grok` caches beside
//! the login. See research/devin-grok.md.

use std::path::{Path, PathBuf};

use serde::Deserialize;
use serde_json::Value;

use super::{ServiceAccount, credential, decode, limit, network};
use crate::drift::Unrecognized;
use crate::paths;
use crate::quota::{Balance, Limit, ReadError, Reading, Service, Span, Window};
use crate::util::{one_line, parse_reset};

pub const BASE: &str = "https://cli-chat-proxy.grok.com/v1";

/// The value of `X-XAI-Token-Auth` that marks a token as the CLI's
/// (grok-build `GrokComConfig::token_header`).
const TOKEN_AUTH: &str = "xai-grok-cli";

/// A token this close to its end is left alone: the request would race it.
const EXPIRY_MARGIN_SECS: i64 = 30;

/// Where the grok CLI keeps its login: `$GROK_HOME/auth.json`, `~/.grok/auth.json`
/// without the variable.
pub fn auth_file() -> PathBuf {
    std::env::var_os("GROK_HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .unwrap_or_else(|| paths::home().join(".grok"))
        .join("auth.json")
}

/// One login in `auth.json`: the file is a map from `<issuer>::<client id>`
/// to these.
#[derive(Deserialize)]
struct Entry {
    /// The access token.
    #[serde(default)]
    key: Option<String>,
    #[serde(default)]
    user_id: Option<String>,
    #[serde(default)]
    email: Option<String>,
    /// RFC 3339.
    #[serde(default)]
    expires_at: Option<String>,
}

/// A login that can be sent.
struct Login {
    token: String,
    user_id: Option<String>,
}

/// The logins in the text of an `auth.json`.
fn entries(text: &str) -> Result<Vec<Entry>, &'static str> {
    let map: std::collections::BTreeMap<String, Entry> =
        serde_json::from_str(text).map_err(|_| "grok's login file is not readable")?;
    Ok(map.into_values().collect())
}

/// The login of `auth.json`'s `text` for the account called `name` (its email
/// when the file holds several), if it is live at `now`.
fn parse_login(text: &str, name: &str, label: Option<&str>, now: i64) -> Result<Login, String> {
    let mut entries = entries(text)?;
    if entries.is_empty() {
        return Err("grok is signed out: run `grok login`".to_owned());
    }
    let is_account = |entry: &Entry| {
        entry.email.as_deref().is_some_and(|email| {
            email.eq_ignore_ascii_case(name)
                || label.is_some_and(|label| email.eq_ignore_ascii_case(label))
        })
    };
    let entry = match entries.iter().position(is_account) {
        Some(at) => entries.swap_remove(at),
        None if entries.len() == 1 => entries.swap_remove(0),
        None => {
            return Err(format!(
                "grok's login file holds several logins and none is {name}"
            ));
        }
    };
    let token = entry
        .key
        .map(|key| key.trim().to_owned())
        .filter(|key| !key.is_empty() && !key.chars().any(char::is_control))
        .ok_or("grok's login has no access token: run `grok login`")?;
    let expires_at = parse_reset(entry.expires_at.as_deref());
    if expires_at > 0 && expires_at <= now + EXPIRY_MARGIN_SECS {
        let when = chrono::DateTime::from_timestamp(expires_at, 0)
            .map(|time| time.format("%Y-%m-%d %H:%M UTC").to_string())
            .unwrap_or_default();
        return Err(format!(
            "grok's login expired {when}: start `grok` to renew it, or run `grok login` if it asks \
             (tokeman does not redeem grok's single-use refresh token)"
        ));
    }
    Ok(Login {
        token,
        user_id: entry.user_id.filter(|id| !id.trim().is_empty()),
    })
}

fn login_file(account: &ServiceAccount) -> Result<PathBuf, ReadError> {
    account
        .key_file
        .as_deref()
        .map(paths::expand_tilde)
        .ok_or(ReadError::NoCredential)
}

fn login(account: &ServiceAccount, now: i64) -> Result<Login, ReadError> {
    let path = login_file(account)?;
    let text = std::fs::read_to_string(&path).map_err(|error| {
        credential(format!(
            "cannot read grok's login file ({error}): run `grok login` ({})",
            paths::tilde(&path)
        ))
    })?;
    parse_login(&text, &account.name, account.label.as_deref(), now)
        .map_err(|why| credential(format!("{why} ({})", paths::tilde(&path))))
}

/// The email of the one login in the grok CLI's `auth.json` at `path`: what
/// `tokeman grok import` names the account. `None` for a login without one.
pub fn email(path: &Path) -> anyhow::Result<Option<String>> {
    let text = std::fs::read_to_string(path)
        .map_err(|error| anyhow::anyhow!("cannot read {}: {error}", path.display()))?;
    let entries = entries(&text).map_err(|why| anyhow::anyhow!("{why} ({})", path.display()))?;
    match entries.as_slice() {
        [] => anyhow::bail!(
            "grok is signed out: run `grok login`, then this again ({} holds no login)",
            path.display()
        ),
        [entry] => Ok(entry
            .email
            .as_deref()
            .map(str::trim)
            .filter(|email| !email.is_empty())
            .map(str::to_owned)),
        _ => anyhow::bail!(
            "{} holds several logins; name one with --name <its email>",
            path.display()
        ),
    }
}

pub async fn read(client: &reqwest::Client, account: &ServiceAccount, now: i64) -> Reading {
    read_from(client, BASE, account, now).await
}

pub(crate) async fn read_from(
    client: &reqwest::Client,
    base: &str,
    account: &ServiceAccount,
    now: i64,
) -> Reading {
    let failed = |error: ReadError| Reading::failed(Service::Grok, &account.name, now, error);
    let login = match login(account, now) {
        Ok(login) => login,
        Err(error) => return failed(error),
    };
    match billing(client, base.trim_end_matches('/'), &login)
        .await
        .and_then(|body| parse_billing(&body))
    {
        Ok(billing) => Reading {
            plan: billing.plan.or_else(|| {
                let cache = login_file(account)
                    .ok()?
                    .with_file_name("settings_cache.json");
                cached_plan(&std::fs::read_to_string(cache).ok()?)
            }),
            limits: billing.limits,
            balances: billing.balances,
            unrecognized: billing.unrecognized,
            ..Reading::empty(Service::Grok, &account.name, now)
        },
        Err(error) => failed(error),
    }
}

async fn billing(client: &reqwest::Client, base: &str, login: &Login) -> Result<String, ReadError> {
    let mut request = client
        .get(format!("{base}/billing?format=credits"))
        .bearer_auth(&login.token)
        .header("X-XAI-Token-Auth", TOKEN_AUTH)
        .header(reqwest::header::ACCEPT, "application/json");
    if let Some(user_id) = &login.user_id {
        request = request.header("x-userid", user_id);
    }
    let response = request
        .send()
        .await
        .map_err(|error| network(format!("request failed: {error}")))?;
    let status = response.status();
    let body = response
        .text()
        .await
        .map_err(|error| network(format!("reading the response failed: {error}")))?;
    if status.is_success() {
        return Ok(body);
    }
    // `{"error":"Invalid or expired credentials (...)"}`.
    let message = serde_json::from_str::<Value>(&body)
        .ok()
        .and_then(|value| value["error"].as_str().map(str::to_owned))
        .unwrap_or_else(|| one_line(&body, 160));
    Err(ReadError::Http {
        status: status.as_u16(),
        body: match status.as_u16() {
            401 | 403 => format!(
                "Grok refused the login: {}; start `grok`, or run `grok login`",
                one_line(&message, 160)
            ),
            _ => one_line(&message, 160),
        },
    })
}

#[derive(Deserialize)]
struct Body {
    #[serde(default)]
    config: Option<Config>,
    /// Not sent by the proxy today: grok adds these from its remote settings
    /// before handing the answer to its own UI. Read in either spelling.
    #[serde(default, alias = "subscriptionTier")]
    subscription_tier: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Config {
    /// 0-100. Omitted at zero.
    #[serde(default)]
    credit_usage_percent: Option<Value>,
    #[serde(default)]
    current_period: Option<Period>,
    #[serde(default)]
    monthly_limit: Option<Cent>,
    #[serde(default)]
    used: Option<Cent>,
    #[serde(default)]
    on_demand_cap: Option<Cent>,
    #[serde(default)]
    on_demand_used: Option<Cent>,
    #[serde(default)]
    prepaid_balance: Option<Cent>,
    #[serde(default)]
    billing_period_start: Option<String>,
    #[serde(default)]
    billing_period_end: Option<String>,
}

#[derive(Deserialize)]
struct Period {
    #[serde(default, rename = "type")]
    kind: Option<String>,
    #[serde(default)]
    start: Option<String>,
    #[serde(default)]
    end: Option<String>,
}

/// US cents. A zero arrives as `{}`.
#[derive(Deserialize)]
struct Cent {
    #[serde(default)]
    val: Option<Value>,
}

impl Cent {
    fn cents(&self) -> i64 {
        number(self.val.as_ref()).map_or(0, |cents| cents.round() as i64)
    }
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

// The known sets of the billing answer: what this parser reads, then what
// has been seen and is not read (research/devin-grok.md).

const KNOWN_KEYS: &[&str] = &[
    "config",
    "subscription_tier",
    "subscriptionTier",
    // Added by grok itself beside the tier; seen in its own log.
    "on_demand_enabled",
    "onDemandEnabled",
];
const KNOWN_CONFIG_KEYS: &[&str] = &[
    "creditUsagePercent",
    "currentPeriod",
    "monthlyLimit",
    "used",
    "onDemandCap",
    "onDemandUsed",
    "prepaidBalance",
    "billingPeriodStart",
    "billingPeriodEnd",
    // In grok-build's `BillingConfig` and its tests, not read: whether the
    // account is on the shared pool, past periods, per-product percentages.
    "isUnifiedBillingUser",
    "history",
    "productUsage",
];
const KNOWN_PERIOD_KEYS: &[&str] = &["type", "start", "end"];
/// Weekly seen in grok's log; monthly from grok-build's doc comment.
const KNOWN_PERIOD_TYPES: &[&str] = &["USAGE_PERIOD_TYPE_WEEKLY", "USAGE_PERIOD_TYPE_MONTHLY"];
const KNOWN_CENT_KEYS: &[&str] = &["val"];
const CENTS: [&str; 5] = [
    "monthlyLimit",
    "used",
    "onDemandCap",
    "onDemandUsed",
    "prepaidBalance",
];

fn unrecognized(body: &Value) -> Vec<String> {
    let mut seen = Unrecognized::new();
    seen.keys("billing", body, KNOWN_KEYS);
    let config = &body["config"];
    seen.keys("billing.config", config, KNOWN_CONFIG_KEYS);
    let period = &config["currentPeriod"];
    seen.keys("billing.config.currentPeriod", period, KNOWN_PERIOD_KEYS);
    seen.value(
        "billing.config.currentPeriod.type",
        period.get("type"),
        KNOWN_PERIOD_TYPES,
    );
    for cent in CENTS {
        seen.keys(
            &format!("billing.config.{cent}"),
            &config[cent],
            KNOWN_CENT_KEYS,
        );
    }
    seen.into_vec()
}

/// What the billing answer holds for tokeman.
#[derive(Debug)]
struct Billing {
    plan: Option<String>,
    limits: Vec<Limit>,
    balances: Vec<Balance>,
    unrecognized: Vec<String>,
}

fn parse_billing(body: &str) -> Result<Billing, ReadError> {
    let unreadable = |error: serde_json::Error| {
        decode(format!(
            "unreadable answer ({error}): {}",
            one_line(body, 160)
        ))
    };
    let value: Value = serde_json::from_str(body).map_err(unreadable)?;
    let unrecognized = unrecognized(&value);
    let parsed: Body = serde_json::from_value(value).map_err(unreadable)?;
    let Some(config) = parsed.config else {
        return Err(decode("the answer has no billing config"));
    };
    // The period: the typed one, else the older pair of dates.
    let period = config.current_period.as_ref();
    let start = period
        .and_then(|period| period.start.as_deref())
        .or(config.billing_period_start.as_deref());
    let end = period
        .and_then(|period| period.end.as_deref())
        .or(config.billing_period_end.as_deref());
    let (starts_at, ends_at) = (parse_reset(start), parse_reset(end));
    let span_secs = match period.and_then(|period| period.kind.as_deref()) {
        _ if starts_at > 0 && ends_at > starts_at => Some(ends_at - starts_at),
        Some("USAGE_PERIOD_TYPE_WEEKLY") => Some(Span::WEEK_SECS),
        Some("USAGE_PERIOD_TYPE_MONTHLY") => Some(30 * Span::DAY_SECS),
        _ => None,
    };
    let has_period = period.is_some() || ends_at > 0;
    let percent = number(config.credit_usage_percent.as_ref()).map(|percent| percent / 100.0);
    let legacy = config.monthly_limit.as_ref().and_then(|limit| {
        let limit = limit.cents();
        let used = config.used.as_ref().map_or(0, Cent::cents);
        (limit > 0).then(|| used as f64 / limit as f64)
    });
    // A period with neither number has used nothing: the zero was omitted.
    let used = percent.or(legacy).or(has_period.then_some(0.0));
    let Some(used) = used else {
        return Err(decode("the answer has no usage period"));
    };
    let span = Span::from_seconds(span_secs);
    let limits = vec![limit(
        None,
        span.label(),
        span_secs,
        Window::new(used.max(0.0), ends_at),
        None,
    )];

    let mut balances = Vec::new();
    let prepaid = config.prepaid_balance.as_ref().map_or(0, Cent::cents);
    if prepaid != 0 {
        balances.push(Balance {
            label: "prepaid credits".into(),
            left_minor: prepaid,
            total_minor: None,
            currency: "USD".into(),
        });
    }
    let cap = config.on_demand_cap.as_ref().map_or(0, Cent::cents);
    if cap > 0 {
        let spent = config.on_demand_used.as_ref().map_or(0, Cent::cents);
        balances.push(Balance {
            label: "on-demand".into(),
            left_minor: cap - spent,
            total_minor: Some(cap),
            currency: "USD".into(),
        });
    }
    Ok(Billing {
        plan: parsed
            .subscription_tier
            .map(|tier| tier.trim().to_owned())
            .filter(|tier| !tier.is_empty()),
        limits,
        balances,
        unrecognized,
    })
}

/// The plan name in grok's `settings_cache.json`: `payload` is a JSON
/// document in a string, whose `settings` carry `subscription_tier_display`
/// ("SuperGrok Heavy") and `subscription_tier`.
fn cached_plan(text: &str) -> Option<String> {
    let cache: Value = serde_json::from_str(text).ok()?;
    let payload: Value = serde_json::from_str(cache["payload"].as_str()?).ok()?;
    let settings = &payload["settings"];
    ["subscription_tier_display", "subscription_tier"]
        .into_iter()
        .filter_map(|key| settings[key].as_str())
        .map(str::trim)
        .find(|tier| !tier.is_empty())
        .map(str::to_owned)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{Router, http::HeaderMap, http::StatusCode, routing::get};

    /// 2026-10-07 02:13 UTC.
    const NOW: i64 = 1_791_339_232;

    /// The billing config of a SuperGrok Heavy login as grok 1.0.46 wrote it
    /// to `~/.grok/logs/unified.jsonl` at [`NOW`]: its client's own
    /// re-serialization of the answer (`BillingConfig`, camelCase, `None`s
    /// skipped), with the `historyLen` its logger adds removed. Nothing in it
    /// identifies the account. Not a capture off the wire: the login had
    /// expired when this was written (research/devin-grok.md).
    const LOGGED: &str = r#"{"config":{"creditUsagePercent":6.0,
        "currentPeriod":{"type":"USAGE_PERIOD_TYPE_WEEKLY","start":"2026-10-02T04:07:01.963528+00:00","end":"2026-10-09T04:07:01.963528+00:00"},
        "onDemandCap":{"val":0},"onDemandUsed":{"val":0},"prepaidBalance":{"val":0},"isUnifiedBillingUser":true,
        "billingPeriodStart":"2026-10-02T04:07:01.963528+00:00","billingPeriodEnd":"2026-10-09T04:07:01.963528+00:00"}}"#;

    /// The same account four days later, at the start of a period, before any
    /// use: no `creditUsagePercent`.
    const LOGGED_FRESH_WEEK: &str = r#"{"config":{
        "currentPeriod":{"type":"USAGE_PERIOD_TYPE_WEEKLY","start":"2026-10-09T04:07:01.963528+00:00","end":"2026-10-16T04:07:01.963528+00:00"},
        "onDemandCap":{"val":0},"onDemandUsed":{"val":0},"prepaidBalance":{"val":0},"isUnifiedBillingUser":true,
        "billingPeriodStart":"2026-10-09T04:07:01.963528+00:00","billingPeriodEnd":"2026-10-16T04:07:01.963528+00:00"}}"#;

    fn auth(expires_at: &str) -> String {
        format!(
            r#"{{"https://auth.x.ai::client":{{"key":"grok-test-token","auth_mode":"oidc","user_id":"user-1",
                "email":"first@example.com","refresh_token":"grok-refresh-secret","expires_at":"{expires_at}"}}}}"#
        )
    }

    fn account(dir: &Path, auth: &str) -> ServiceAccount {
        let file = dir.join("auth.json");
        std::fs::write(&file, auth).unwrap();
        ServiceAccount::with_key_file(Service::Grok, "first@example.com", file.to_str().unwrap())
    }

    #[test]
    fn grok_logged_billing_becomes_the_weekly_window() {
        let billing = parse_billing(LOGGED).unwrap();
        assert!(
            billing.unrecognized.is_empty(),
            "{:?}",
            billing.unrecognized
        );
        assert_eq!(billing.limits.len(), 1);
        let weekly = &billing.limits[0];
        assert_eq!(weekly.span, Span::Weekly);
        assert!(weekly.is_shared());
        assert!((weekly.window.used - 0.06).abs() < 1e-9);
        assert_eq!(
            weekly.window.resets_at,
            chrono::DateTime::parse_from_rfc3339("2026-10-09T04:07:01.963528+00:00")
                .unwrap()
                .timestamp()
        );
        // No bought credits and no on-demand cap: no balance to show.
        assert!(billing.balances.is_empty());
        assert_eq!(billing.plan, None, "the answer does not name the plan");
    }

    #[test]
    fn grok_period_without_a_percentage_has_used_nothing() {
        let billing = parse_billing(LOGGED_FRESH_WEEK).unwrap();
        assert_eq!(billing.limits[0].span, Span::Weekly);
        assert_eq!(billing.limits[0].window.used, 0.0);
    }

    #[test]
    fn grok_money_fields_and_the_older_shape_are_read() {
        // grok-build's own test bodies (extensions/billing.rs).
        let credits = r#"{"config":{"creditUsagePercent":42.5,
            "currentPeriod":{"type":"USAGE_PERIOD_TYPE_MONTHLY","start":"2026-06-01T00:00:00Z","end":"2026-07-01T00:00:00Z"},
            "onDemandCap":{"val":5000},"onDemandUsed":{"val":300},"prepaidBalance":{"val":1250},
            "isUnifiedBillingUser":true,"productUsage":[{"product":"PRODUCT_GROK_BUILD","usagePercent":61.2}],
            "history":[{"period":{"type":"USAGE_PERIOD_TYPE_WEEKLY"},"onDemandUsed":{"val":120}}]},
            "subscriptionTier":"SuperGrok"}"#;
        let billing = parse_billing(credits).unwrap();
        assert!(
            billing.unrecognized.is_empty(),
            "{:?}",
            billing.unrecognized
        );
        assert_eq!(billing.limits[0].span, Span::Monthly);
        assert!((billing.limits[0].window.used - 0.425).abs() < 1e-9);
        assert_eq!(billing.plan.as_deref(), Some("SuperGrok"));
        assert_eq!(
            billing.balances,
            [
                Balance {
                    label: "prepaid credits".into(),
                    left_minor: 1250,
                    total_minor: None,
                    currency: "USD".into(),
                },
                Balance {
                    label: "on-demand".into(),
                    left_minor: 4700,
                    total_minor: Some(5000),
                    currency: "USD".into(),
                },
            ]
        );

        let legacy = r#"{"config":{"monthlyLimit":{"val":2000},"used":{"val":500},"onDemandCap":{},
            "billingPeriodStart":"2025-04-01T00:00:00Z","billingPeriodEnd":"2025-05-01T00:00:00Z"}}"#;
        let billing = parse_billing(legacy).unwrap();
        assert_eq!(billing.limits[0].span, Span::Monthly);
        assert!((billing.limits[0].window.used - 0.25).abs() < 1e-9);
        assert!(billing.balances.is_empty());
    }

    #[test]
    fn grok_unreadable_or_empty_answer_is_a_decode_error() {
        for body in [
            "<html>down</html>",
            r#"{"config":null}"#,
            "{}",
            // Nothing that says a period exists: no window is invented.
            r#"{"config":{"prepaidBalance":{"val":100}}}"#,
            r#"{"config":{"currentPeriod":"weekly"}}"#,
        ] {
            let error = parse_billing(body).unwrap_err();
            assert!(
                matches!(error, ReadError::Decode { .. }),
                "{body}: {error:?}"
            );
        }
    }

    #[test]
    fn grok_key_or_period_type_it_does_not_know_is_reported_by_name() {
        let body = r#"{"config":{"creditUsagePercent":1,
            "currentPeriod":{"type":"USAGE_PERIOD_TYPE_DAILY","start":"2026-06-01T00:00:00Z","end":"2026-06-02T00:00:00Z","grace":true},
            "prepaidBalance":{"val":0,"currency":"USD"},"burstPercent":3},"notices":[]}"#;
        let billing = parse_billing(body).unwrap();
        assert_eq!(billing.limits[0].span, Span::Other(86_400));
        assert_eq!(
            billing.unrecognized,
            [
                "billing.config.burstPercent",
                "billing.config.currentPeriod.grace",
                "billing.config.currentPeriod.type=USAGE_PERIOD_TYPE_DAILY",
                "billing.config.prepaidBalance.currency",
                "billing.notices",
            ]
        );
    }

    #[test]
    fn grok_login_is_used_only_while_it_is_live_and_never_quoted() {
        let live = parse_login(
            &auth("2026-10-07T08:00:00Z"),
            "first@example.com",
            None,
            NOW,
        );
        let live = live.unwrap();
        assert_eq!(live.token, "grok-test-token");
        assert_eq!(live.user_id.as_deref(), Some("user-1"));
        // An alias finds its login through the label, and one login needs no match.
        assert!(
            parse_login(
                &auth("2026-10-07T08:00:00Z"),
                "work",
                Some("FIRST@example.com"),
                NOW
            )
            .is_ok()
        );
        assert!(parse_login(&auth("2026-10-07T08:00:00Z"), "work", None, NOW).is_ok());

        for (text, why) in [
            (auth("2026-10-07T02:00:00Z"), "expired 2026-10-07 02:00 UTC"),
            ("{}".to_owned(), "grok is signed out"),
            ("[]".to_owned(), "login file is not readable"),
            (
                r#"{"a":{"email":"first@example.com","refresh_token":"grok-refresh-secret"}}"#.to_owned(),
                "has no access token",
            ),
            (
                r#"{"a":{"key":"k","email":"x@example.com"},"b":{"key":"k","email":"y@example.com"}}"#.to_owned(),
                "several logins",
            ),
        ] {
            let error = parse_login(&text, "first@example.com", None, NOW).err().unwrap();
            assert!(error.contains(why), "{error}");
            assert!(!error.contains("grok-"), "{error}");
        }
    }

    #[test]
    fn grok_import_names_the_account_by_the_email_of_the_one_login() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("auth.json");
        std::fs::write(&file, auth("2026-10-07T08:00:00Z")).unwrap();
        assert_eq!(email(&file).unwrap().as_deref(), Some("first@example.com"));
        std::fs::write(&file, "{}").unwrap();
        let error = email(&file).unwrap_err().to_string();
        assert!(error.contains("grok is signed out"), "{error}");
        assert!(email(&dir.path().join("missing.json")).is_err());
    }

    #[test]
    fn grok_plan_comes_from_the_settings_grok_cached() {
        let payload = r#"{"origin":"https://cli-chat-proxy.grok.com/v1","settings":{"subscription_tier":null,"subscription_tier_display":"SuperGrok Heavy"}}"#;
        let cache = serde_json::json!({ "payload": payload, "signature": "s" }).to_string();
        assert_eq!(cached_plan(&cache).as_deref(), Some("SuperGrok Heavy"));
        assert_eq!(cached_plan(r#"{"payload":"{}"}"#), None);
        assert_eq!(cached_plan("not json"), None);
    }

    #[tokio::test]
    async fn grok_read_sends_the_cli_token_and_never_reads_past_an_expired_login() {
        fn authorized(headers: &HeaderMap) -> bool {
            let header = |name: &str| headers.get(name).and_then(|value| value.to_str().ok());
            header("authorization") == Some("Bearer grok-test-token")
                && header("x-xai-token-auth") == Some("xai-grok-cli")
                && header("x-userid") == Some("user-1")
        }
        let app = Router::new().route(
            "/v1/billing",
            get(
                |headers: HeaderMap, query: axum::extract::RawQuery| async move {
                    if query.0.as_deref() != Some("format=credits") {
                        (StatusCode::NOT_FOUND, String::new())
                    } else if authorized(&headers) {
                        (StatusCode::OK, LOGGED.to_owned())
                    } else {
                        (
                            StatusCode::UNAUTHORIZED,
                            r#"{"error":"Invalid or expired credentials (auth_kind=none)"}"#
                                .to_owned(),
                        )
                    }
                },
            ),
        );
        let base = format!("{}/v1", crate::testing::serve(app).await);
        let client = reqwest::Client::new();
        let dir = tempfile::tempdir().unwrap();

        let live = account(dir.path(), &auth("2026-10-07T08:00:00Z"));
        std::fs::write(
            dir.path().join("settings_cache.json"),
            serde_json::json!({ "payload": r#"{"settings":{"subscription_tier_display":"SuperGrok Heavy"}}"# })
                .to_string(),
        )
        .unwrap();
        let reading = read_from(&client, &base, &live, NOW).await;
        assert_eq!(reading.error, None);
        assert_eq!(reading.service, Service::Grok);
        assert_eq!(reading.plan.as_deref(), Some("SuperGrok Heavy"));
        assert!(reading.has_quota());

        // A token the server refuses: its words, not the token.
        let refused = account(
            dir.path(),
            &auth("2026-10-07T08:00:00Z").replace("grok-test-token", "grok-stale-token"),
        );
        let error = read_from(&client, &base, &refused, NOW)
            .await
            .error
            .unwrap()
            .to_string();
        assert!(error.contains("HTTP 401"), "{error}");
        assert!(error.contains("Grok refused the login"), "{error}");
        assert!(!error.contains("grok-stale-token"), "{error}");

        // Expired on disk: a credential error, and no request (this base
        // would answer 200 to the token).
        let expired = account(dir.path(), &auth("2026-10-07T02:00:00Z"));
        let reading = read_from(&client, &base, &expired, NOW).await;
        assert!(
            matches!(&reading.error, Some(ReadError::Credential { message }) if message.contains("expired")),
            "{:?}",
            reading.error
        );
        assert!(reading.limits.is_empty());
    }
}
