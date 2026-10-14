//! Browser OAuth login for a Claude account: the authorization-code + PKCE
//! flow Claude Code uses, with `code=true` so the consent page shows the code
//! for pasting instead of redirecting to a local port.
//!
//! A `claude setup-token` is inference-only; `/api/oauth/usage` and the
//! Keychain login record both need `user:profile`, which only this grants.

use std::fmt;
use std::time::Duration;

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::{Client, describe};
use crate::account::Grant;

pub const CLIENT_ID: &str = "9d1c250a-e61b-44d9-88ed-5944d1962f5e";
pub const AUTHORIZE_URL: &str = "https://claude.com/cai/oauth/authorize";
pub const TOKEN_URL: &str = "https://platform.claude.com/v1/oauth/token";
pub const REDIRECT_URI: &str = "https://platform.claude.com/oauth/code/callback";
/// Claude Code's own claude.ai login scopes (2.1.282) minus
/// `org:create_api_key`. The refresh token comes with the grant; asking for
/// `offline_access` makes the consent page refuse.
pub const SCOPES: &str =
    "user:profile user:inference user:sessions:claude_code user:mcp_servers user:file_upload";
/// The token endpoint rate-limits hard and a pasted code survives a 429.
const EXCHANGE_RETRY_SECS: [u64; 3] = [5, 15, 30];

/// A login in progress: the URL to open and what redeems its code.
#[derive(Debug, Clone)]
pub struct Pkce {
    pub verifier: String,
    pub state: String,
    pub url: String,
}

/// Whether a refusal will hold until a new browser login.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refusal {
    /// The grant was revoked or the refresh token already redeemed.
    Permanent,
    Transient,
}

/// 401, or an `invalid_grant` error code in the body.
pub fn refusal(status: u16, body: &str) -> Refusal {
    let code = serde_json::from_str::<serde_json::Value>(body)
        .ok()
        .and_then(|body| body.get("error")?.as_str().map(str::to_owned));
    if status == 401 || code.as_deref() == Some("invalid_grant") {
        Refusal::Permanent
    } else {
        Refusal::Transient
    }
}

#[derive(Debug)]
pub enum Error {
    /// The pasted code cannot be redeemed here.
    Paste(&'static str),
    /// The endpoint answered with a non-success status.
    Refused {
        status: u16,
        body: String,
    },
    Network(reqwest::Error),
    Unreadable(serde_json::Error),
    Entropy(std::io::Error),
}

impl Error {
    pub fn refusal(&self) -> Refusal {
        match self {
            Self::Refused { status, body } => refusal(*status, body),
            _ => Refusal::Transient,
        }
    }

    pub fn is_permanent(&self) -> bool {
        self.refusal() == Refusal::Permanent
    }

    fn is_rate_limited(&self) -> bool {
        matches!(self, Self::Refused { status: 429, .. })
    }
}

/// Whether a refresh failure holds until a new browser login.
pub fn is_permanent(error: &Error) -> bool {
    error.is_permanent()
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Paste(reason) => f.write_str(reason),
            Self::Refused { status, body } => {
                write!(f, "token endpoint returned HTTP {status}: {body}")
            }
            Self::Network(error) => write!(f, "token endpoint request failed: {}", describe(error)),
            Self::Unreadable(error) => write!(f, "could not parse the token response: {error}"),
            Self::Entropy(error) => write!(f, "could not read /dev/urandom: {error}"),
        }
    }
}

impl std::error::Error for Error {}

/// 32 bytes from the OS, read with `read_exact`: `/dev/urandom` never reaches
/// EOF, so a read-to-end there allocates without bound.
fn random_b64() -> Result<String, Error> {
    use std::io::Read;
    let mut bytes = [0u8; 32];
    std::fs::File::open("/dev/urandom")
        .and_then(|mut source| source.read_exact(&mut bytes))
        .map_err(Error::Entropy)?;
    Ok(URL_SAFE_NO_PAD.encode(bytes))
}

