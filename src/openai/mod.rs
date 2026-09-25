//! Codex (OpenAI / ChatGPT OAuth) support.
//!
//! Anthropic and Codex differ in a way that shapes this whole module: Claude
//! Code reads one global credential slot, so `rotation.rs` has to time-slice a
//! single identity. Codex resolves auth per request
//! (`core/src/client.rs::current_client_setup`), and `$CODEX_HOME` is a
//! per-process env var, so several codexes can hold different accounts at once
//! and a live one can be migrated between accounts without restarting.
//!
//! The migration is only safe when tokeman *drives* the reload. Codex guards
//! its 401 auto-recovery path with an account-id equality check
//! (`login/src/auth/manager.rs::reload_if_account_id_matches`): if a process
//! discovers a swapped `auth.json` by way of a 401, recovery fails permanently
//! for that turn. The plain `reload()` has no such guard, and the app-server
//! calls it after `account/login/start`. So we always announce a swap over the
//! app-server rather than letting a 401 stumble onto it.

pub mod appserver;
pub mod authfile;
pub mod cli;
pub mod probe;
pub mod refresh;

/// Shared HTTP client for the ChatGPT backend.
///
/// Two deliberate settings, both learned from being served Cloudflare challenge
/// pages instead of JSON:
///
/// * **HTTP/1.1 only.** reqwest negotiates h2 over ALPN by default; the edge is
///   markedly more willing to challenge h2 requests carrying these headers.
/// * **Cookie store.** A challenged response sets `__cf_bm`, and replaying it
///   is what stops the next request being challenged too. One client must be
///   shared across every probe — a per-request client throws the cookie away
///   and makes a fleet sweep look like N unrelated bots.
///
/// Getting this wrong does not fail loudly; it reports healthy accounts as dead
/// and parks working capacity, which is the worst failure mode a rotator has.
pub fn http_client(timeout_secs: u64) -> reqwest::Result<reqwest::Client> {
    reqwest::Client::builder()
        .http1_only()
        .cookie_store(true)
        .timeout(std::time::Duration::from_secs(timeout_secs))
        .build()
}

/// Whether a response is a Cloudflare bot challenge rather than a real answer
/// from the API. These are probabilistic, so callers retry rather than treating
/// the account as dead.
pub fn is_edge_challenge(response: &reqwest::Response) -> bool {
    response.status() == reqwest::StatusCode::FORBIDDEN
        && (response.headers().contains_key("cf-mitigated")
            || response
                .headers()
                .get(reqwest::header::CONTENT_TYPE)
                .and_then(|value| value.to_str().ok())
                .is_some_and(|value| value.starts_with("text/html")))
}
