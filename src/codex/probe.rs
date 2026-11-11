//! The usage probe: `GET /backend-api/codex/usage` reports which windows
//! exist, how full they are, when they reset, and whether the backend accepts
//! the account at all. It is the only liveness check: `codex login status`
//! reads the local file and reports a revoked account as logged in.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::fmt;
use std::path::PathBuf;
use std::time::Duration;

use super::auth::{AuthFile, CodexHome};
use super::{Client, is_edge_challenge};
use crate::account::CodexAccount;
use crate::quota::Window;

/// Codex identifies itself this way; the backend varies behaviour by originator.
const ORIGINATOR: &str = "codex_cli_rs";
/// Retries after a Cloudflare challenge. The first retry already carries the
/// `__cf_bm` cookie the challenge set, which is usually enough.
const CHALLENGE_RETRIES: u32 = 3;

/// A ChatGPT plan, with the backend's spelling kept for the ones not named.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(from = "String", into = "String")]
pub enum Plan {
    Free,
    Go,
    Plus,
    Pro,
    Team,
    Business,
    Enterprise,
    Edu,
    Other(String),
}

impl Plan {
    pub fn parse(raw: &str) -> Self {
        match raw.to_ascii_lowercase().as_str() {
            "free" => Self::Free,
            "go" => Self::Go,
            "plus" => Self::Plus,
            "pro" => Self::Pro,
            "team" => Self::Team,
            "business" => Self::Business,
            "enterprise" => Self::Enterprise,
            "edu" => Self::Edu,
            _ => Self::Other(raw.to_owned()),
        }
    }

    pub fn as_str(&self) -> &str {
        match self {
            Self::Free => "free",
            Self::Go => "go",
            Self::Plus => "plus",
            Self::Pro => "pro",
            Self::Team => "team",
            Self::Business => "business",
            Self::Enterprise => "enterprise",
            Self::Edu => "edu",
            Self::Other(raw) => raw,
        }
    }
}

impl From<String> for Plan {
    fn from(raw: String) -> Self {
        Self::parse(&raw)
    }
}

impl From<Plan> for String {
    fn from(plan: Plan) -> Self {
        plan.as_str().to_owned()
    }
}

impl fmt::Display for Plan {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// How long a window lasts, which is what names it. The backend moves
/// windows between `primary_window` and `secondary_window` (an account
/// whose only window is the weekly one reports it as primary), so position
/// says nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Span {
    FiveHour,
    Weekly,
    /// Any other length, in seconds.
    Seconds(i64),
    /// The backend did not say.
    Unknown,
}

impl Span {
    pub const FIVE_HOUR_SECS: i64 = 5 * 3_600;
    pub const WEEK_SECS: i64 = 7 * 86_400;

    pub fn from_seconds(seconds: Option<i64>) -> Self {
        match seconds {
            Some(Self::FIVE_HOUR_SECS) => Self::FiveHour,
            Some(Self::WEEK_SECS) => Self::Weekly,
            Some(seconds) if seconds > 0 => Self::Seconds(seconds),
            _ => Self::Unknown,
        }
    }

    /// `5h`, `weekly`, or the span in its largest whole unit (`1d`, `90m`).
    pub fn label(&self) -> String {
        match *self {
            Self::FiveHour => "5h".to_owned(),
            Self::Weekly => "weekly".to_owned(),
            Self::Seconds(seconds) => {
                let unit = [(86_400, "d"), (3_600, "h"), (60, "m")]
                    .into_iter()
                    .find(|(size, _)| seconds % size == 0);
                match unit {
                    Some((size, suffix)) => format!("{}{suffix}", seconds / size),
                    None => format!("{seconds}s"),
                }
            }
            Self::Unknown => "window".to_owned(),
        }
    }
}

impl fmt::Display for Span {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.label())
    }
}

/// One window of a limit, named by its length.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct SpanWindow {
    pub span: Span,
    #[serde(flatten)]
    pub window: Window,
}

/// One metered limit and the windows it is measured over, in the order the
/// backend sent them.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct RateLimit {
    pub allowed: Option<bool>,
    pub limit_reached: Option<bool>,
    pub windows: Vec<SpanWindow>,
}

impl RateLimit {
    /// The window of this length, if the limit has one.
    pub fn window(&self, span: Span) -> Option<Window> {
        self.windows
            .iter()
            .find(|window| window.span == span)
            .map(|window| window.window)
    }

    /// The smallest remaining fraction over every window: whichever bites first.
    pub fn remaining(&self) -> Option<f64> {
        self.windows
            .iter()
            .map(|window| window.window.remaining().clamp(0.0, 1.0))
            .reduce(f64::min)
    }

    /// The window with the least left.
    pub fn tightest(&self) -> Option<&SpanWindow> {
        self.windows
            .iter()
            .min_by(|a, b| a.window.remaining().total_cmp(&b.window.remaining()))
    }

    /// `allowed` is authoritative when present; absence is not a rejection.
    pub fn allows(&self) -> bool {
        self.limit_reached != Some(true) && self.allowed.unwrap_or(true)
    }
}

/// A per-feature or per-model limit (e.g. a separate bucket for
/// Codex-Spark), metered apart from the account-wide windows.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct ExtraLimit {
    pub name: Option<String>,
    pub feature: Option<String>,
    /// The model whose ordinary limit this one stands in for.
    pub model: Option<String>,
    pub limit: Option<RateLimit>,
}

impl ExtraLimit {
    /// Loose match on the name, the metered feature or the model, so a
    /// caller can say "spark".
    pub fn matches(&self, needle: &str) -> bool {
        let needle = needle.to_ascii_lowercase();
        [&self.name, &self.feature, &self.model]
            .into_iter()
            .flatten()
            .any(|text| text.to_ascii_lowercase().contains(&needle))
    }

