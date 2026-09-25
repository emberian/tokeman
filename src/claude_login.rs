//! Browser OAuth login for a Claude account.
//!
//! `claude setup-token` mints an INFERENCE-ONLY credential: it can call
//! `/v1/messages` and nothing else. That is why `/api/oauth/usage` answers
//! `OAuth token does not meet scope requirement user:profile`, and why a stored
//! setup token is refused as a claude.ai login record. Both limits come from
//! the same missing scope, so both are fixed by asking for it.
//!
//! This runs the same authorization-code + PKCE flow Claude Code uses. The
//! `code=true` parameter makes the consent page render the code for pasting
//! rather than redirecting to a local port, so no callback server is needed.

use anyhow::{Context, Result, bail};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde::Deserialize;
use sha2::{Digest, Sha256};

pub const CLIENT_ID: &str = "9d1c250a-e61b-44d9-88ed-5944d1962f5e";
pub const AUTHORIZE_URL: &str = "https://claude.com/cai/oauth/authorize";
pub const TOKEN_URL: &str = "https://platform.claude.com/v1/oauth/token";
pub const REDIRECT_URI: &str = "https://platform.claude.com/oauth/code/callback";

/// `offline_access` is what earns a refresh token; without it the credential
/// dies in an hour and the daemon cannot renew it.
pub const SCOPES: &str = "user:profile user:inference user:sessions:claude_code user:file_upload user:mcp_servers offline_access";

pub struct Pkce {
    pub verifier: String,
    pub state: String,
    pub url: String,
}

#[derive(Debug, Clone)]
pub struct Bundle {
    pub access_token: String,
    pub refresh_token: Option<String>,
    /// Epoch milliseconds, matching Claude's own Keychain record.
    pub expires_at: Option<i64>,
    pub scopes: Vec<String>,
}

impl Bundle {
    pub fn has_profile_scope(&self) -> bool {
        self.scopes.iter().any(|scope| scope == "user:profile")
    }
}

/// 32 bytes from the OS. Avoids pulling a RNG crate in for one call.
///
/// `read_exact` into a fixed buffer: `/dev/urandom` never reaches EOF, so
/// anything that reads "the whole file" (`fs::read`, `read_to_end`) allocates
/// until the machine runs out of memory.
fn random_b64() -> Result<String> {
    use std::io::Read;
    let mut bytes = [0u8; 32];
    std::fs::File::open("/dev/urandom")
        .and_then(|mut source| source.read_exact(&mut bytes))
        .context("could not read /dev/urandom for PKCE material")?;
    Ok(URL_SAFE_NO_PAD.encode(bytes))
}

pub fn begin() -> Result<Pkce> {
    let verifier = random_b64()?;
    let state = random_b64()?;
    let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
    let url = format!(
        "{AUTHORIZE_URL}?code=true&client_id={CLIENT_ID}&response_type=code\
         &redirect_uri={redirect}&scope={scope}&code_challenge={challenge}\
         &code_challenge_method=S256&state={state}",
        redirect = urlencode(REDIRECT_URI),
        scope = urlencode(SCOPES),
    );
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

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
    refresh_token: Option<String>,
    expires_in: Option<i64>,
    scope: Option<String>,
}

