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

/// One metered limit: a short window and a long one.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct RateLimit {
    pub allowed: Option<bool>,
    pub limit_reached: Option<bool>,
    pub primary: Option<Window>,
    pub secondary: Option<Window>,
}

impl RateLimit {
    /// The smaller remaining fraction of the two windows: whichever bites first.
    pub fn remaining(&self) -> Option<f64> {
        [self.primary, self.secondary]
            .into_iter()
            .flatten()
            .map(|window| window.remaining().clamp(0.0, 1.0))
            .reduce(f64::min)
    }

    /// `allowed` is authoritative when present; absence is not a rejection.
    pub fn allows(&self) -> bool {
        self.limit_reached != Some(true) && self.allowed.unwrap_or(true)
    }
}

/// A per-model limit (e.g. a separate weekly bucket for Codex-Spark), metered
/// apart from the account-wide windows.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct ExtraLimit {
    pub name: Option<String>,
    pub feature: Option<String>,
    pub limit: Option<RateLimit>,
}

impl ExtraLimit {
    /// Loose match on either the name or the metered feature, so a caller
    /// can say "spark".
    pub fn matches(&self, needle: &str) -> bool {
        let needle = needle.to_ascii_lowercase();
        [&self.name, &self.feature]
            .into_iter()
            .flatten()
            .any(|text| text.to_ascii_lowercase().contains(&needle))
    }

