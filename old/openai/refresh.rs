//! ChatGPT OAuth token refresh.
//!
//! Request shape mirrors Codex's own `RefreshRequest`
//! (`login/src/auth/manager.rs:1618`) exactly — `client_id`, `grant_type`,
//! `refresh_token`, no scope — so that tokeman and Codex are interchangeable
//! refreshers for the same account.
//!
//! The refresh token ROTATES. The presented one dies server-side the instant
//! the request succeeds, and re-presenting it earns `refresh_token_reused`,
//! which is terminal. Everything here is arranged so the new bundle reaches
//! disk before any other operation is allowed to fail.

use anyhow::{Context, Result, bail};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::openai::authfile::{CodexHome, TokenBundle};

/// Codex's OAuth client id (`login/src/auth/manager.rs:1632`).
pub const CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
pub const CLIENT_ID_OVERRIDE_ENV_VAR: &str = "CODEX_APP_SERVER_LOGIN_CLIENT_ID";

/// `login/src/auth/manager.rs:194`.
pub const REFRESH_TOKEN_URL: &str = "https://auth.openai.com/oauth/token";
pub const REFRESH_TOKEN_URL_OVERRIDE_ENV_VAR: &str = "CODEX_REFRESH_TOKEN_URL_OVERRIDE";

/// Codex refreshes proactively at roughly this age; we stay a little ahead so a
/// parked profile never reaches the 401 path, where the account-id guard would
/// block a swap.
pub const DEFAULT_MAX_AGE_DAYS: i64 = 7;

#[derive(Serialize)]
struct RefreshRequest {
    client_id: String,
    grant_type: &'static str,
    refresh_token: String,
}

#[derive(Deserialize, Clone, Default)]
struct RefreshResponse {
    id_token: Option<String>,
    access_token: Option<String>,
    refresh_token: Option<String>,
}

/// Why a refresh failed, mirroring Codex's `classify_refresh_token_failure`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefreshFailure {
    /// The refresh token aged out. Re-login required.
    Expired,
    /// The token was already redeemed — someone else rotated it out from under
    /// us. Re-login required, and it means two refreshers are racing.
    Reused,
    /// Revoked server-side, e.g. the user signed out elsewhere.
    Invalidated,
    /// Network or unclassified server error; retrying later is reasonable.
    Transient,
}

impl RefreshFailure {
    pub fn is_permanent(self) -> bool {
        !matches!(self, Self::Transient)
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Expired => "refresh token expired — re-login required",
            Self::Reused => "refresh token already redeemed — another refresher is racing us",
            Self::Invalidated => "refresh token revoked — re-login required",
            Self::Transient => "transient refresh failure",
        }
    }
}

#[derive(Debug)]
pub struct RefreshError {
    pub failure: RefreshFailure,
    pub detail: String,
}

impl std::fmt::Display for RefreshError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.failure.label(), self.detail)
    }
}

impl std::error::Error for RefreshError {}

impl RefreshError {
    fn transient(detail: impl std::fmt::Display) -> Self {
        Self {
            failure: RefreshFailure::Transient,
            detail: detail.to_string(),
        }
    }
}

fn client_id() -> String {
    std::env::var(CLIENT_ID_OVERRIDE_ENV_VAR)
        .ok()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| CLIENT_ID.to_string())
}

fn endpoint() -> String {
    std::env::var(REFRESH_TOKEN_URL_OVERRIDE_ENV_VAR)
        .unwrap_or_else(|_| REFRESH_TOKEN_URL.to_string())
}

/// The OAuth error code in a failure body — `{"error": "code"}`,
/// `{"error": {"code": …}}`, or a top-level `code` — lowercased, or "".
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

fn classify(status: reqwest::StatusCode, body: &str) -> RefreshFailure {
    match error_code(body).as_str() {
        "refresh_token_expired" => RefreshFailure::Expired,
        "refresh_token_reused" => RefreshFailure::Reused,
        "refresh_token_invalidated" => RefreshFailure::Invalidated,
        // A 401 with no recognized code is still terminal for this token.
        _ if status == reqwest::StatusCode::UNAUTHORIZED => RefreshFailure::Invalidated,
        _ => RefreshFailure::Transient,
    }
}