/// Start a login. `login_hint` (an email) pre-fills the account on the
/// consent page, so a round is not completed as the wrong account.
pub fn begin(login_hint: Option<&str>) -> Result<Pkce, Error> {
    let verifier = random_b64()?;
    let state = random_b64()?;
    let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
    let mut url = format!(
        "{AUTHORIZE_URL}?code=true&client_id={CLIENT_ID}&response_type=code\
         &redirect_uri={redirect}&scope={scope}&code_challenge={challenge}\
         &code_challenge_method=S256&state={state}",
        redirect = urlencode(REDIRECT_URI),
        scope = urlencode(SCOPES),
    );
    if let Some(hint) = login_hint {
        url.push_str(&format!("&login_hint={}", urlencode(hint)));
    }
    Ok(Pkce {
        verifier,
        state,
        url,
    })
}

fn urlencode(value: &str) -> String {
    value
        .bytes()
        .map(|byte| match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                (byte as char).to_string()
            }
            _ => format!("%{byte:02X}"),
        })
        .collect()
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TokenResponse {
    pub access_token: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refresh_token: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_in: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
}

/// `expires_at` in epoch milliseconds, as Claude's own Keychain record keeps it.
pub fn grant_of(response: TokenResponse, now_ms: i64) -> Grant {
    Grant {
        expires_at: response.expires_in.map(|seconds| now_ms + seconds * 1000),
        scopes: response
            .scope
            .unwrap_or_default()
            .split_whitespace()
            .map(str::to_owned)
            .collect(),
        access_token: response.access_token,
        refresh_token: response.refresh_token,
    }
}

/// Redeem a pasted `code` or `code#state`. The state is checked before any
/// network call, so a code from a different run cannot be redeemed here.
pub async fn exchange(client: &Client, pasted: &str, pkce: &Pkce) -> Result<Grant, Error> {
    exchange_waiting(client, pasted, pkce, &EXCHANGE_RETRY_SECS).await
}

async fn exchange_waiting(
    client: &Client,
    pasted: &str,
    pkce: &Pkce,
    retry_secs: &[u64],
) -> Result<Grant, Error> {
    let pasted = pasted.trim();
    let (code, state) = match pasted.split_once('#') {
        Some((code, state)) => (code.trim(), Some(state.trim())),
        None => (pasted, None),
    };
    if code.is_empty() {
        return Err(Error::Paste("no authorization code was pasted"));
    }
    if state.is_some_and(|state| state != pkce.state) {
        return Err(Error::Paste(
            "state mismatch: that code came from a different login attempt",
        ));
    }
    let body = serde_json::json!({
        "grant_type": "authorization_code",
        "client_id": CLIENT_ID,
        "code": code,
        "redirect_uri": REDIRECT_URI,
        "code_verifier": pkce.verifier,
        "state": pkce.state,
    });
    let mut waits = retry_secs.iter().copied();
    loop {
        match post_token(client, &body).await {
            Err(error) if error.is_rate_limited() => match waits.next() {
                Some(secs) => {
                    eprintln!("  token endpoint is rate limiting; retrying in {secs}s");
                    tokio::time::sleep(Duration::from_secs(secs)).await;
                }
                None => return Err(error),
            },
            outcome => return outcome,
        }
    }
}

/// Renew a grant. `scopes` are the ones it was issued with; Claude Code
/// states them on every refresh. The old access token is revoked within
/// seconds of success.
pub async fn refresh(
    client: &Client,
    refresh_token: &str,
    scopes: &[String],
) -> Result<Grant, Error> {
    let scope = if scopes.is_empty() {
        SCOPES.to_owned()
    } else {
        scopes.join(" ")
    };
    let body = serde_json::json!({
        "grant_type": "refresh_token",
        "refresh_token": refresh_token,
        "client_id": CLIENT_ID,
        "scope": scope,
    });
    post_token(client, &body).await
}

