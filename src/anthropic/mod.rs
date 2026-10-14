//! The Anthropic side: the probe, the profile endpoint, OAuth, resets.
//!
//! Every request goes through one [`Client`] that identifies as Claude Code.
//! The server derives the requesting surface from the user agent: under any
//! other agent premium models answer a bare 429, the reset programs answer
//! `ineligible_reason: "surface"`, and the token endpoint rate-limits.

pub mod oauth;
pub mod probe;
pub mod profile;
pub mod resets;

use std::sync::OnceLock;
use std::time::Duration;

use reqwest::header::{ACCEPT, CONTENT_TYPE, USER_AGENT};

use crate::account::Credential;
use crate::quota::ProbeError;

pub const API: &str = "https://api.anthropic.com";
pub const ANTHROPIC_VERSION: &str = "2023-06-01";
/// Lets a subscription OAuth token call `/v1/messages`.
pub const MESSAGES_BETA: &str = "claude-code-20250219,oauth-2025-04-20";
/// Lets a subscription OAuth token call the `/api/oauth/*` endpoints.
pub const OAUTH_BETA: &str = "oauth-2025-04-20";
/// The system prompt Claude Code opens every request with. Premium models
/// refuse subscription tokens with a bare 429 unless the request carries it.
pub const CLAUDE_CODE_IDENTITY: &str = "You are Claude Code, Anthropic's official CLI for Claude.";
/// Claude Code's token requests go out through axios with its default
/// headers; the token endpoint rate-limits other clients.
pub const TOKEN_USER_AGENT: &str = "axios/1.15.2";
/// Used when the installed `claude` cannot report its version.
const FALLBACK_CLI_VERSION: &str = "2.1.282";
const CONNECT_TIMEOUT: Duration = Duration::from_secs(8);
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);
const VERSION_QUERY_TIMEOUT: Duration = Duration::from_secs(5);

/// `claude-cli/<version> (external, cli)` for the installed Claude Code,
/// read once per process.
pub fn user_agent() -> &'static str {
    static AGENT: OnceLock<String> = OnceLock::new();
    AGENT.get_or_init(|| {
        format!(
            "claude-cli/{} (external, cli)",
            installed_cli_version()
                .as_deref()
                .unwrap_or(FALLBACK_CLI_VERSION)
        )
    })
}

/// The first dotted-number word of `claude --version`, waiting at most
/// [`VERSION_QUERY_TIMEOUT`] for it.
fn installed_cli_version() -> Option<String> {
    use std::io::Read;
    use std::process::{Command, Stdio};
    let binary = std::env::var("TOKEMAN_CLAUDE_BIN").unwrap_or_else(|_| "claude".into());
    let mut child = Command::new(binary)
        .arg("--version")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let mut stdout = child.stdout.take()?;
    let reader = std::thread::spawn(move || {
        let mut text = String::new();
        let _ = stdout.read_to_string(&mut text);
        text
    });
    let deadline = std::time::Instant::now() + VERSION_QUERY_TIMEOUT;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if std::time::Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(50));
            }
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    };
    if !status.success() {
        return None;
    }
    let text = reader.join().ok()?;
    text.split_whitespace()
        .next()
        .filter(|version| version.chars().all(|c| c.is_ascii_digit() || c == '.'))
        .map(str::to_owned)
}

/// The HTTP client every Anthropic request goes through.
#[derive(Debug, Clone)]
pub struct Client {
    http: reqwest::Client,
    api: String,
    token_url: String,
}

impl Default for Client {
    fn default() -> Self {
        Self::new()
    }
}

impl Client {
    pub fn new() -> Self {
        Self::with_base(API, oauth::TOKEN_URL)
    }

    /// One client per process; `reqwest::Client` is a handle, so clones share it.
    pub fn shared() -> &'static Client {
        static CLIENT: OnceLock<Client> = OnceLock::new();
        CLIENT.get_or_init(Client::new)
    }

    /// Talk to a different host, for tests against a local server.
    pub fn with_base(api: &str, token_url: &str) -> Self {
        let http = reqwest::Client::builder()
            .connect_timeout(CONNECT_TIMEOUT)
            .timeout(DEFAULT_TIMEOUT)
            .user_agent(user_agent())
            .build()
            .unwrap_or_default();
        Self {
            http,
            api: api.trim_end_matches('/').to_owned(),
            token_url: token_url.to_owned(),
        }
    }

    pub fn url(&self, path: &str) -> String {
        format!("{}{path}", self.api)
    }

    /// A `/v1/messages` request on `credential`: Bearer with the OAuth betas
    /// for a subscription token, `x-api-key` for an API key.
    pub(crate) fn messages(&self, credential: Credential<'_>) -> reqwest::RequestBuilder {
        let value = credential.value();
        let request = self
            .http
            .post(self.url("/v1/messages"))
            .header("anthropic-version", ANTHROPIC_VERSION)
            .header(CONTENT_TYPE, "application/json");
        if value.starts_with("sk-ant-oat01-") {
            request
                .bearer_auth(value)
                .header("anthropic-beta", MESSAGES_BETA)
        } else {
            request.header("x-api-key", value)
        }
    }

    /// A GET on an `/api/oauth/*` path with a full-scope login token.
    pub(crate) fn oauth_get(&self, path: &str, bearer: &str) -> reqwest::RequestBuilder {
        self.http
            .get(self.url(path))
            .bearer_auth(bearer)
            .header("anthropic-beta", OAUTH_BETA)
    }

    pub(crate) fn oauth_post(&self, path: &str, bearer: &str) -> reqwest::RequestBuilder {
        self.http
            .post(self.url(path))
            .bearer_auth(bearer)
            .header("anthropic-beta", OAUTH_BETA)
    }

    /// A POST to the token endpoint with axios's default headers.
    pub(crate) fn token(&self) -> reqwest::RequestBuilder {
        self.http
            .post(&self.token_url)
            .header(ACCEPT, "application/json, text/plain, */*")
            .header(CONTENT_TYPE, "application/json")
            .header(USER_AGENT, TOKEN_USER_AGENT)
    }
}