    pub fn is_limited(&self) -> bool {
        self.limit.as_ref().is_some_and(|limit| !limit.allows())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Credits {
    pub has_credits: Option<bool>,
    pub unlimited: Option<bool>,
    pub overage_limit_reached: Option<bool>,
    /// The backend sends `null`, `"0"` or a number here; kept as text.
    pub balance: Option<String>,
}

/// Whether the account may use one model right now (`model_usage`).
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct ModelUsage {
    pub slug: String,
    pub available: Option<bool>,
    /// Epoch seconds when the model becomes available again.
    pub available_at: Option<i64>,
    /// Buying credits would make the model available.
    pub credits_would_enable: Option<bool>,
}

impl ModelUsage {
    pub fn is_unavailable(&self) -> bool {
        self.available == Some(false)
    }
}

/// The workspace's cap on credit spending (`spend_control`). The amounts are
/// decimal strings, kept as sent.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct SpendControl {
    pub reached: Option<bool>,
    pub limit: Option<SpendLimit>,
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct SpendLimit {
    pub limit: Option<String>,
    pub used: Option<String>,
    pub remaining: Option<String>,
    pub used_percent: Option<f64>,
    /// Epoch seconds.
    pub reset_at: Option<i64>,
    pub source: Option<String>,
}

/// Reset credits held by the account, as the usage body counts them.
/// `applicable` is how many would clear a limit the account is at now.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct ResetCreditCount {
    pub available: i64,
    pub applicable: Option<i64>,
}

/// What the usage endpoint said about an account.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct Usage {
    pub user_id: Option<String>,
    pub account_id: Option<String>,
    pub email: Option<String>,
    pub plan: Option<Plan>,
    pub limit: Option<RateLimit>,
    pub credits: Option<Credits>,
    /// The backend's name for the limit it is enforcing, when it is.
    pub limit_reason: Option<String>,
    pub extra: Vec<ExtraLimit>,
    /// Per-model availability, by slug.
    pub models: Vec<ModelUsage>,
    pub spend_control: Option<SpendControl>,
    pub reset_credits: Option<ResetCreditCount>,
}

/// Why a probe produced no usage.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ProbeError {
    /// auth.json is missing or unreadable.
    AuthFile {
        message: String,
    },
    AuthFileHasNoAccessToken,
    Network {
        message: String,
    },
    /// Every attempt was answered by the edge, not the API.
    Challenged {
        status: u16,
        attempts: u32,
    },
    Http {
        status: u16,
        body: String,
    },
    Decode {
        message: String,
    },
}

impl ProbeError {
    /// The backend rejected the token: the caller should refresh.
    pub fn is_unauthorized(&self) -> bool {
        matches!(self, Self::Http { status: 401, .. })
    }
}

impl fmt::Display for ProbeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::AuthFile { message } => f.write_str(message),
            Self::AuthFileHasNoAccessToken => f.write_str("auth.json has no access_token"),
            Self::Network { message } => f.write_str(message),
            Self::Challenged { status, attempts } => write!(
                f,
                "HTTP {status}: Cloudflare challenge after {attempts} attempts, not an auth failure"
            ),
            Self::Http { status, body } => write!(f, "HTTP {status}: {body}"),
            Self::Decode { message } => f.write_str(message),
        }
    }
}

/// Whether an account can take work right now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum State {
    Ok,
    Limited { reason: Option<String> },
    Error { error: ProbeError },
}

impl fmt::Display for State {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Ok => f.write_str("ok"),
            Self::Limited {
                reason: Some(reason),
            } => write!(f, "limited ({reason})"),
            Self::Limited { reason: None } => f.write_str("limited"),
            Self::Error { error } if error.is_unauthorized() => {
                write!(f, "ERROR: {error}, needs `tokeman codex refresh`")
            }
            Self::Error { error } => write!(f, "ERROR: {error}"),
        }
    }
}

/// One probe of one account. Local signals are filled even when the request
/// fails.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Probe {
    pub account: String,
    pub codex_home: PathBuf,
    /// Epoch seconds.
    pub at: i64,
    pub usage: Option<Usage>,
    pub error: Option<ProbeError>,
    pub last_refresh: Option<DateTime<Utc>>,
    pub email_hint: Option<String>,
}

impl Probe {
    fn new(
        account: &CodexAccount,
        auth: Option<&AuthFile>,
        outcome: Result<Usage, ProbeError>,
        at: DateTime<Utc>,
    ) -> Self {
        let (usage, error) = match outcome {
            Ok(usage) => (Some(usage), None),
            Err(error) => (None, Some(error)),
        };
        Self {
            account: account.name.clone(),
            codex_home: account.home(),
            at: at.timestamp(),
            usage,
            error,
            last_refresh: auth.and_then(AuthFile::last_refresh),
            email_hint: auth
                .and_then(AuthFile::claims)
                .and_then(|claims| claims.email),
        }
    }

    pub fn limit(&self) -> Option<&RateLimit> {
        self.usage.as_ref()?.limit.as_ref()
    }

    /// The account-wide window of this length.
    pub fn window(&self, span: Span) -> Option<Window> {
        self.limit()?.window(span)
    }

    /// Account-wide windows of a length other than 5h or weekly.
    pub fn other_windows(&self) -> impl Iterator<Item = &SpanWindow> {
        self.limit()
            .into_iter()
            .flat_map(|limit| &limit.windows)
            .filter(|window| !matches!(window.span, Span::FiveHour | Span::Weekly))
    }

    /// Least remaining across the reported windows. `None` is no reading,
    /// which differs from no headroom.
    pub fn remaining(&self) -> Option<f64> {
        self.limit()?.remaining()
    }

    /// Headroom in the named per-model bucket.
    pub fn remaining_for(&self, limit: &str) -> Option<f64> {
        self.extra()
            .iter()
            .find(|extra| extra.matches(limit))?
            .limit
            .as_ref()?
            .remaining()
    }