async fn post_token(client: &Client, body: &serde_json::Value) -> Result<Grant, Error> {
    let response = client
        .token()
        .json(body)
        .send()
        .await
        .map_err(Error::Network)?;
    let status = response.status();
    let text = response.text().await.unwrap_or_default();
    if !status.is_success() {
        return Err(Error::Refused {
            status: status.as_u16(),
            body: text,
        });
    }
    let parsed: TokenResponse = serde_json::from_str(&text).map_err(Error::Unreadable)?;
    Ok(grant_of(parsed, chrono::Utc::now().timestamp_millis()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::anthropic::testing::serve;
    use axum::Router;
    use axum::http::StatusCode;
    use axum::routing::post;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU32, Ordering};

    fn dead_client() -> Client {
        Client::with_base("http://127.0.0.1:9", "http://127.0.0.1:9/t")
    }

    #[test]
    fn authorize_url_carries_profile_scope_and_a_challenge() {
        let pkce = begin(None).expect("pkce");
        assert!(pkce.url.contains("user%3Aprofile"));
        assert!(!pkce.url.contains("offline_access"));
        assert!(!pkce.url.contains("login_hint"));
        let hinted = begin(Some("me@example.com")).expect("pkce");
        assert!(hinted.url.ends_with("&login_hint=me%40example.com"));
        assert!(pkce.url.contains("code_challenge_method=S256"));
        assert!(pkce.url.contains(CLIENT_ID));
        assert_ne!(pkce.verifier, pkce.state);
        let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(pkce.verifier.as_bytes()));
        assert!(pkce.url.contains(&format!("code_challenge={challenge}&")));
        assert_eq!(pkce.verifier.len(), 43, "32 bytes, unpadded");
    }

    #[tokio::test]
    async fn a_code_from_another_attempt_is_refused() {
        let pkce = begin(None).expect("pkce");
        let error = exchange(&dead_client(), "somecode#not-our-state", &pkce)
            .await
            .expect_err("state mismatch must be rejected before any network call");
        assert!(matches!(error, Error::Paste(_)));
        assert!(error.to_string().contains("state mismatch"));
    }

    #[tokio::test]
    async fn an_empty_paste_is_refused() {
        let pkce = begin(None).expect("pkce");
        assert!(matches!(
            exchange(&dead_client(), "   ", &pkce).await,
            Err(Error::Paste(_))
        ));
    }

    #[test]
    fn only_revoked_or_redeemed_grants_are_permanent() {
        let error = |status, body: &str| Error::Refused {
            status,
            body: body.into(),
        };
        assert!(error(400, r#"{"error":"invalid_grant"}"#).is_permanent());
        assert!(error(401, "").is_permanent());
        assert!(!error(429, r#"{"error":"rate_limited"}"#).is_permanent());
        // A message merely mentioning the code is not the code.
        assert!(!error(500, "upstream said invalid_grant?").is_permanent());
        assert_eq!(Error::Paste("x").refusal(), Refusal::Transient);
        assert_eq!(
            Error::Unreadable(serde_json::from_str::<i32>("x").unwrap_err()).refusal(),
            Refusal::Transient
        );
    }

    #[test]
    fn token_responses_round_trip_into_grants() {
        let response = TokenResponse {
            access_token: "sk-ant-oat01-a".into(),
            refresh_token: Some("sk-ant-ort01-r".into()),
            expires_in: Some(28800),
            scope: Some("user:profile user:inference".into()),
        };
        let json = serde_json::to_string(&response).unwrap();
        assert_eq!(
            serde_json::from_str::<TokenResponse>(&json).unwrap(),
            response
        );
        let grant = grant_of(response, 1_000);
        assert_eq!(grant.expires_at, Some(1_000 + 28_800_000));
        assert_eq!(grant.scopes, vec!["user:profile", "user:inference"]);
        assert_eq!(grant.refresh_token.as_deref(), Some("sk-ant-ort01-r"));
        let sparse: TokenResponse = serde_json::from_str(r#"{"access_token":"a"}"#).unwrap();
        let grant = grant_of(sparse, 0);
        assert_eq!(grant.expires_at, None);
        assert!(grant.scopes.is_empty());
    }

    /// A token endpoint answering from a script, one entry per call, that
    /// checks axios's headers and records the bodies it was sent.
    fn scripted(
        script: Vec<(StatusCode, serde_json::Value)>,
    ) -> (
        Router,
        Arc<AtomicU32>,
        Arc<std::sync::Mutex<Vec<serde_json::Value>>>,
    ) {
        let calls = Arc::new(AtomicU32::new(0));
        let bodies = Arc::new(std::sync::Mutex::new(Vec::new()));
        let (counter, seen, script) = (calls.clone(), bodies.clone(), Arc::new(script));
        let router = Router::new().route(
            "/v1/oauth/token",
            post(move |headers: axum::http::HeaderMap, body: String| {
                let (script, counter, seen) = (script.clone(), counter.clone(), seen.clone());
                async move {
                    assert_eq!(
                        headers.get("user-agent").and_then(|v| v.to_str().ok()),
                        Some(super::super::TOKEN_USER_AGENT)
                    );
                    assert_eq!(
                        headers.get("accept").and_then(|v| v.to_str().ok()),
                        Some("application/json, text/plain, */*")
                    );
                    seen.lock()
                        .unwrap()
                        .push(serde_json::from_str(&body).unwrap());
                    let index = counter.fetch_add(1, Ordering::SeqCst) as usize;
                    let (status, body) = script
                        .get(index)
                        .cloned()
                        .unwrap_or((StatusCode::IM_A_TEAPOT, serde_json::Value::Null));
                    (status, body.to_string())
                }
            }),
        );
        (router, calls, bodies)
    }

    #[tokio::test]
    async fn a_refresh_states_its_scopes_and_returns_a_grant() {
        let (router, _, bodies) = scripted(vec![(
            StatusCode::OK,
            serde_json::json!({"access_token": "new", "refresh_token": "next",
                               "expires_in": 60, "scope": "user:profile"}),
        )]);
        let client = serve(router).await;
        let grant = refresh(&client, "old-refresh", &["user:profile".into()])
            .await
            .unwrap();
        assert_eq!(grant.access_token, "new");
        assert_eq!(grant.refresh_token.as_deref(), Some("next"));
        assert!(grant.expires_at.is_some_and(|at| at > 0));
        let sent = bodies.lock().unwrap().remove(0);
        assert_eq!(sent["grant_type"], "refresh_token");
        assert_eq!(sent["scope"], "user:profile");
        assert_eq!(sent["client_id"], CLIENT_ID);

        let (router, _, bodies) = scripted(vec![(
            StatusCode::BAD_REQUEST,
            serde_json::json!({"error": "invalid_grant"}),
        )]);
        let client = serve(router).await;
        let error = refresh(&client, "redeemed", &[]).await.unwrap_err();
        assert!(error.is_permanent(), "{error}");
        assert_eq!(bodies.lock().unwrap()[0]["scope"], SCOPES);
    }

    #[tokio::test]
    async fn an_exchange_waits_out_the_token_endpoints_429s() {
        assert_eq!(EXCHANGE_RETRY_SECS, [5, 15, 30]);
        let no_wait = [0u64; 3];
        let limited = (
            StatusCode::TOO_MANY_REQUESTS,
            serde_json::json!({"error": "rate"}),
        );
        let (router, calls, bodies) = scripted(vec![
            limited.clone(),
            limited.clone(),
            (StatusCode::OK, serde_json::json!({"access_token": "a"})),
        ]);
        let client = serve(router).await;
        let pkce = begin(None).unwrap();
        let pasted = format!("thecode#{}", pkce.state);
        let grant = exchange_waiting(&client, &pasted, &pkce, &no_wait)
            .await
            .unwrap();
        assert_eq!(grant.access_token, "a");
        assert_eq!(calls.load(Ordering::SeqCst), 3);
        let sent = bodies.lock().unwrap()[0].clone();
        assert_eq!(sent["code"], "thecode");
        assert_eq!(sent["code_verifier"], pkce.verifier);
        assert_eq!(sent["state"], pkce.state);

        let (router, calls, _) = scripted(vec![limited.clone(); 4]);
        let client = serve(router).await;
        let error = exchange_waiting(&client, "thecode", &pkce, &no_wait)
            .await
            .unwrap_err();
        assert!(error.is_rate_limited(), "{error}");
        assert_eq!(
            calls.load(Ordering::SeqCst),
            4,
            "three retries, then give up"
        );
    }
}