fn to_bundle(response: TokenResponse) -> Bundle {
    Bundle {
        expires_at: response
            .expires_in
            .map(|seconds| chrono::Utc::now().timestamp_millis() + seconds * 1000),
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

/// The consent page hands back `code#state`; accept either form and verify the
/// state so a code pasted from a different run cannot be redeemed here.
pub async fn exchange(pasted: &str, pkce: &Pkce) -> Result<Bundle> {
    let pasted = pasted.trim();
    let (code, state) = match pasted.split_once('#') {
        Some((code, state)) => (code.trim(), Some(state.trim())),
        None => (pasted, None),
    };
    if code.is_empty() {
        bail!("no authorization code was pasted");
    }
    if let Some(state) = state
        && state != pkce.state
    {
        bail!("state mismatch: that code came from a different login attempt");
    }
    let body = serde_json::json!({
        "grant_type": "authorization_code",
        "client_id": CLIENT_ID,
        "code": code,
        "redirect_uri": REDIRECT_URI,
        "code_verifier": pkce.verifier,
        "state": pkce.state,
    });
    post_token(body).await
}

pub async fn refresh(refresh_token: &str) -> Result<Bundle> {
    let body = serde_json::json!({
        "grant_type": "refresh_token",
        "client_id": CLIENT_ID,
        "refresh_token": refresh_token,
    });
    post_token(body).await
}

/// A non-success answer from the token endpoint.
#[derive(Debug)]
pub struct TokenEndpointError {
    pub status: u16,
    pub body: String,
}

impl std::fmt::Display for TokenEndpointError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "token endpoint returned HTTP {}: {}",
            self.status, self.body
        )
    }
}

impl std::error::Error for TokenEndpointError {}

impl TokenEndpointError {
    /// Refused for good: the grant was revoked, or the refresh token was
    /// already redeemed. Only a new browser login recovers it.
    fn is_permanent(&self) -> bool {
        let code = serde_json::from_str::<serde_json::Value>(&self.body)
            .ok()
            .and_then(|body| body.get("error")?.as_str().map(str::to_owned));
        self.status == 401 || code.as_deref() == Some("invalid_grant")
    }
}

pub fn is_permanent_refresh_failure(error: &anyhow::Error) -> bool {
    error
        .downcast_ref::<TokenEndpointError>()
        .is_some_and(TokenEndpointError::is_permanent)
}

async fn post_token(body: serde_json::Value) -> Result<Bundle> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()?;
    let response = client
        .post(TOKEN_URL)
        .header("Content-Type", "application/json")
        .header("anthropic-beta", "oauth-2025-04-20")
        .header("user-agent", "claude-code/2.1.247")
        .json(&body)
        .send()
        .await
        .context("token endpoint request failed")?;
    let status = response.status();
    let text = response.text().await.unwrap_or_default();
    if !status.is_success() {
        return Err(TokenEndpointError {
            status: status.as_u16(),
            body: text,
        }
        .into());
    }
    let parsed: TokenResponse =
        serde_json::from_str(&text).context("could not parse the token response")?;
    Ok(to_bundle(parsed))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn authorize_url_carries_profile_scope_and_a_challenge() {
        let pkce = begin().expect("pkce");
        assert!(pkce.url.contains("user%3Aprofile"));
        assert!(pkce.url.contains("offline_access"));
        assert!(pkce.url.contains("code_challenge_method=S256"));
        assert!(pkce.url.contains(CLIENT_ID));
        assert_ne!(pkce.verifier, pkce.state);
    }

    #[tokio::test]
    async fn a_code_from_another_attempt_is_refused() {
        let pkce = begin().expect("pkce");
        let error = exchange("somecode#not-our-state", &pkce)
            .await
            .expect_err("state mismatch must be rejected before any network call");
        assert!(error.to_string().contains("state mismatch"));
    }

    #[test]
    fn only_revoked_or_redeemed_grants_are_permanent() {
        let error = |status, body: &str| {
            anyhow::Error::from(TokenEndpointError {
                status,
                body: body.into(),
            })
        };
        assert!(is_permanent_refresh_failure(&error(
            400,
            r#"{"error":"invalid_grant"}"#
        )));
        assert!(is_permanent_refresh_failure(&error(401, "")));
        assert!(!is_permanent_refresh_failure(&error(
            429,
            r#"{"error":"rate_limited"}"#
        )));
        // A message merely mentioning the code is not the code.
        assert!(!is_permanent_refresh_failure(&error(
            500,
            "upstream said invalid_grant?"
        )));
        assert!(!is_permanent_refresh_failure(&anyhow::anyhow!("timed out")));
    }

    #[tokio::test]
    async fn an_empty_paste_is_refused() {
        let pkce = begin().expect("pkce");
        assert!(exchange("   ", &pkce).await.is_err());
    }
}