    pub fn extra(&self) -> &[ExtraLimit] {
        self.usage
            .as_ref()
            .map(|usage| usage.extra.as_slice())
            .unwrap_or(&[])
    }

    pub fn models(&self) -> &[ModelUsage] {
        self.usage
            .as_ref()
            .map(|usage| usage.models.as_slice())
            .unwrap_or(&[])
    }

    pub fn spend_control(&self) -> Option<&SpendControl> {
        self.usage.as_ref()?.spend_control.as_ref()
    }

    pub fn reset_credits(&self) -> Option<ResetCreditCount> {
        self.usage.as_ref()?.reset_credits
    }

    /// Whether the backend would accept work on this account. A 200 with no
    /// limit block means no limit is being enforced.
    pub fn is_viable(&self) -> bool {
        match (&self.usage, self.limit()) {
            (None, _) => false,
            (Some(_), None) => true,
            (Some(_), Some(limit)) => limit.allows(),
        }
    }

    pub fn is_unauthorized(&self) -> bool {
        self.error.as_ref().is_some_and(ProbeError::is_unauthorized)
    }

    pub fn state(&self) -> State {
        if let Some(error) = &self.error {
            return State::Error {
                error: error.clone(),
            };
        }
        if self.is_viable() {
            State::Ok
        } else {
            State::Limited {
                reason: self.limit_reason().map(str::to_owned),
            }
        }
    }

    pub fn limit_reason(&self) -> Option<&str> {
        self.usage.as_ref()?.limit_reason.as_deref()
    }

    pub fn email(&self) -> Option<&str> {
        self.usage
            .as_ref()
            .and_then(|usage| usage.email.as_deref())
            .or(self.email_hint.as_deref())
    }

    pub fn plan(&self) -> Option<&Plan> {
        self.usage.as_ref()?.plan.as_ref()
    }
}

// The endpoint's shapes, as sent. Every field is optional: this is not a
// stable contract.

#[derive(Deserialize, Default)]
struct RawWindow {
    used_percent: Option<f64>,
    limit_window_seconds: Option<i64>,
    reset_after_seconds: Option<i64>,
    reset_at: Option<i64>,
}

impl RawWindow {
    fn window(&self, at: DateTime<Utc>) -> Option<SpanWindow> {
        let used = self.used_percent? / 100.0;
        let resets_at = self
            .reset_at
            .or_else(|| self.reset_after_seconds.map(|after| at.timestamp() + after))
            .unwrap_or(0);
        Some(SpanWindow {
            span: Span::from_seconds(self.limit_window_seconds),
            window: Window::new(used, resets_at),
        })
    }
}

#[derive(Deserialize, Default)]
struct RawRateLimit {
    allowed: Option<bool>,
    limit_reached: Option<bool>,
    primary_window: Option<RawWindow>,
    secondary_window: Option<RawWindow>,
}

impl RawRateLimit {
    fn limit(&self, at: DateTime<Utc>) -> RateLimit {
        RateLimit {
            allowed: self.allowed,
            limit_reached: self.limit_reached,
            windows: [&self.primary_window, &self.secondary_window]
                .into_iter()
                .flatten()
                .filter_map(|window| window.window(at))
                .collect(),
        }
    }
}

#[derive(Deserialize, Default)]
struct RawCredits {
    has_credits: Option<bool>,
    unlimited: Option<bool>,
    overage_limit_reached: Option<bool>,
    balance: Option<Value>,
}

#[derive(Deserialize, Default)]
struct RawExtraLimit {
    limit_name: Option<String>,
    metered_feature: Option<String>,
    normal_model_slug: Option<String>,
    rate_limit: Option<RawRateLimit>,
}

#[derive(Deserialize, Default)]
struct RawReachedType {
    #[serde(rename = "type")]
    kind: Option<String>,
}

#[derive(Deserialize, Default)]
struct RawModelUsage {
    available: Option<bool>,
    available_at: Option<Value>,
    credits_would_enable: Option<bool>,
}

#[derive(Deserialize, Default)]
struct RawSpendLimit {
    source: Option<String>,
    limit: Option<Value>,
    used: Option<Value>,
    remaining: Option<Value>,
    used_percent: Option<f64>,
    reset_at: Option<i64>,
}

#[derive(Deserialize, Default)]
struct RawSpendControl {
    reached: Option<bool>,
    individual_limit: Option<RawSpendLimit>,
}

#[derive(Deserialize, Default)]
struct RawResetCredits {
    available_count: Option<i64>,
    applicable_available_count: Option<i64>,
}

/// A number or string the backend may send either way, as text.
fn text_of(value: Option<Value>) -> Option<String> {
    match value? {
        Value::Null => None,
        Value::String(text) => Some(text),
        other => Some(other.to_string()),
    }
}

/// Epoch seconds from a number or an RFC 3339 string.
pub(crate) fn epoch_of(value: Option<&Value>) -> Option<i64> {
    match value? {
        Value::Number(number) => number.as_i64(),
        Value::String(text) => DateTime::parse_from_rfc3339(text)
            .ok()
            .map(|at| at.timestamp()),
        _ => None,
    }
}

