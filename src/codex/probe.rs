//! The usage probe: `GET /backend-api/codex/usage` reports which windows
//! exist, how full they are, when they reset, and whether the backend accepts
//! the account at all. It is the only liveness check: `codex login status`
//! reads the local file and reports a revoked account as logged in.
//!
//! The answer becomes a [`Reading`]: the account-wide windows as
//! account-wide limits named by their length, each additional limit as
//! limits scoped to its model or metered feature, and the rest in
//! [`CodexDetail`].

use chrono::{DateTime, Utc};
use serde::Deserialize;
use serde_json::Value;
use std::fmt;
use std::time::Duration;

use super::auth::{AuthFile, CodexHome};
use super::{Client, is_edge_challenge};
use crate::account::CodexAccount;
use crate::quota::{
    CodexDetail, Credits, Detail, Limit, ModelKey, ModelUsage, ReadError, Reading,
    ResetCreditCount, Scope, Service, Source, Span, SpendControl, SpendLimit, Verdict, Window,
    WindowStatus,
};

/// Codex identifies itself this way; the backend varies behaviour by originator.
const ORIGINATOR: &str = "codex_cli_rs";
/// Retries after a Cloudflare challenge. The first retry already carries the
/// `__cf_bm` cookie the challenge set, which is usually enough.
const CHALLENGE_RETRIES: u32 = 3;

/// One limit block with its windows named by length, in the order the
/// backend sent them. The backend moves windows between `primary_window`
/// and `secondary_window` (an account whose only window is the weekly one
/// reports it as primary), so position says nothing.
#[derive(Debug, Clone, PartialEq, Default)]
pub(crate) struct Block {
    pub allowed: Option<bool>,
    pub limit_reached: Option<bool>,
    pub windows: Vec<(Span, Window)>,
}

impl Block {
    /// `allowed` is authoritative when present; absence is not a rejection.
    pub(crate) fn allows(&self) -> bool {
        self.limit_reached != Some(true) && self.allowed.unwrap_or(true)
    }
}

/// A per-feature or per-model limit (e.g. a separate bucket for
/// Codex-Spark), metered apart from the account-wide windows.
#[derive(Debug, Clone, PartialEq, Default)]
pub(crate) struct Extra {
    pub name: Option<String>,
    pub feature: Option<String>,
    /// The model whose ordinary limit this one stands in for.
    pub model: Option<String>,
    pub block: Option<Block>,
}

/// The limits of a usage answer. The account-wide block's windows are
/// account-wide limits; whether the backend accepts work under it is
/// [`CodexDetail::allowed`]. An additional limit's windows are scoped to its
/// model when it names one, else to its metered feature, carry its name as
/// their label, and are marked rejected when the backend refuses it.
pub(crate) fn limits_of(shared: Option<&Block>, extras: &[Extra]) -> Vec<Limit> {
    let mut limits: Vec<Limit> = shared
        .into_iter()
        .flat_map(|block| &block.windows)
        .map(|(span, window)| Limit::new(*span, Scope::All, *window, Source::Poll))
        .collect();
    for extra in extras {
        let Some(block) = &extra.block else {
            continue;
        };
        let name = extra.name.as_deref().unwrap_or("?");
        let (scope, label) = match (&extra.model, &extra.feature) {
            (Some(model), _) => (
                Scope::Model(ModelKey::new(model)),
                format!("{name} ({model})"),
            ),
            (None, Some(feature)) => (Scope::Group(feature.clone()), name.to_owned()),
            (None, None) => (Scope::Group(name.to_owned()), name.to_owned()),
        };
        let status = (!block.allows()).then_some(WindowStatus {
            verdict: Verdict::Rejected,
            surpassed_threshold: None,
        });
        limits.extend(block.windows.iter().map(|(span, window)| Limit {
            label: label.clone(),
            status,
            ..Limit::new(*span, scope.clone(), *window, Source::Poll)
        }));
    }
    limits
}

/// Whether the backend would accept work on this account. A 200 with no
/// limit block means no limit is being enforced.
pub fn is_viable(reading: &Reading) -> bool {
    reading.error.is_none() && reading.allows()
}

/// Least remaining across the account-wide windows: whichever bites first.
/// `None` is no reading, which differs from no headroom.
pub fn remaining(reading: &Reading) -> Option<f64> {
    reading
        .shared()
        .map(|limit| limit.window.remaining().clamp(0.0, 1.0))
        .reduce(f64::min)
}

/// Whether an account can take work right now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum State {
    Ok,
    Limited { reason: Option<String> },
    Error { error: ReadError },
}