    pub fn is_limited(&self) -> bool {
        self.limit
            .as_ref()
            .is_some_and(|limit| limit.limit_reached == Some(true))
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

    fn limit(&self) -> Option<&RateLimit> {
        self.usage.as_ref()?.limit.as_ref()
    }

    pub fn primary(&self) -> Option<Window> {
        self.limit()?.primary
    }

    pub fn secondary(&self) -> Option<Window> {
        self.limit()?.secondary
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
    reset_after_seconds: Option<i64>,
    reset_at: Option<i64>,
}

impl RawWindow {
    fn window(&self, at: DateTime<Utc>) -> Option<Window> {
        let used = self.used_percent? / 100.0;
        let resets_at = self
            .reset_at
            .or_else(|| self.reset_after_seconds.map(|after| at.timestamp() + after))
            .unwrap_or(0);
        Some(Window::new(used, resets_at))
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
            primary: self.primary_window.as_ref().and_then(|w| w.window(at)),
            secondary: self.secondary_window.as_ref().and_then(|w| w.window(at)),
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
    rate_limit: Option<RawRateLimit>,
}

#[derive(Deserialize, Default)]
struct RawReachedType {
    #[serde(rename = "type")]
    kind: Option<String>,
}

#[derive(Deserialize, Default)]
struct RawUsage {
    user_id: Option<String>,
    account_id: Option<String>,
    email: Option<String>,
    plan_type: Option<String>,
    rate_limit: Option<RawRateLimit>,
    credits: Option<RawCredits>,
    rate_limit_reached_type: Option<RawReachedType>,
    #[serde(default)]
    additional_rate_limits: Vec<RawExtraLimit>,
}

impl RawUsage {
    fn usage(self, at: DateTime<Utc>) -> Usage {
        Usage {
            user_id: self.user_id,
            account_id: self.account_id,
            email: self.email,
            plan: self.plan_type.as_deref().map(Plan::parse),
            limit: self.rate_limit.as_ref().map(|limit| limit.limit(at)),
            credits: self.credits.map(|credits| Credits {
                has_credits: credits.has_credits,
                unlimited: credits.unlimited,
                overage_limit_reached: credits.overage_limit_reached,
                balance: match credits.balance {
                    None | Some(Value::Null) => None,
                    Some(Value::String(text)) => Some(text),
                    Some(other) => Some(other.to_string()),
                },
            }),
            limit_reason: self
                .rate_limit_reached_type
                .and_then(|reached| reached.kind),
            extra: self
                .additional_rate_limits
                .into_iter()
                .map(|extra| ExtraLimit {
                    name: extra.limit_name,
                    feature: extra.metered_feature,
                    limit: extra.rate_limit.as_ref().map(|limit| limit.limit(at)),
                })
                .collect(),
        }
    }
}

/// Decode a usage body. On a strict failure, retry on just the subtree
/// rotation depends on, so a cosmetic field changing shape does not park a
/// working account.
pub fn decode_usage(body: &str, at: DateTime<Utc>) -> Result<Usage, ProbeError> {
    let strict_error = match serde_json::from_str::<RawUsage>(body) {
        Ok(raw) => return Ok(raw.usage(at)),
        Err(error) => error,
    };
    let value: Value = serde_json::from_str(body).map_err(|error| ProbeError::Decode {
        message: format!("failed to parse usage response as JSON: {error}"),
    })?;
    let rate_limit: Option<RawRateLimit> = value
        .get("rate_limit")
        .and_then(|limit| serde_json::from_value(limit.clone()).ok());
    if rate_limit.is_none() {
        return Err(ProbeError::Decode {
            message: format!("failed to decode usage response: {strict_error}"),
        });
    }
    let text = |key: &str| value.get(key)?.as_str().map(str::to_owned);
    Ok(RawUsage {
        user_id: text("user_id"),
        account_id: text("account_id"),
        email: text("email"),
        plan_type: text("plan_type"),
        rate_limit,
        credits: None,
        rate_limit_reached_type: value
            .get("rate_limit_reached_type")
            .and_then(|reached| serde_json::from_value(reached.clone()).ok()),
        additional_rate_limits: Vec::new(),
    }
    .usage(at))
}

async fn fetch_usage(
    client: &Client,
    account: &str,
    auth: &AuthFile,
    at: DateTime<Utc>,
) -> Result<Usage, ProbeError> {
    let access_token = auth
        .access_token()
        .ok_or(ProbeError::AuthFileHasNoAccessToken)?;
    let account_id = auth.effective_account_id();
    let mut attempt = 0;
    let response = loop {
        let mut request = client
            .http()
            .get(client.usage_url())
            .bearer_auth(access_token)
            .header("originator", ORIGINATOR)
            .header("User-Agent", "codex-cli");
        if let Some(account_id) = &account_id {
            request = request.header("chatgpt-account-id", account_id);
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
                "--- {account} {status} {:?} (attempts: {})",
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
    let body = response.text().await.map_err(|error| ProbeError::Network {
        message: error.to_string(),
    })?;
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
    use axum::Router;
    use axum::body::Bytes;
    use axum::extract::State as AxumState;
    use axum::http::{HeaderMap, StatusCode};
    use axum::response::IntoResponse;
    use axum::routing::{get, post};
    use serde_json::json;
    use std::sync::{Arc, Mutex};

    /// Serve `app` on a loopback port; returns its base URL.
    pub(crate) async fn serve(app: Router) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        format!("http://{address}")
    }

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
                .with_state(self.clone());
            let base = serve(app).await;
            Client::with_endpoints(
                format!("{base}/usage"),
                format!("{base}/token"),
                "test-client",
            )
            .unwrap()
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

    async fn token_handler(
        AxumState(backend): AxumState<Backend>,
        headers: HeaderMap,
        body: Bytes,
    ) -> axum::response::Response {
        backend.seen.lock().unwrap().push(Seen {
            path: "/token".into(),
            headers,
            body: String::from_utf8_lossy(&body).into_owned(),
        });
        match take(&backend.token) {
            Some((status, body)) => (status, body).into_response(),
            None => StatusCode::NOT_IMPLEMENTED.into_response(),
        }
    }

    pub(crate) fn ok(body: Value) -> (StatusCode, HeaderMap, String) {
        let mut headers = HeaderMap::new();
        headers.insert("content-type", "application/json".parse().unwrap());
        (StatusCode::OK, headers, body.to_string())
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

    /// A profile directory with a seated bundle; returns the account.
    pub(crate) fn profile(dir: &std::path::Path, name: &str, refresh_token: &str) -> CodexAccount {
        let home = CodexHome::new(dir.join(name));
        home.write_tokens(&TokenBundle {
            access_token: format!("access-{name}"),
            refresh_token: refresh_token.to_owned(),
            id_token: Some(crate::codex::auth::tests::id_token(
                &format!("{name}@x.y"),
                &format!("acct-{name}"),
                "plus",
            )),
            account_id: None,
            last_refresh: Some(Utc::now()),
        })
        .unwrap();
        CodexAccount {
            name: name.to_owned(),
            codex_home: home.path().display().to_string(),
            ..CodexAccount::default()
        }
    }

    #[test]
    fn a_full_body_decodes_to_typed_usage() {
        let at = DateTime::from_timestamp(1_000, 0).unwrap();
        let usage = decode_usage(&usage_body(40.0, 75.0).to_string(), at).unwrap();
        assert_eq!(usage.plan, Some(Plan::Plus));
        let limit = usage.limit.as_ref().unwrap();
        assert_eq!(limit.primary, Some(Window::new(0.4, 4_600)));
        assert_eq!(limit.secondary, Some(Window::new(0.75, 1_800_000_000)));
        assert_eq!(limit.remaining(), Some(0.25));
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
        assert!(usage.credits.is_none());
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
        let account = CodexAccount {
            name: "a".into(),
            codex_home: "/nowhere".into(),
            ..CodexAccount::default()
        };
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
            (
                StatusCode::UNAUTHORIZED,
                HeaderMap::new(),
                r#"{"detail": "token  expired"}"#.into(),
            ),
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
        let missing = CodexAccount {
            name: "missing".into(),
            codex_home: dir.path().join("missing").display().to_string(),
            ..CodexAccount::default()
        };
        let no_token = CodexHome::new(dir.path().join("no-token"));
        std::fs::create_dir_all(no_token.path()).unwrap();
        std::fs::write(no_token.auth_path(), r#"{"tokens":{"refresh_token":"r"}}"#).unwrap();
        let no_token = CodexAccount {
            name: "no-token".into(),
            codex_home: no_token.path().display().to_string(),
            ..CodexAccount::default()
        };
        let probes = probe_all(&client, &[missing, no_token]).await;
        assert!(matches!(probes[0].error, Some(ProbeError::AuthFile { .. })));
        assert_eq!(probes[1].error, Some(ProbeError::AuthFileHasNoAccessToken));
        assert!(backend.requests("/usage").is_empty());
        assert!(probe_all(&client, &[]).await.is_empty());
    }
}