/// Decode a usage body field by field. `rate_limit` is what rotation rests
/// on, so a malformed one fails the decode; any other field that changed
/// shape is dropped alone, so a cosmetic change does not park a working
/// account or hide the rest of the body.
pub fn decode_usage(body: &str, at: DateTime<Utc>) -> Result<Usage, ProbeError> {
    let value: Value = serde_json::from_str(body).map_err(|error| ProbeError::Decode {
        message: format!("failed to parse usage response as JSON: {error}"),
    })?;
    let Some(object) = value.as_object() else {
        return Err(ProbeError::Decode {
            message: "usage response is not a JSON object".to_owned(),
        });
    };
    fn field<T: serde::de::DeserializeOwned>(
        object: &serde_json::Map<String, Value>,
        key: &str,
    ) -> Option<T> {
        serde_json::from_value(object.get(key)?.clone()).ok()
    }
    let rate_limit = match object.get("rate_limit") {
        None | Some(Value::Null) => None,
        Some(raw) => Some(serde_json::from_value::<RawRateLimit>(raw.clone()).map_err(
            |error| ProbeError::Decode {
                message: format!("failed to decode usage rate_limit: {error}"),
            },
        )?),
    };
    let credits: Option<RawCredits> = field(object, "credits");
    let extra: Vec<RawExtraLimit> = field(object, "additional_rate_limits").unwrap_or_default();
    let models: std::collections::BTreeMap<String, RawModelUsage> =
        field(object, "model_usage").unwrap_or_default();
    let spend: Option<RawSpendControl> = field(object, "spend_control");
    let resets: Option<RawResetCredits> = field(object, "rate_limit_reset_credits");
    let reached: Option<RawReachedType> = field(object, "rate_limit_reached_type");
    Ok(Usage {
        user_id: field(object, "user_id"),
        account_id: field(object, "account_id"),
        email: field(object, "email"),
        plan: field::<String>(object, "plan_type")
            .as_deref()
            .map(Plan::parse),
        limit: rate_limit.as_ref().map(|limit| limit.limit(at)),
        credits: credits.map(|credits| Credits {
            has_credits: credits.has_credits,
            unlimited: credits.unlimited,
            overage_limit_reached: credits.overage_limit_reached,
            balance: text_of(credits.balance),
        }),
        limit_reason: reached.and_then(|reached| reached.kind),
        extra: extra
            .into_iter()
            .map(|extra| ExtraLimit {
                name: extra.limit_name,
                feature: extra.metered_feature,
                model: extra.normal_model_slug,
                limit: extra.rate_limit.as_ref().map(|limit| limit.limit(at)),
            })
            .collect(),
        models: models
            .into_iter()
            .map(|(slug, model)| ModelUsage {
                slug,
                available: model.available,
                available_at: epoch_of(model.available_at.as_ref()),
                credits_would_enable: model.credits_would_enable,
            })
            .collect(),
        spend_control: spend.map(|spend| SpendControl {
            reached: spend.reached,
            limit: spend.individual_limit.map(|limit| SpendLimit {
                limit: text_of(limit.limit),
                used: text_of(limit.used),
                remaining: text_of(limit.remaining),
                used_percent: limit.used_percent,
                reset_at: limit.reset_at,
                source: limit.source,
            }),
        }),
        reset_credits: resets.and_then(|resets| {
            Some(ResetCreditCount {
                available: resets.available_count?,
                applicable: resets.applicable_available_count,
            })
        }),
    })
}

/// One request to the ChatGPT backend as Codex makes it, retried through
/// edge challenges; the body of a 2xx answer. A POST is retried too, so a
/// caller sending one must make it idempotent.
pub(crate) async fn call(
    client: &Client,
    account: &str,
    auth: &AuthFile,
    method: reqwest::Method,
    url: &str,
    json: Option<&Value>,
) -> Result<String, ProbeError> {
    let access_token = auth
        .access_token()
        .ok_or(ProbeError::AuthFileHasNoAccessToken)?;
    let account_id = auth.effective_account_id();
    let mut attempt = 0;
    let response = loop {
        let mut request = client
            .http()
            .request(method.clone(), url)
            .bearer_auth(access_token)
            .header("originator", ORIGINATOR)
            .header("User-Agent", "codex-cli");
        if let Some(account_id) = &account_id {
            request = request.header("chatgpt-account-id", account_id);
        }
        if let Some(json) = json {
            request = request.json(json);
        }
        let response = request.send().await.map_err(|error| ProbeError::Network {
            message: error.to_string(),
        })?;
        if is_edge_challenge(&response) && attempt < CHALLENGE_RETRIES {
            attempt += 1;
            tokio::time::sleep(Duration::from_millis(400 * u64::from(attempt))).await;
            continue;
        }
        break response;
    };

    let status = response.status();
    if !status.is_success() {
        let challenged = is_edge_challenge(&response);
        if std::env::var_os("TOKEMAN_CODEX_DEBUG").is_some() {
            eprintln!(
                "--- {account} {method} {url} {status} {:?} (attempts: {})",
                response.version(),
                attempt + 1
            );
            for (name, value) in response.headers() {
                eprintln!("    {name}: {}", value.to_str().unwrap_or("<binary>"));
            }
        }
        let body = response.text().await.unwrap_or_default();
        return Err(if challenged {
            ProbeError::Challenged {
                status: status.as_u16(),
                attempts: attempt + 1,
            }
        } else {
            ProbeError::Http {
                status: status.as_u16(),
                body: super::one_line(&body, 160),
            }
        });
    }
    response.text().await.map_err(|error| ProbeError::Network {
        message: error.to_string(),
    })
}

async fn fetch_usage(
    client: &Client,
    account: &str,
    auth: &AuthFile,
    at: DateTime<Utc>,
) -> Result<Usage, ProbeError> {
    let body = call(
        client,
        account,
        auth,
        reqwest::Method::GET,
        client.usage_url(),
        None,
    )
    .await?;
    decode_usage(&body, at)
}

/// Probe one account. A dead account is data, not an error.
pub async fn probe(client: &Client, account: &CodexAccount) -> Probe {
    let at = Utc::now();
    match CodexHome::new(account.home()).read() {
        Ok(auth) => {
            let outcome = fetch_usage(client, &account.name, &auth, at).await;
            Probe::new(account, Some(&auth), outcome, at)
        }
        Err(error) => Probe::new(
            account,
            None,
            Err(ProbeError::AuthFile {
                message: format!("{error:#}"),
            }),
            at,
        ),
    }
}