impl State {
    pub fn of(reading: &Reading) -> Self {
        if let Some(error) = &reading.error {
            return Self::Error {
                error: error.clone(),
            };
        }
        if is_viable(reading) {
            Self::Ok
        } else {
            Self::Limited {
                reason: reading.codex().and_then(|codex| codex.limit_reason.clone()),
            }
        }
    }
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
    fn window(&self, at: DateTime<Utc>) -> Option<(Span, Window)> {
        let used = self.used_percent? / 100.0;
        let resets_at = self
            .reset_at
            .or_else(|| self.reset_after_seconds.map(|after| at.timestamp() + after))
            .unwrap_or(0);
        Some((
            Span::from_seconds(self.limit_window_seconds),
            Window::new(used, resets_at),
        ))
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
    fn block(&self, at: DateTime<Utc>) -> Block {
        Block {
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

/// Decode a usage body field by field into a reading of `account`.
/// `rate_limit` is what rotation rests on, so a malformed one fails the
/// decode; any other field that changed shape is dropped alone, so a
/// cosmetic change does not park a working account or hide the rest of the
/// body.
pub fn decode_usage(account: &str, body: &str, at: DateTime<Utc>) -> Result<Reading, ReadError> {
    let value: Value = serde_json::from_str(body).map_err(|error| ReadError::Decode {
        message: format!("failed to parse usage response as JSON: {error}"),
    })?;
    let Some(object) = value.as_object() else {
        return Err(ReadError::Decode {
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
            |error| ReadError::Decode {
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
    let shared = rate_limit.as_ref().map(|limit| limit.block(at));
    let extras: Vec<Extra> = extra
        .into_iter()
        .map(|extra| Extra {
            name: extra.limit_name,
            feature: extra.metered_feature,
            model: extra.normal_model_slug,
            block: extra.rate_limit.as_ref().map(|limit| limit.block(at)),
        })
        .collect();
    Ok(Reading {
        plan: field(object, "plan_type"),
        limits: limits_of(shared.as_ref(), &extras),
        detail: Detail::Codex(CodexDetail {
            email: field(object, "email"),
            allowed: shared.as_ref().map(Block::allows),
            limit_reason: reached.and_then(|reached| reached.kind),
            credits: credits.map(|credits| Credits {
                has_credits: credits.has_credits,
                unlimited: credits.unlimited,
                overage_limit_reached: credits.overage_limit_reached,
                balance: text_of(credits.balance),
            }),
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
        }),
        ..Reading::empty(Service::Codex, account, at.timestamp())
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
) -> Result<String, ReadError> {
    let access_token = auth.access_token().ok_or_else(|| ReadError::Credential {
        message: "auth.json has no access_token".to_owned(),
    })?;
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
        let response = request.send().await.map_err(|error| ReadError::Network {
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
            ReadError::Edge {
                status: status.as_u16(),
                attempts: attempt + 1,
            }
        } else {
            ReadError::Http {
                status: status.as_u16(),
                body: super::one_line(&body, 160),
            }
        });
    }
    response.text().await.map_err(|error| ReadError::Network {
        message: error.to_string(),
    })
}

/// Probe one account. A dead account is data, not an error: the reading
/// carries why, and the email the stored id token names.
pub async fn probe(client: &Client, account: &CodexAccount) -> Reading {
    let at = Utc::now();
    let auth = CodexHome::new(account.home()).read();
    let outcome = match &auth {
        Ok(auth) => {
            match call(
                client,
                &account.name,
                auth,
                reqwest::Method::GET,
                client.usage_url(),
                None,
            )
            .await
            {
                Ok(body) => decode_usage(&account.name, &body, at),
                Err(error) => Err(error),
            }
        }
        Err(error) => Err(ReadError::Credential {
            message: format!("{error:#}"),
        }),
    };
    let mut reading = outcome.unwrap_or_else(|error| {
        Reading::failed(Service::Codex, &account.name, at.timestamp(), error)
    });
    if let Detail::Codex(codex) = &mut reading.detail
        && codex.email.is_none()
    {
        codex.email = auth
            .ok()
            .as_ref()
            .and_then(AuthFile::claims)
            .and_then(|claims| claims.email);
    }
    reading
}

/// Probe every account through one client. The first request goes alone so
/// a challenge settles and seeds the cookie jar; the rest run concurrently.
pub async fn probe_all(client: &Client, accounts: &[CodexAccount]) -> Vec<Reading> {
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

    fn decode(body: &Value, at: i64) -> Reading {
        decode_usage(
            "a",
            &body.to_string(),
            DateTime::from_timestamp(at, 0).unwrap(),
        )
        .unwrap()
    }

    fn scoped_names(reading: &Reading) -> Vec<String> {
        reading
            .scoped()
            .into_iter()
            .map(|(_, limits)| limits[0].label.clone())
            .collect()
    }

    #[test]
    fn a_full_body_decodes_to_a_reading() {
        let reading = decode(&usage_body(40.0, 75.0), 1_000);
        assert_eq!((reading.service, reading.at), (Service::Codex, 1_000));
        assert_eq!(reading.account, "a");
        assert_eq!(reading.plan.as_deref(), Some("plus"));
        assert_eq!(reading.session(), Some(Window::new(0.4, 4_600)));
        assert_eq!(reading.weekly(), Some(Window::new(0.75, 1_800_000_000)));
        assert_eq!(remaining(&reading), Some(0.25));
        assert_eq!(reading.tightest().unwrap().span, Span::Weekly);
        assert!(reading.allows() && is_viable(&reading));
        let codex = reading.codex().unwrap();
        assert_eq!(codex.allowed, Some(true));
        assert_eq!(codex.email.as_deref(), Some("a@b.c"));
        assert_eq!(
            codex.credits.as_ref().unwrap().balance.as_deref(),
            Some("0")
        );
        assert_eq!(codex.limit_reason, None);
        let scoped = reading.scoped();
        assert_eq!(scoped.len(), 1);
        let (scope, spark) = &scoped[0];
        assert_eq!(**scope, Scope::Group("gpt-5.3-codex-spark".into()));
        assert_eq!(spark[0].label, "codex_spark");
        assert!(spark[0].matches("SPARK") && spark[0].matches("gpt-5.3"));
        assert!(spark[0].is_rejected());
        assert_eq!(spark[0].window.remaining(), 0.0);
        assert_eq!(
            spark[0].span,
            Span::Unknown,
            "a window without limit_window_seconds is not guessed from its position"
        );
        assert!(
            reading.above(0.0, 0.0, None),
            "a spent feature limit is metered apart from the account"
        );
        assert!(codex.models.is_empty());
        assert_eq!(codex.reset_credits, None);
    }

    #[test]
    fn todays_weekly_only_body_names_its_one_window_weekly() {
        let reading = decode(&weekly_only_body(42.0), 1_000);
        assert_eq!(reading.plan.as_deref(), Some("promax"));
        assert_eq!(reading.session(), None);
        assert_eq!(reading.weekly(), Some(Window::new(0.42, 1_794_607_293)));
        assert_eq!(reading.limits.len(), 1, "additional_rate_limits: null");
        assert!((remaining(&reading).unwrap() - 0.58).abs() < 1e-9);
        let codex = reading.codex().unwrap();
        assert_eq!(
            codex.models,
            vec![ModelUsage {
                slug: "gpt-6-astra".into(),
                available: Some(true),
                available_at: None,
                credits_would_enable: Some(false),
            }]
        );
        assert_eq!(
            codex.spend_control,
            Some(SpendControl {
                reached: Some(false),
                limit: None
            })
        );
        assert_eq!(
            codex.reset_credits,
            Some(ResetCreditCount {
                available: 2,
                applicable: Some(0)
            })
        );
        assert_eq!(
            codex.credits.as_ref().unwrap().balance.as_deref(),
            Some("0")
        );
    }

    #[test]
    fn newer_fields_decode_with_their_alternative_spellings() {
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
        let reading = decode(&body, 0);
        assert_eq!(
            reading.shared().map(|limit| limit.span).collect::<Vec<_>>(),
            vec![Span::Weekly, Span::Other(86_400)]
        );
        assert_eq!(reading.tightest().unwrap().span, Span::Other(86_400));
        let codex = reading.codex().unwrap();
        assert_eq!(
            codex
                .models
                .iter()
                .map(|m| (m.slug.as_str(), m.available_at))
                .collect::<Vec<_>>(),
            vec![
                ("a-model", Some(1_794_000_000)),
                ("b-model", Some(1_794_441_600))
            ]
        );
        assert!(codex.models.iter().all(ModelUsage::is_unavailable));
        let spend = codex.spend_control.clone().unwrap();
        assert_eq!(spend.reached, Some(true));
        let cap = spend.limit.unwrap();
        assert_eq!(cap.limit.as_deref(), Some("50.00"));
        assert_eq!(cap.used.as_deref(), Some("50"));
        assert_eq!(cap.used_percent, Some(100.0));
        // A limit that stands in for a model's ordinary one is scoped to it.
        let spark = reading
            .limit(
                Span::FiveHour,
                &Scope::Model(ModelKey::new("gpt-6-astra-spark")),
            )
            .unwrap();
        assert_eq!(spark.label, "spark (gpt-6-astra-spark)");
        assert!(spark.matches("astra-spark") && !spark.is_rejected());
        assert_eq!(spark.window, Window::new(0.2, 0));
    }

    #[test]
    fn a_cosmetic_shape_change_falls_back_to_the_rate_limit_subtree() {
        let at = DateTime::from_timestamp(0, 0).unwrap();
        let mut body = usage_body(10.0, 20.0);
        body["additional_rate_limits"] = json!("surprise");
        body["rate_limit_reached_type"] = json!({"type": "secondary_window"});
        let reading = decode(&body, 0);
        let codex = reading.codex().unwrap();
        assert_eq!(codex.email.as_deref(), Some("a@b.c"));
        assert_eq!(remaining(&reading), Some(0.8));
        assert_eq!(codex.limit_reason.as_deref(), Some("secondary_window"));
        assert!(scoped_names(&reading).is_empty());
        assert_eq!(
            codex.credits.as_ref().unwrap().balance.as_deref(),
            Some("0"),
            "a broken field does not take its neighbours with it"
        );
        for broken in ["[1, 2]", r#"{"rate_limit": 5}"#, "not json"] {
            assert!(matches!(
                decode_usage("a", broken, at),
                Err(ReadError::Decode { .. })
            ));
        }
    }

    #[test]
    fn viability_follows_the_limit_block() {
        let with = |block: Option<Block>, reason: Option<&str>| {
            let mut reading = Reading::empty(Service::Codex, "a", 0);
            reading.limits = limits_of(block.as_ref(), &[]);
            reading.detail = Detail::Codex(CodexDetail {
                allowed: block.as_ref().map(Block::allows),
                limit_reason: reason.map(str::to_owned),
                ..CodexDetail::default()
            });
            reading
        };
        assert!(
            is_viable(&with(None, None)),
            "no limit block: nothing enforced"
        );
        assert_eq!(State::of(&with(None, None)), State::Ok);
        let reached = with(
            Some(Block {
                limit_reached: Some(true),
                ..Block::default()
            }),
            Some("primary_window"),
        );
        assert!(!is_viable(&reached));
        assert_eq!(State::of(&reached).to_string(), "limited (primary_window)");
        let refused = with(
            Some(Block {
                allowed: Some(false),
                ..Block::default()
            }),
            None,
        );
        assert_eq!(State::of(&refused).to_string(), "limited");
        let failed = Reading::failed(
            Service::Codex,
            "a",
            0,
            ReadError::Http {
                status: 401,
                body: "nope".into(),
            },
        );
        assert!(failed.is_refused());
        assert!(!is_viable(&failed));
        assert_eq!(
            State::of(&failed).to_string(),
            "ERROR: HTTP 401: nope, needs `tokeman codex refresh`"
        );
    }

    #[tokio::test]
    async fn a_probe_sends_codex_identity_and_reads_a_reading() {
        let dir = tempfile::tempdir().unwrap();
        let account = profile(dir.path(), "one", "r1");
        let backend = Backend::default().usage_answers(vec![ok(usage_body(30.0, 60.0))]);
        let client = backend.client().await;
        let probe = probe(&client, &account).await;
        assert_eq!(probe.error, None);
        assert_eq!(probe.account, "one");
        assert_eq!(remaining(&probe), Some(0.4));
        let spark = probe.scoped_to(Some("spark")).unwrap();
        assert_eq!(remaining(&spark), Some(0.0));
        assert_eq!(probe.scoped_to(Some("nothing")), None);
        assert_eq!(probe.codex().unwrap().email.as_deref(), Some("a@b.c"));
        assert_eq!(probe.plan.as_deref(), Some("plus"));
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
        assert_eq!(remaining(&probes[0]), Some(1.0));
        assert_eq!(
            probes[1].error,
            Some(ReadError::Http {
                status: 401,
                body: r#"{"detail": "token expired"}"#.into()
            })
        );
        assert!(probes[1].error.as_ref().unwrap().is_unauthorized());
        assert_eq!(
            probes[1].codex().unwrap().email.as_deref(),
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
            Some(ReadError::Edge {
                status: 403,
                attempts: CHALLENGE_RETRIES + 1
            })
        );
        assert!(!probe.is_refused());
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
        assert!(matches!(
            probes[0].error,
            Some(ReadError::Credential { .. })
        ));
        assert_eq!(
            probes[1].error.as_ref().unwrap().to_string(),
            "auth.json has no access_token"
        );
        assert!(probes.iter().all(|probe| probe.service == Service::Codex));
        assert!(backend.requests("/usage").is_empty());
        assert!(probe_all(&client, &[]).await.is_empty());
    }
}
