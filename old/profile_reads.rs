//! Rate-limited, cached reads of the profile-usage endpoint.
//!
//! `/api/oauth/usage` is quick to answer 429, and the rotation daemon probes
//! as often as every 20 seconds. Per-model weekly limits and reset offers move
//! over hours, so each account's read is cached on disk (shared by the daemon
//! and every CLI command) and refreshed at most every [`MAX_AGE_SECS`]. A 429
//! backs the account off; while it lasts, the last good answer is served.

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

use crate::probe::client_user_agent;

const CACHE_FILE: &str = "claude-profile-reads.json";
/// How long an answer is reused before the endpoint is asked again.
pub const MAX_AGE_SECS: i64 = 300;
const FIRST_BACKOFF_SECS: i64 = 600;
const MAX_BACKOFF_SECS: i64 = 3600;
/// Backoff after an error that is not a rate limit (network, 5xx).
const ERROR_RETRY_SECS: i64 = 60;

/// Which variant of the endpoint to read.
#[derive(Debug, Clone, Copy)]
pub enum Read {
    /// What `/usage` reads: windows and per-model limits.
    Usage,
    /// What Claude Code reads at a limit: the same, plus reset programs.
    AtWall,
}

impl Read {
    fn path(self) -> &'static str {
        match self {
            Self::Usage => "/api/oauth/usage",
            Self::AtWall => "/api/oauth/usage?at_wall=1&skip_spend=1",
        }
    }

    fn key(self, account: &str) -> String {
        match self {
            Self::Usage => format!("{account}|usage"),
            Self::AtWall => format!("{account}|at_wall"),
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct Entry {
    #[serde(default)]
    fetched_at: i64,
    #[serde(default)]
    body: Option<serde_json::Value>,
    #[serde(default)]
    retry_after: i64,
    #[serde(default)]
    rate_limited: u32,
    #[serde(default)]
    last_error: Option<String>,
}

fn cache_path() -> Result<std::path::PathBuf> {
    Ok(crate::private_fs::state_dir()?.join(CACHE_FILE))
}

fn load() -> BTreeMap<String, Entry> {
    cache_path()
        .ok()
        .and_then(|path| std::fs::read(path).ok())
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default()
}

/// Best-effort: a lost update only costs one extra read later. The file
/// holds usage figures, never credentials.
fn store(key: &str, entry: &Entry) {
    let mut cache = load();
    cache.insert(key.to_owned(), entry.clone());
    if let (Ok(path), Ok(bytes)) = (cache_path(), serde_json::to_vec_pretty(&cache)) {
        let _ = crate::private_fs::write_atomic(&path, &bytes, 0o600);
    }
}

/// The endpoint's JSON for `account`, from cache when younger than
/// [`MAX_AGE_SECS`] (unless `fresh`), or while the account is backed off.
pub async fn read(
    account: &str,
    bearer: &str,
    variant: Read,
    fresh: bool,
) -> Result<serde_json::Value> {
    let now = chrono::Utc::now().timestamp();
    let key = variant.key(account);
    let mut entry = load().remove(&key).unwrap_or_default();
    let young = now - entry.fetched_at < MAX_AGE_SECS;
    if let Some(body) = &entry.body
        && young
        && !fresh
    {
        return Ok(body.clone());
    }
    if now < entry.retry_after {
        return match &entry.body {
            Some(body) if !fresh => Ok(body.clone()),
            _ => bail!(
                "{} (backing off for {}s)",
                entry.last_error.as_deref().unwrap_or("recently refused"),
                entry.retry_after - now
            ),
        };
    }

    let outcome = fetch(bearer, variant).await;
    match outcome {
        Ok(body) => {
            entry = Entry {
                fetched_at: now,
                body: Some(body.clone()),
                ..Entry::default()
            };
            store(&key, &entry);
            Ok(body)
        }
        Err(Failure {
            message,
            rate_limited,
            retry_after,
        }) => {
            if rate_limited {
                entry.rate_limited = entry.rate_limited.saturating_add(1);
            }
            let backoff = backoff_secs(rate_limited, entry.rate_limited, retry_after);
            entry.retry_after = now + backoff;
            entry.last_error = Some(message.clone());
            store(&key, &entry);
            match entry.body {
                Some(body) if !fresh => Ok(body),
                _ => bail!("{message}"),
            }
        }
    }
}

/// 10 minutes, doubling per consecutive 429 up to an hour, or the server's
/// `Retry-After` when that is longer; other errors retry after a minute.
fn backoff_secs(rate_limited: bool, consecutive: u32, retry_after: Option<i64>) -> i64 {
    if !rate_limited {
        return ERROR_RETRY_SECS;
    }
    let doubling = FIRST_BACKOFF_SECS << consecutive.saturating_sub(1).min(6);
    doubling.min(MAX_BACKOFF_SECS).max(retry_after.unwrap_or(0))
}

struct Failure {
    message: String,
    rate_limited: bool,
    retry_after: Option<i64>,
}

async fn fetch(bearer: &str, variant: Read) -> std::result::Result<serde_json::Value, Failure> {
    let failure = |message: String| Failure {
        message,
        rate_limited: false,
        retry_after: None,
    };
    let client = reqwest::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(8))
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .map_err(|error| failure(error.to_string()))?;
    let path = variant.path();
    let response = client
        .get(format!("https://api.anthropic.com{path}"))
        .header("Authorization", format!("Bearer {bearer}"))
        .header("anthropic-beta", "oauth-2025-04-20")
        .header("User-Agent", client_user_agent())
        .send()
        .await
        .map_err(|error| failure(format!("{path} request failed: {error}")))?;
    let status = response.status();
    let retry_after = response
        .headers()
        .get("retry-after")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.trim().parse::<i64>().ok());
    let text = response.text().await.unwrap_or_default();
    if !status.is_success() {
        return Err(Failure {
            message: format!(
                "{path} answered HTTP {status}: {}",
                crate::text::one_line(&text, 160)
            ),
            rate_limited: status.as_u16() == 429,
            retry_after,
        });
    }
    serde_json::from_str(&text)
        .map_err(|error| failure(format!("unreadable {path} response: {error}")))
}

#[cfg(test)]
mod tests {
    use super::backoff_secs;

    #[test]
    fn rate_limits_back_off_exponentially_and_honor_retry_after() {
        assert_eq!(backoff_secs(true, 1, None), 600);
        assert_eq!(backoff_secs(true, 2, None), 1200);
        assert_eq!(backoff_secs(true, 9, None), 3600);
        assert_eq!(backoff_secs(true, 1, Some(7200)), 7200);
        assert_eq!(backoff_secs(false, 5, Some(7200)), 60);
    }
}
