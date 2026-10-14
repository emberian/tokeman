//! ChatGPT OAuth token refresh, shaped like Codex's own `RefreshRequest`
//! (`client_id`, `grant_type`, `refresh_token`, no scope) so tokeman and
//! Codex are interchangeable refreshers for one account.
//!
//! The refresh token rotates: the presented one dies the instant the request
//! succeeds, and re-presenting it earns `refresh_token_reused`, which is
//! terminal. The new bundle therefore reaches disk before anything else can
//! fail.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::fmt;

use super::Client;
use super::auth::{CodexHome, IdClaims, TokenBundle};

/// Codex refreshes on its own at about this age; tokeman stays ahead of it.
pub const DEFAULT_MAX_AGE_DAYS: i64 = 7;

#[derive(Serialize)]
struct RefreshRequest<'a> {
    client_id: &'a str,
    grant_type: &'static str,
    refresh_token: &'a str,
}

#[derive(Deserialize, Default)]
struct RefreshResponse {
    id_token: Option<String>,
    access_token: Option<String>,
    refresh_token: Option<String>,
}

/// Why a refresh failed, as Codex's `classify_refresh_token_failure` sees it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Failure {
    /// The refresh token aged out; re-login required.
    Expired,
    /// Already redeemed: another refresher rotated it; re-login required.
    Reused,
    /// Revoked server-side, e.g. a sign-out elsewhere; re-login required.
    Invalidated,
    /// The profile holds no refresh token (access-only); re-login required.
    NoRefreshToken,
    /// Network or unclassified server error; retry later.
    Transient,
}

impl Failure {
    /// No retry can help: the account needs an interactive `codex login`.
    pub fn is_permanent(self) -> bool {
        !matches!(self, Self::Transient)
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Expired => "refresh token expired, re-login required",
            Self::Reused => "refresh token already redeemed, another refresher is racing us",
            Self::Invalidated => "refresh token revoked, re-login required",
            Self::NoRefreshToken => "no refresh token, re-login required",
            Self::Transient => "transient refresh failure",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RefreshError {
    pub failure: Failure,
    pub detail: String,
}

impl fmt::Display for RefreshError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.failure.label(), self.detail)
    }
}

impl std::error::Error for RefreshError {}

impl RefreshError {
    fn transient(detail: impl fmt::Display) -> Self {
        Self {
            failure: Failure::Transient,
            detail: detail.to_string(),
        }
    }
}

/// The OAuth error code of a failure body (`{"error": "code"}`,
/// `{"error": {"code": ...}}` or a top-level `code`), lower-cased.
fn error_code(body: &str) -> String {
    let value: Value = serde_json::from_str(body).unwrap_or_default();
    match value.get("error") {
        Some(Value::String(code)) => Some(code.as_str()),
        Some(error) => error.get("code").and_then(Value::as_str),
        None => None,
    }
    .or_else(|| value.get("code").and_then(Value::as_str))
    .unwrap_or("")
    .to_ascii_lowercase()
}

fn classify(status: reqwest::StatusCode, body: &str) -> Failure {
    match error_code(body).as_str() {
        "refresh_token_expired" => Failure::Expired,
        "refresh_token_reused" => Failure::Reused,
        "refresh_token_invalidated" => Failure::Invalidated,
        // A 401 with no recognized code is still terminal for this token.
        _ if status == reqwest::StatusCode::UNAUTHORIZED => Failure::Invalidated,
        _ => Failure::Transient,
    }
}

/// Exchange a refresh token for a new bundle. Touches no disk: callers that
/// own a profile use [`refresh_home`], which persists first.
pub async fn exchange(client: &Client, refresh_token: &str) -> Result<TokenBundle, RefreshError> {
    let response = client
        .http()
        .post(client.token_url())
        .header("Content-Type", "application/json")
        .header("User-Agent", "codex-cli")
        .json(&RefreshRequest {
            client_id: client.client_id(),
            grant_type: "refresh_token",
            refresh_token,
        })
        .send()
        .await
        .map_err(RefreshError::transient)?;
    let status = response.status();
    let body = response.text().await.unwrap_or_default();
    if !status.is_success() {
        return Err(RefreshError {
            failure: classify(status, &body),
            detail: format!("HTTP {status}: {}", super::one_line(&body, 200)),
        });
    }
    let parsed: RefreshResponse = serde_json::from_str(&body).map_err(|err| {
        RefreshError::transient(format!("failed to decode refresh response: {err}"))
    })?;
    let access_token = parsed
        .access_token
        .ok_or_else(|| RefreshError::transient("refresh response carried no access_token"))?;
    Ok(TokenBundle {
        access_token,
        // Codex keeps the prior refresh token when the response omits one.
        refresh_token: parsed
            .refresh_token
            .unwrap_or_else(|| refresh_token.to_owned()),
        id_token: parsed.id_token,
        account_id: None,
        last_refresh: Some(Utc::now()),
    })
}