/// A request that never completed, with its cause chain in one line.
pub fn network_error(error: &reqwest::Error) -> ProbeError {
    ProbeError::Network {
        message: describe(error),
    }
}

/// `error` and its sources, joined, without the repeats reqwest adds.
pub fn describe(error: &dyn std::error::Error) -> String {
    let mut parts: Vec<String> = Vec::new();
    let mut current: Option<&dyn std::error::Error> = Some(error);
    while let Some(error) = current {
        let text = error.to_string();
        if !parts.iter().any(|part| part.contains(&text)) {
            parts.push(text);
        }
        current = error.source();
    }
    parts.join(": ")
}

/// A non-success answer. The body is kept whole: [`ProbeError::reason`]
/// reads the cause out of it.
pub fn http_error(status: reqwest::StatusCode, body: String) -> ProbeError {
    ProbeError::Http {
        status: status.as_u16(),
        body,
    }
}

/// `text` on one line, cut to `max_chars` with an ellipsis.
pub fn one_line(text: &str, max_chars: usize) -> String {
    let single_line = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if single_line.chars().count() <= max_chars {
        single_line
    } else {
        let cut: String = single_line.chars().take(max_chars).collect();
        format!("{cut}…")
    }
}

/// `Retry-After` in seconds, when the server sent one.
pub fn retry_after_secs(headers: &reqwest::header::HeaderMap) -> Option<i64> {
    headers
        .get(reqwest::header::RETRY_AFTER)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.trim().parse().ok())
}

#[cfg(test)]
pub(crate) mod testing {
    //! A loopback server the modules' tests point a [`Client`] at.

    use super::Client;
    use axum::Router;

    /// Serve `router` on a free loopback port for the rest of the test.
    pub async fn serve(router: Router) -> Client {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind a loopback port");
        let addr = listener.local_addr().expect("local address");
        tokio::spawn(async move {
            let _ = axum::serve(listener, router).await;
        });
        let base = format!("http://{addr}");
        Client::with_base(&base, &format!("{base}/v1/oauth/token"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_user_agent_is_claude_codes_shape() {
        let agent = user_agent();
        let version = agent
            .strip_prefix("claude-cli/")
            .and_then(|rest| rest.strip_suffix(" (external, cli)"))
            .expect("claude-cli/<version> (external, cli)");
        assert!(!version.is_empty());
        assert!(version.chars().all(|c| c.is_ascii_digit() || c == '.'));
    }

    #[test]
    fn one_line_flattens_and_caps_on_characters() {
        assert_eq!(one_line("a\n  b\tc", 10), "a b c");
        assert_eq!(one_line("ééééé", 3), "ééé…");
    }

    #[test]
    fn describe_joins_the_cause_chain_once() {
        #[derive(Debug)]
        struct Outer(std::io::Error);
        impl std::fmt::Display for Outer {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("request failed")
            }
        }
        impl std::error::Error for Outer {
            fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
                Some(&self.0)
            }
        }
        let error = Outer(std::io::Error::new(
            std::io::ErrorKind::TimedOut,
            "timed out",
        ));
        assert_eq!(describe(&error), "request failed: timed out");
    }

    #[test]
    fn credentials_pick_their_auth_scheme() {
        let client = Client::with_base("http://127.0.0.1:9", "http://127.0.0.1:9/t");
        let oauth = client
            .messages(Credential::Setup("sk-ant-oat01-x"))
            .build()
            .unwrap();
        assert_eq!(
            oauth.headers().get("authorization").unwrap(),
            "Bearer sk-ant-oat01-x"
        );
        assert_eq!(
            oauth.headers().get("anthropic-beta").unwrap(),
            MESSAGES_BETA
        );
        let api_key = client
            .messages(Credential::Setup("sk-ant-api03-x"))
            .build()
            .unwrap();
        assert_eq!(
            api_key.headers().get("x-api-key").unwrap(),
            "sk-ant-api03-x"
        );
        assert!(api_key.headers().get("authorization").is_none());
        let token = client.token().build().unwrap();
        assert_eq!(token.headers().get("user-agent").unwrap(), TOKEN_USER_AGENT);
    }
}