/// Probe every account through one client. The first request goes alone so
/// a challenge settles and seeds the cookie jar; the rest run concurrently.
pub async fn probe_all(client: &Client, accounts: &[CodexAccount]) -> Vec<Probe> {
    let Some((first, rest)) = accounts.split_first() else {
        return Vec::new();
    };
    let mut probes = Vec::with_capacity(accounts.len());
    probes.push(probe(client, first).await);
    probes
        .extend(futures::future::join_all(rest.iter().map(|account| probe(client, account))).await);
    probes
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::codex::auth::TokenBundle;
    use crate::codex::auth::tests::{bundle, id_token};
    use crate::testing::{codex_account, serve};
    use axum::Router;
    use axum::body::Bytes;
    use axum::extract::State as AxumState;
    use axum::http::{HeaderMap, StatusCode};
    use axum::response::IntoResponse;
    use axum::routing::{get, post};
    use serde_json::json;
    use std::sync::{Arc, Mutex};

    /// One request as the fake backend saw it.
    #[derive(Debug, Clone)]
    pub(crate) struct Seen {
        pub path: String,
        pub headers: HeaderMap,
        pub body: String,
    }

    /// A fake backend: scripted usage answers in order (the last one repeats),
    /// scripted token answers likewise, and a log of what it saw.
    #[derive(Clone, Default)]
    pub(crate) struct Backend {
        pub usage: Arc<Mutex<Vec<(StatusCode, HeaderMap, String)>>>,
        pub token: Arc<Mutex<Vec<(StatusCode, String)>>>,
        pub credits: Arc<Mutex<Vec<(StatusCode, String)>>>,
        pub consume: Arc<Mutex<Vec<(StatusCode, String)>>>,
        pub seen: Arc<Mutex<Vec<Seen>>>,
    }

    impl Backend {
        pub fn usage_answers(self, answers: Vec<(StatusCode, HeaderMap, String)>) -> Self {
            *self.usage.lock().unwrap() = answers;
            self
        }

        pub fn token_answers(self, answers: Vec<(StatusCode, String)>) -> Self {
            *self.token.lock().unwrap() = answers;
            self
        }

        pub fn credit_answers(self, answers: Vec<(StatusCode, String)>) -> Self {
            *self.credits.lock().unwrap() = answers;
            self
        }

        pub fn consume_answers(self, answers: Vec<(StatusCode, String)>) -> Self {
            *self.consume.lock().unwrap() = answers;
            self
        }

        pub fn requests(&self, path: &str) -> Vec<Seen> {
            self.seen
                .lock()
                .unwrap()
                .iter()
                .filter(|seen| seen.path == path)
                .cloned()
                .collect()
        }

        /// Serve it and build a client pointed at it.
        pub async fn client(&self) -> Client {
            let app = Router::new()
                .route("/usage", get(usage_handler))
                .route("/token", post(token_handler))
                .route("/credits", get(credits_handler))
                .route("/credits/consume", post(consume_handler))
                .with_state(self.clone());
            let base = serve(app).await;
            Client::with_endpoints(
                format!("{base}/usage"),
                format!("{base}/token"),
                "test-client",
            )
            .unwrap()
            .with_credits_url(format!("{base}/credits"))
        }
    }

    fn take<T: Clone>(queue: &Mutex<Vec<T>>) -> Option<T> {
        let mut queue = queue.lock().unwrap();
        if queue.len() > 1 {
            Some(queue.remove(0))
        } else {
            queue.first().cloned()
        }
    }

    async fn usage_handler(
        AxumState(backend): AxumState<Backend>,
        headers: HeaderMap,
    ) -> axum::response::Response {
        backend.seen.lock().unwrap().push(Seen {
            path: "/usage".into(),
            headers,
            body: String::new(),
        });
        match take(&backend.usage) {
            Some((status, headers, body)) => (status, headers, body).into_response(),
            None => StatusCode::NOT_IMPLEMENTED.into_response(),
        }
    }

    fn answer(
        backend: &Backend,
        path: &str,
        queue: &Mutex<Vec<(StatusCode, String)>>,
        headers: HeaderMap,
        body: Bytes,
    ) -> axum::response::Response {
        backend.seen.lock().unwrap().push(Seen {
            path: path.into(),
            headers,
            body: String::from_utf8_lossy(&body).into_owned(),
        });
        match take(queue) {
            Some((status, body)) => (status, body).into_response(),
            None => StatusCode::NOT_IMPLEMENTED.into_response(),
        }
    }

    async fn token_handler(
        AxumState(backend): AxumState<Backend>,
        headers: HeaderMap,
        body: Bytes,
    ) -> axum::response::Response {
        answer(&backend, "/token", &backend.token, headers, body)
    }

    async fn credits_handler(
        AxumState(backend): AxumState<Backend>,
        headers: HeaderMap,
    ) -> axum::response::Response {
        answer(
            &backend,
            "/credits",
            &backend.credits,
            headers,
            Bytes::new(),
        )
    }

    async fn consume_handler(
        AxumState(backend): AxumState<Backend>,
        headers: HeaderMap,
        body: Bytes,
    ) -> axum::response::Response {
        answer(
            &backend,
            "/credits/consume",
            &backend.consume,
            headers,
            body,
        )
    }

    pub(crate) fn ok(body: Value) -> (StatusCode, HeaderMap, String) {
        let mut headers = HeaderMap::new();
        headers.insert("content-type", "application/json".parse().unwrap());
        (StatusCode::OK, headers, body.to_string())
    }

    /// A bare answer with no headers.
    pub(crate) fn status(code: StatusCode, body: &str) -> (StatusCode, HeaderMap, String) {
        (code, HeaderMap::new(), body.into())
    }

    pub(crate) fn challenge() -> (StatusCode, HeaderMap, String) {
        let mut headers = HeaderMap::new();
        headers.insert("cf-mitigated", "challenge".parse().unwrap());
        headers.insert("content-type", "text/html".parse().unwrap());
        headers.insert("set-cookie", "__cf_bm=jar; Path=/".parse().unwrap());
        (
            StatusCode::FORBIDDEN,
            headers,
            "<html>challenge</html>".into(),
        )
    }

    pub(crate) fn usage_body(used_primary: f64, used_secondary: f64) -> Value {
        json!({
            "user_id": "u1",
            "account_id": "acct-1",
            "email": "a@b.c",
            "plan_type": "plus",
            "rate_limit": {
                "allowed": true,
                "limit_reached": false,
                "primary_window": {"used_percent": used_primary, "limit_window_seconds": 18000, "reset_after_seconds": 3600},
                "secondary_window": {"used_percent": used_secondary, "limit_window_seconds": 604800, "reset_at": 1_800_000_000},
            },
            "credits": {"has_credits": false, "unlimited": false, "balance": "0"},
            "rate_limit_reached_type": null,
            "additional_rate_limits": [
                {"limit_name": "codex_spark", "metered_feature": "gpt-5.3-codex-spark",
                 "rate_limit": {"allowed": false, "limit_reached": true, "primary_window": {"used_percent": 100.0, "reset_after_seconds": 100}}}
            ],
        })
    }

    /// The body `/wham/usage` sent on 2026-11-11 (ids changed): the weekly
    /// window as `primary_window`, no 5h window, and the newer fields.
    pub(crate) fn weekly_only_body(used: f64) -> Value {
        json!({
            "user_id": "u1",
            "account_id": "acct-1",
            "email": "a@b.c",
            "plan_type": "promax",
            "rate_limit": {
                "allowed": true,
                "limit_reached": false,
                "primary_window": {"used_percent": used, "limit_window_seconds": 604800,
                                   "reset_after_seconds": 172920, "reset_at": 1_794_607_293},
                "secondary_window": null
            },
            "code_review_rate_limit": null,
            "additional_rate_limits": null,
            "model_usage": {"gpt-6-astra": {"available": true, "available_at": null, "credits_would_enable": false}},
            "credits": {"has_credits": false, "unlimited": false, "overage_limit_reached": false, "balance": "0",
                        "approx_local_messages": [0, 0], "approx_cloud_messages": [0, 0]},
            "spend_control": {"reached": false, "individual_limit": null},
            "rate_limit_reached_type": null,
            "promo": null,
            "rate_limit_reset_credits": {"available_count": 2, "applicable_available_count": 0}
        })
    }

    /// The weekly-only shape at its limit, with one reset credit that applies.
    pub(crate) fn limited_body() -> Value {
        let mut body = weekly_only_body(100.0);
        body["rate_limit"]["allowed"] = json!(false);
        body["rate_limit"]["limit_reached"] = json!(true);
        body["rate_limit_reached_type"] = json!({"type": "rate_limit_reached"});
        body["rate_limit_reset_credits"]["applicable_available_count"] = json!(1);
        body
    }

    /// A profile directory with a seated bundle; returns the account.
    pub(crate) fn profile(dir: &std::path::Path, name: &str, refresh_token: &str) -> CodexAccount {
        let home = CodexHome::new(dir.join(name));
        home.write_tokens(&TokenBundle {
            id_token: Some(id_token(
                &format!("{name}@x.y"),
                &format!("acct-{name}"),
                "plus",
            )),
            ..bundle(&format!("access-{name}"), refresh_token)
        })
        .unwrap();
        codex_account(name, &home.path().display().to_string())
    }

    #[test]
    fn a_full_body_decodes_to_typed_usage() {
        let at = DateTime::from_timestamp(1_000, 0).unwrap();
        let usage = decode_usage(&usage_body(40.0, 75.0).to_string(), at).unwrap();
        assert_eq!(usage.plan, Some(Plan::Plus));
        let limit = usage.limit.as_ref().unwrap();
        assert_eq!(limit.window(Span::FiveHour), Some(Window::new(0.4, 4_600)));
        assert_eq!(
            limit.window(Span::Weekly),
            Some(Window::new(0.75, 1_800_000_000))
        );
        assert_eq!(limit.remaining(), Some(0.25));
        assert_eq!(limit.tightest().unwrap().span, Span::Weekly);
        assert!(limit.allows());
        assert_eq!(
            usage.credits.as_ref().unwrap().balance.as_deref(),
            Some("0")
        );
        assert_eq!(usage.limit_reason, None);
        assert_eq!(usage.extra.len(), 1);
        assert!(usage.extra[0].matches("SPARK"));
        assert!(usage.extra[0].is_limited());
        assert_eq!(
            usage.extra[0].limit.as_ref().unwrap().remaining(),
            Some(0.0)
        );
        assert_eq!(
            usage.extra[0].limit.as_ref().unwrap().windows[0].span,
            Span::Unknown,
            "a window without limit_window_seconds is not guessed from its position"
        );
        assert!(usage.models.is_empty());
        assert_eq!(usage.reset_credits, None);
    }

    #[test]
    fn todays_weekly_only_body_names_its_one_window_weekly() {
        let at = DateTime::from_timestamp(1_000, 0).unwrap();
        let usage = decode_usage(&weekly_only_body(42.0).to_string(), at).unwrap();
        assert_eq!(usage.plan, Some(Plan::Other("promax".into())));
        let limit = usage.limit.as_ref().unwrap();
        assert_eq!(limit.window(Span::FiveHour), None);
        assert_eq!(
            limit.window(Span::Weekly),
            Some(Window::new(0.42, 1_794_607_293))
        );
        assert_eq!(limit.windows.len(), 1);
        assert!((limit.remaining().unwrap() - 0.58).abs() < 1e-9);
        assert!(usage.extra.is_empty(), "additional_rate_limits: null");
        assert_eq!(
            usage.models,
            vec![ModelUsage {
                slug: "gpt-6-astra".into(),
                available: Some(true),
                available_at: None,
                credits_would_enable: Some(false),
            }]
        );
        assert_eq!(
            usage.spend_control,
            Some(SpendControl {
                reached: Some(false),
                limit: None
            })
        );
        assert_eq!(
            usage.reset_credits,
            Some(ResetCreditCount {
                available: 2,
                applicable: Some(0)
            })
        );
        assert_eq!(
            usage.credits.as_ref().unwrap().balance.as_deref(),
            Some("0")
        );
    }

    #[test]
    fn newer_fields_decode_with_their_alternative_spellings() {
        let at = DateTime::from_timestamp(0, 0).unwrap();
        let mut body = weekly_only_body(10.0);
        body["rate_limit"]["secondary_window"] =
            json!({"used_percent": 50, "limit_window_seconds": 86400, "reset_at": 99});
        body["model_usage"] = json!({
            "b-model": {"available": false, "available_at": "2026-11-12T00:00:00Z", "credits_would_enable": true},
            "a-model": {"available": false, "available_at": 1_794_000_000},
        });
        body["spend_control"] = json!({"reached": true, "individual_limit": {
            "source": "workspace", "limit": "50.00", "used": 50, "remaining": "0",
            "used_percent": 100, "remaining_percent": 0, "reset_after_seconds": 10, "reset_at": 1_794_000_000}});
        body["additional_rate_limits"] = json!([{"limit_name": "spark", "metered_feature": "codex_spark",
            "normal_model_slug": "gpt-6-astra-spark",
            "rate_limit": {"allowed": true, "limit_reached": false,
                           "primary_window": {"used_percent": 20, "limit_window_seconds": 18000}}}]);
        let usage = decode_usage(&body.to_string(), at).unwrap();
        let limit = usage.limit.as_ref().unwrap();
        assert_eq!(
            limit.windows.iter().map(|w| w.span).collect::<Vec<_>>(),
            vec![Span::Weekly, Span::Seconds(86_400)]
        );
        assert_eq!(limit.tightest().unwrap().span, Span::Seconds(86_400));
        assert_eq!(
            usage
                .models
                .iter()
                .map(|m| (m.slug.as_str(), m.available_at))
                .collect::<Vec<_>>(),
            vec![
                ("a-model", Some(1_794_000_000)),
                ("b-model", Some(1_794_441_600))
            ]
        );
        assert!(usage.models.iter().all(ModelUsage::is_unavailable));
        let spend = usage.spend_control.unwrap();
        assert_eq!(spend.reached, Some(true));
        let cap = spend.limit.unwrap();
        assert_eq!(cap.limit.as_deref(), Some("50.00"));
        assert_eq!(cap.used.as_deref(), Some("50"));
        assert_eq!(cap.used_percent, Some(100.0));
        assert_eq!(usage.extra[0].model.as_deref(), Some("gpt-6-astra-spark"));
        assert!(usage.extra[0].matches("astra-spark"));
        assert_eq!(
            usage.extra[0]
                .limit
                .as_ref()
                .unwrap()
                .window(Span::FiveHour),
            Some(Window::new(0.2, 0))
        );
    }

    #[test]
    fn spans_are_named_by_length() {
        assert_eq!(Span::from_seconds(Some(18_000)), Span::FiveHour);
        assert_eq!(Span::from_seconds(Some(604_800)), Span::Weekly);
        assert_eq!(Span::from_seconds(None), Span::Unknown);
        assert_eq!(Span::from_seconds(Some(0)), Span::Unknown);
        assert_eq!(Span::FiveHour.label(), "5h");
        assert_eq!(Span::Weekly.label(), "weekly");
        assert_eq!(Span::Seconds(86_400).label(), "1d");
        assert_eq!(Span::Seconds(2_592_000).label(), "30d");
        assert_eq!(Span::Seconds(5_400).label(), "90m");
        assert_eq!(Span::Seconds(3_600).label(), "1h");
        assert_eq!(Span::Seconds(61).label(), "61s");
        assert_eq!(Span::Unknown.to_string(), "window");
    }

    #[test]
    fn a_cosmetic_shape_change_falls_back_to_the_rate_limit_subtree() {
        let at = DateTime::from_timestamp(0, 0).unwrap();
        let mut body = usage_body(10.0, 20.0);
        body["additional_rate_limits"] = json!("surprise");
        body["rate_limit_reached_type"] = json!({"type": "secondary_window"});
        let usage = decode_usage(&body.to_string(), at).unwrap();
        assert_eq!(usage.email.as_deref(), Some("a@b.c"));
        assert_eq!(usage.limit.as_ref().unwrap().remaining(), Some(0.8));
        assert_eq!(usage.limit_reason.as_deref(), Some("secondary_window"));
        assert!(usage.extra.is_empty());
        assert_eq!(
            usage.credits.as_ref().unwrap().balance.as_deref(),
            Some("0"),
            "a broken field does not take its neighbours with it"
        );
        assert!(matches!(
            decode_usage("[1, 2]", at),
            Err(ProbeError::Decode { .. })
        ));
        assert!(matches!(
            decode_usage(r#"{"rate_limit": 5}"#, at),
            Err(ProbeError::Decode { .. })
        ));
        assert!(matches!(
            decode_usage("not json", at),
            Err(ProbeError::Decode { .. })
        ));
    }

    #[test]
    fn plans_keep_unknown_spellings_and_round_trip() {
        assert_eq!(Plan::parse("Pro"), Plan::Pro);
        assert_eq!(Plan::parse("team-plus"), Plan::Other("team-plus".into()));
        let json = serde_json::to_string(&Plan::Other("odd".into())).unwrap();
        assert_eq!(json, r#""odd""#);
        assert_eq!(
            serde_json::from_str::<Plan>(r#""enterprise""#).unwrap(),
            Plan::Enterprise
        );
    }

    #[test]
    fn viability_follows_the_limit_block() {
        let account = codex_account("a", "/nowhere");
        let at = Utc::now();
        let with = |limit: Option<RateLimit>, reason: Option<&str>| {
            Probe::new(
                &account,
                None,
                Ok(Usage {
                    limit,
                    limit_reason: reason.map(str::to_owned),
                    ..Usage::default()
                }),
                at,
            )
        };
        assert!(
            with(None, None).is_viable(),
            "no limit block: nothing enforced"
        );
        assert_eq!(with(None, None).state(), State::Ok);
        let reached = with(
            Some(RateLimit {
                limit_reached: Some(true),
                ..RateLimit::default()
            }),
            Some("primary_window"),
        );
        assert!(!reached.is_viable());
        assert_eq!(reached.state().to_string(), "limited (primary_window)");
        let refused = with(
            Some(RateLimit {
                allowed: Some(false),
                ..RateLimit::default()
            }),
            None,
        );
        assert_eq!(refused.state().to_string(), "limited");
        let failed = Probe::new(
            &account,
            None,
            Err(ProbeError::Http {
                status: 401,
                body: "nope".into(),
            }),
            at,
        );
        assert!(failed.is_unauthorized());
        assert!(!failed.is_viable());
        assert_eq!(
            failed.state().to_string(),
            "ERROR: HTTP 401: nope, needs `tokeman codex refresh`"
        );
    }

    #[tokio::test]
    async fn a_probe_sends_codex_identity_and_reads_typed_usage() {
        let dir = tempfile::tempdir().unwrap();
        let account = profile(dir.path(), "one", "r1");
        let backend = Backend::default().usage_answers(vec![ok(usage_body(30.0, 60.0))]);
        let client = backend.client().await;
        let probe = probe(&client, &account).await;
        assert_eq!(probe.error, None);
        assert_eq!(probe.remaining(), Some(0.4));
        assert_eq!(probe.remaining_for("spark"), Some(0.0));
        assert_eq!(probe.remaining_for("nothing"), None);
        assert_eq!(probe.email(), Some("a@b.c"));
        assert_eq!(probe.plan(), Some(&Plan::Plus));
        assert!(probe.last_refresh.is_some());
        let seen = backend.requests("/usage");
        assert_eq!(seen.len(), 1);
        let header = |name: &str| {
            seen[0]
                .headers
                .get(name)
                .unwrap()
                .to_str()
                .unwrap()
                .to_owned()
        };
        assert_eq!(header("authorization"), "Bearer access-one");
        assert_eq!(header("originator"), ORIGINATOR);
        assert_eq!(header("user-agent"), "codex-cli");
        assert_eq!(header("chatgpt-account-id"), "acct-one");
    }

    #[tokio::test]
    async fn a_challenge_is_retried_with_the_cookie_and_a_401_is_unauthorized() {
        let dir = tempfile::tempdir().unwrap();
        let first = profile(dir.path(), "first", "r1");
        let second = profile(dir.path(), "second", "r2");
        let backend = Backend::default().usage_answers(vec![
            challenge(),
            ok(usage_body(0.0, 0.0)),
            status(StatusCode::UNAUTHORIZED, r#"{"detail": "token  expired"}"#),
        ]);
        let client = backend.client().await;
        let probes = probe_all(&client, &[first, second]).await;
        assert_eq!(probes[0].error, None);
        assert_eq!(probes[0].remaining(), Some(1.0));
        assert_eq!(
            probes[1].error,
            Some(ProbeError::Http {
                status: 401,
                body: r#"{"detail": "token expired"}"#.into()
            })
        );
        assert!(probes[1].is_unauthorized());
        assert_eq!(
            probes[1].email(),
            Some("second@x.y"),
            "the id_token still names the account"
        );
        let seen = backend.requests("/usage");
        assert_eq!(seen.len(), 3);
        assert!(seen[0].headers.get("cookie").is_none());
        for later in &seen[1..] {
            assert!(
                later
                    .headers
                    .get("cookie")
                    .unwrap()
                    .to_str()
                    .unwrap()
                    .contains("__cf_bm=jar"),
                "every request after the challenge replays the jar"
            );
        }
    }

    #[tokio::test]
    async fn a_standing_challenge_is_not_an_auth_failure() {
        let dir = tempfile::tempdir().unwrap();
        let account = profile(dir.path(), "one", "r1");
        let backend = Backend::default().usage_answers(vec![challenge()]);
        let client = backend.client().await;
        let probe = probe(&client, &account).await;
        assert_eq!(
            probe.error,
            Some(ProbeError::Challenged {
                status: 403,
                attempts: CHALLENGE_RETRIES + 1
            })
        );
        assert!(!probe.is_unauthorized());
        assert_eq!(
            backend.requests("/usage").len(),
            (CHALLENGE_RETRIES + 1) as usize
        );
    }

    #[tokio::test]
    async fn local_failures_make_no_request() {
        let dir = tempfile::tempdir().unwrap();
        let backend = Backend::default().usage_answers(vec![ok(usage_body(0.0, 0.0))]);
        let client = backend.client().await;
        let missing = codex_account("missing", &dir.path().join("missing").display().to_string());
        let no_token = CodexHome::new(dir.path().join("no-token"));
        std::fs::create_dir_all(no_token.path()).unwrap();
        std::fs::write(no_token.auth_path(), r#"{"tokens":{"refresh_token":"r"}}"#).unwrap();
        let no_token = codex_account("no-token", &no_token.path().display().to_string());
        let probes = probe_all(&client, &[missing, no_token]).await;
        assert!(matches!(probes[0].error, Some(ProbeError::AuthFile { .. })));
        assert_eq!(probes[1].error, Some(ProbeError::AuthFileHasNoAccessToken));
        assert!(backend.requests("/usage").is_empty());
        assert!(probe_all(&client, &[]).await.is_empty());
    }
}