/// Exchange a refresh token for a fresh bundle. Does not touch disk — callers
/// that own a profile should use [`refresh_home`], which persists atomically.
pub async fn exchange(refresh_token: &str) -> std::result::Result<TokenBundle, RefreshError> {
    let client = crate::openai::http_client(30).map_err(RefreshError::transient)?;

    let response = client
        .post(endpoint())
        .header("Content-Type", "application/json")
        .header("User-Agent", "codex-cli")
        .json(&RefreshRequest {
            client_id: client_id(),
            grant_type: "refresh_token",
            refresh_token: refresh_token.to_string(),
        })
        .send()
        .await
        .map_err(RefreshError::transient)?;

    let status = response.status();
    let body = response.text().await.unwrap_or_default();

    if !status.is_success() {
        let flattened = body.split_whitespace().collect::<Vec<_>>().join(" ");
        return Err(RefreshError {
            failure: classify(status, &body),
            detail: format!(
                "HTTP {status}: {}",
                flattened.chars().take(200).collect::<String>()
            ),
        });
    }

    let parsed: RefreshResponse = serde_json::from_str(&body).map_err(|err| {
        RefreshError::transient(format!("failed to decode refresh response: {err}"))
    })?;

    let access_token = parsed
        .access_token
        .ok_or_else(|| RefreshError::transient("refresh response carried no access_token"))?;

    Ok(TokenBundle {
        // Codex keeps the prior refresh token when the response omits one.
        refresh_token: parsed
            .refresh_token
            .unwrap_or_else(|| refresh_token.to_string()),
        id_token: parsed.id_token,
        account_id: None,
        last_refresh: Some(Utc::now()),
        access_token,
    })
}

/// Refresh the profile at `home` in place.
///
/// Ordering is the entire point: the rotated bundle is written to disk before
/// this function does anything that could fail. Losing the new refresh token
/// after the server has already invalidated the old one locks the account out
/// permanently, and no backup can undo it.
pub async fn refresh_home(home: &CodexHome) -> Result<TokenBundle> {
    let auth = home.read()?;
    let Some(refresh_token) = auth.refresh_token() else {
        bail!("{} has no refresh_token", home.auth_path().display());
    };
    // Preserve the account id: the refresh response does not echo it back, and
    // dropping it would strip the `chatgpt-account-id` header.
    let account_id = auth.effective_account_id();

    let mut bundle = exchange(refresh_token)
        .await
        .with_context(|| format!("refreshing {}", home.path().display()))?;
    bundle.account_id = account_id.or_else(|| {
        bundle
            .id_token
            .as_deref()
            .and_then(crate::openai::authfile::IdClaims::decode)
            .and_then(|claims| claims.chatgpt_account_id)
    });

    home.write_tokens(&bundle)?;
    Ok(bundle)
}

/// Recover the typed failure from an error returned by [`refresh_home`].
///
/// A permanent failure means no amount of retrying helps — the account needs an
/// interactive `codex login` — so the daemon must say so rather than logging an
/// indistinguishable error every half hour forever.
pub fn failure_kind(error: &anyhow::Error) -> Option<RefreshFailure> {
    error
        .chain()
        .find_map(|cause| cause.downcast_ref::<RefreshError>())
        .map(|refresh_error| refresh_error.failure)
}

/// Refresh only if the stored bundle is older than `max_age_days`.
/// Returns `Ok(None)` when the profile was already fresh.
pub async fn refresh_if_stale(home: &CodexHome, max_age_days: i64) -> Result<Option<TokenBundle>> {
    let auth = home.read()?;
    if !auth.is_stale(max_age_days) {
        return Ok(None);
    }
    refresh_home(home).await.map(Some)
}

/// `force` refreshes unconditionally; otherwise only when older than
/// `max_age_days`. Returns `Ok(None)` when the profile was left alone.
pub async fn refresh(
    home: &CodexHome,
    force: bool,
    max_age_days: i64,
) -> Result<Option<TokenBundle>> {
    if force {
        refresh_home(home).await.map(Some)
    } else {
        refresh_if_stale(home, max_age_days).await
    }
}