/// Refresh the profile at `home` in place. The rotated bundle is on disk
/// before this returns anything that could fail afterwards.
pub async fn refresh_home(client: &Client, home: &CodexHome) -> Result<TokenBundle, RefreshError> {
    let auth = home
        .read()
        .map_err(|err| RefreshError::transient(format!("{err:#}")))?;
    let Some(refresh_token) = auth.refresh_token() else {
        return Err(RefreshError {
            failure: Failure::NoRefreshToken,
            detail: format!("{} has no refresh_token", home.auth_path().display()),
        });
    };
    // The response does not echo the account id; without it the
    // `chatgpt-account-id` header would be lost.
    let account_id = auth.effective_account_id();
    let mut bundle = exchange(client, refresh_token).await?;
    bundle.account_id = account_id.or_else(|| {
        bundle
            .id_token
            .as_deref()
            .and_then(IdClaims::decode)
            .and_then(|claims| claims.chatgpt_account_id)
    });
    home.write_tokens(&bundle)
        .map_err(|err| RefreshError::transient(format!("{err:#}")))?;
    Ok(bundle)
}

/// `force` refreshes unconditionally; otherwise only a bundle older than
/// `max_age_days` at `now`. `Ok(None)` means the profile was left alone.
pub async fn refresh(
    client: &Client,
    home: &CodexHome,
    force: bool,
    max_age_days: i64,
    now: DateTime<Utc>,
) -> Result<Option<TokenBundle>, RefreshError> {
    if !force {
        let auth = home
            .read()
            .map_err(|err| RefreshError::transient(format!("{err:#}")))?;
        if !auth.is_stale(max_age_days, now) {
            return Ok(None);
        }
    }
    refresh_home(client, home).await.map(Some)
}

#[cfg(test)]
mod tests {
    use super::*;
    use reqwest::StatusCode;

    #[test]
    fn failures_classify_by_code_then_status() {
        assert_eq!(
            classify(
                StatusCode::BAD_REQUEST,
                r#"{"error":"refresh_token_expired"}"#
            ),
            Failure::Expired
        );
        assert_eq!(
            classify(
                StatusCode::BAD_REQUEST,
                r#"{"error":{"code":"Refresh_Token_Reused"}}"#
            ),
            Failure::Reused
        );
        assert_eq!(
            classify(
                StatusCode::FORBIDDEN,
                r#"{"code":"refresh_token_invalidated"}"#
            ),
            Failure::Invalidated
        );
        assert_eq!(
            classify(StatusCode::UNAUTHORIZED, "not json"),
            Failure::Invalidated
        );
        assert_eq!(
            classify(StatusCode::BAD_GATEWAY, "<html>"),
            Failure::Transient
        );
        assert!(Failure::NoRefreshToken.is_permanent());
        assert!(!Failure::Transient.is_permanent());
    }

    #[tokio::test]
    async fn a_profile_without_a_refresh_token_fails_locally() {
        let dir = tempfile::tempdir().unwrap();
        let home = CodexHome::new(dir.path());
        std::fs::write(
            home.auth_path(),
            r#"{"tokens":{"access_token":"a","refresh_token":""}}"#,
        )
        .unwrap();
        // An unroutable endpoint: the failure must come before any request.
        let client =
            Client::with_endpoints("http://127.0.0.1:1/usage", "http://127.0.0.1:1/token", "id")
                .unwrap();
        let error = refresh_home(&client, &home).await.unwrap_err();
        assert_eq!(error.failure, Failure::NoRefreshToken);
        assert!(error.to_string().contains("auth.json"));
        let missing = CodexHome::new(dir.path().join("absent"));
        let error = refresh(&client, &missing, false, 7, Utc::now())
            .await
            .unwrap_err();
        assert_eq!(error.failure, Failure::Transient);
    }

    #[tokio::test]
    async fn a_fresh_profile_is_left_alone_unless_forced() {
        let dir = tempfile::tempdir().unwrap();
        let home = CodexHome::new(dir.path());
        home.write_tokens(&TokenBundle {
            access_token: "a".into(),
            refresh_token: "r".into(),
            id_token: None,
            account_id: None,
            last_refresh: Some(Utc::now()),
        })
        .unwrap();
        let client =
            Client::with_endpoints("http://127.0.0.1:1/usage", "http://127.0.0.1:1/token", "id")
                .unwrap();
        assert_eq!(
            refresh(&client, &home, false, 7, Utc::now()).await.unwrap(),
            None
        );
        let error = refresh(&client, &home, true, 7, Utc::now())
            .await
            .unwrap_err();
        assert_eq!(error.failure, Failure::Transient);
        assert_eq!(home.read().unwrap().refresh_token(), Some("r"));
    }
}
