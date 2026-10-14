//! Codex (OpenAI) accounts: probe, refresh, seat and migrate running Codexes.
//!
//! Codex resolves auth per request from `$CODEX_HOME/auth.json`, so several
//! Codexes can hold different accounts at once and a live one can be moved
//! between accounts. The move is announced over the app-server socket: Codex's
//! 401 recovery refuses an `auth.json` whose account id changed, while the
//! app-server reload path does not.

pub mod appserver;
pub mod auth;
pub mod commands;
pub mod probe;
pub mod refresh;

use anyhow::Result;
use std::time::Duration;

/// `GET` here answers the usage probe.
pub const USAGE_URL: &str = "https://chatgpt.com/backend-api/codex/usage";
pub const USAGE_URL_ENV: &str = "TOKEMAN_CODEX_USAGE_URL";
/// Codex's token endpoint and OAuth client id; tokeman and Codex refresh the
/// same account interchangeably by using the same ones.
pub const TOKEN_URL: &str = "https://auth.openai.com/oauth/token";
pub const TOKEN_URL_ENV: &str = "CODEX_REFRESH_TOKEN_URL_OVERRIDE";
pub const CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
pub const CLIENT_ID_ENV: &str = "CODEX_APP_SERVER_LOGIN_CLIENT_ID";

const TIMEOUT: Duration = Duration::from_secs(30);

/// The HTTP client every Codex request goes through.
///
/// HTTP/1.1 only and one cookie jar: the ChatGPT edge challenges h2 requests
/// carrying Codex's headers, and the `__cf_bm` cookie a challenge sets is what
/// clears the next request, so a sweep must share one jar.
#[derive(Debug, Clone)]
pub struct Client {
    http: reqwest::Client,
    usage_url: String,
    token_url: String,
    client_id: String,
}

impl Client {
    /// The production endpoints, each overridable by its environment variable.
    pub fn new() -> Result<Self> {
        let env = |name: &str, default: &str| {
            std::env::var(name)
                .ok()
                .filter(|value| !value.trim().is_empty())
                .unwrap_or_else(|| default.to_owned())
        };
        Self::with_endpoints(
            env(USAGE_URL_ENV, USAGE_URL),
            env(TOKEN_URL_ENV, TOKEN_URL),
            env(CLIENT_ID_ENV, CLIENT_ID),
        )
    }

    pub fn with_endpoints(
        usage_url: impl Into<String>,
        token_url: impl Into<String>,
        client_id: impl Into<String>,
    ) -> Result<Self> {
        let http = reqwest::Client::builder()
            .http1_only()
            .cookie_store(true)
            .timeout(TIMEOUT)
            .build()?;
        Ok(Self {
            http,
            usage_url: usage_url.into(),
            token_url: token_url.into(),
            client_id: client_id.into(),
        })
    }

    pub fn http(&self) -> &reqwest::Client {
        &self.http
    }

    pub fn usage_url(&self) -> &str {
        &self.usage_url
    }

    pub fn token_url(&self) -> &str {
        &self.token_url
    }

    pub fn client_id(&self) -> &str {
        &self.client_id
    }
}

/// A Cloudflare bot challenge rather than an answer from the API. Challenges
/// are probabilistic; callers retry instead of treating the account as dead.
pub fn is_edge_challenge(response: &reqwest::Response) -> bool {
    response.status() == reqwest::StatusCode::FORBIDDEN
        && (response.headers().contains_key("cf-mitigated")
            || response
                .headers()
                .get(reqwest::header::CONTENT_TYPE)
                .and_then(|value| value.to_str().ok())
                .is_some_and(|value| value.starts_with("text/html")))
}

/// Collapse whitespace and cap the length, so an error body fits a table row.
pub(crate) fn one_line(text: &str, max_chars: usize) -> String {
    let single = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if single.chars().count() <= max_chars {
        single
    } else {
        let cut: String = single.chars().take(max_chars).collect();
        format!("{cut}…")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_line_flattens_and_caps_on_characters() {
        assert_eq!(one_line("  a\n  b\t c ", 10), "a b c");
        assert_eq!(one_line("héllo wörld", 5), "héllo…");
    }
}
