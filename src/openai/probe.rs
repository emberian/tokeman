//! Quota probing for Codex accounts.
//!
//! `GET /backend-api/codex/usage` answers everything rotation needs in one
//! read-only call: which windows exist, how full they are, when they reset, and
//! whether the backend currently considers the account usable. It is also the
//! only reliable liveness check — `codex login status` merely parses the local
//! file and will happily report "Logged in using ChatGPT" for an account the
//! backend rejects with a 401.

use anyhow::Result;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::config::CodexAccount;
use crate::openai::authfile::{AuthFile, CodexHome};

pub const USAGE_URL: &str = "https://chatgpt.com/backend-api/codex/usage";
/// Redirect the usage probe, for tests against a local stand-in.
pub const USAGE_URL_OVERRIDE_ENV_VAR: &str = "TOKEMAN_CODEX_USAGE_URL";

fn usage_url() -> String {
    std::env::var(USAGE_URL_OVERRIDE_ENV_VAR)
        .ok()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| USAGE_URL.to_string())
}

/// Codex identifies itself this way; the backend varies behaviour by originator.
const ORIGINATOR: &str = "codex_cli_rs";

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
pub struct CodexWindow {
    #[serde(default)]
    pub used_percent: Option<f64>,
    #[serde(default)]
    pub limit_window_seconds: Option<i64>,
    #[serde(default)]
    pub reset_after_seconds: Option<i64>,
    #[serde(default)]
    pub reset_at: Option<i64>,
}

impl CodexWindow {
    /// Fraction of the window still available, in the same orientation as
    /// `rotation::remaining` uses for Anthropic tokens.
    pub fn remaining(&self) -> Option<f64> {
        self.used_percent
            .map(|used| (1.0 - used / 100.0).clamp(0.0, 1.0))
    }
}

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
pub struct CodexRateLimit {
    #[serde(default)]
    pub allowed: Option<bool>,
    #[serde(default)]
    pub limit_reached: Option<bool>,
    #[serde(default)]
    pub primary_window: Option<CodexWindow>,
    #[serde(default)]
    pub secondary_window: Option<CodexWindow>,
}

impl CodexRateLimit {
    /// The smaller of the two windows' remaining fractions: whichever bites first.
    pub fn remaining(&self) -> Option<f64> {
        [&self.primary_window, &self.secondary_window]
            .into_iter()
            .flatten()
            .filter_map(CodexWindow::remaining)
            .reduce(f64::min)
    }
}

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
pub struct CodexCredits {
    #[serde(default)]
    pub has_credits: Option<bool>,
    #[serde(default)]
    pub unlimited: Option<bool>,
    #[serde(default)]
    pub overage_limit_reached: Option<bool>,
    /// The backend is inconsistent here: `null` on one account and the string
    /// `"0"` on another. Keep it raw and coerce on read.
    #[serde(default)]
    pub balance: Option<serde_json::Value>,
}

/// Per-model limits, e.g. a separate weekly bucket for Codex-Spark. These are
/// the Codex analogue of tokeman's Anthropic per-model windows.
#[derive(Debug, Clone, Deserialize, Serialize, Default)]
pub struct AdditionalRateLimit {
    #[serde(default)]
    pub limit_name: Option<String>,
    #[serde(default)]
    pub metered_feature: Option<String>,
    #[serde(default)]
    pub rate_limit: Option<CodexRateLimit>,
}

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
pub struct ReachedType {
    #[serde(rename = "type", default)]
    pub kind: Option<String>,
}

/// The subset of `/backend-api/codex/usage` tokeman acts on.
#[derive(Debug, Clone, Deserialize, Serialize, Default)]
pub struct CodexUsage {
    #[serde(default)]
    pub user_id: Option<String>,
    #[serde(default)]
    pub account_id: Option<String>,
    #[serde(default)]
    pub email: Option<String>,
    #[serde(default)]
    pub plan_type: Option<String>,
    #[serde(default)]
    pub rate_limit: Option<CodexRateLimit>,
    #[serde(default)]
    pub credits: Option<CodexCredits>,
    #[serde(default)]
    pub rate_limit_reached_type: Option<ReachedType>,
    #[serde(default)]
    pub additional_rate_limits: Vec<AdditionalRateLimit>,
}

/// Decode a usage payload without letting one unexpected field type blackhole
/// the whole reading.
///
/// This endpoint is not a stable contract — `credits.balance` already arrives
/// as both `null` and `"0"` depending on the account. A rotation daemon that
/// reports "account dead" because a cosmetic field changed shape would park
/// perfectly good capacity, so on a strict-decode failure we retry against just
/// the subtree rotation actually depends on.
fn decode_usage(body: &str) -> Result<CodexUsage, String> {
    match serde_json::from_str::<CodexUsage>(body) {
        Ok(usage) => Ok(usage),
        Err(strict_err) => {
            let value: serde_json::Value = serde_json::from_str(body)
                .map_err(|err| format!("failed to parse usage response as JSON: {err}"))?;
            let rate_limit = value
                .get("rate_limit")
                .and_then(|v| serde_json::from_value(v.clone()).ok());
            if rate_limit.is_none() {
                return Err(format!("failed to decode usage response: {strict_err}"));
            }
            let text = |key: &str| value.get(key).and_then(|v| v.as_str()).map(str::to_string);
            Ok(CodexUsage {
                user_id: text("user_id"),
                account_id: text("account_id"),
                email: text("email"),
                plan_type: text("plan_type"),
                rate_limit,
                credits: None,
                rate_limit_reached_type: value
                    .get("rate_limit_reached_type")
                    .and_then(|v| serde_json::from_value(v.clone()).ok()),
                additional_rate_limits: Vec::new(),
            })
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct CodexProbeResult {
    pub account_name: String,
    pub codex_home: String,
    pub probed_at: DateTime<Utc>,
    pub usage: Option<CodexUsage>,
    /// Set when the backend rejected the token; the caller should refresh.
    pub unauthorized: bool,
    pub error: Option<String>,
    /// Local signals, available even when the network call fails.
    pub last_refresh: Option<DateTime<Utc>>,
    pub email_hint: Option<String>,
}

impl CodexProbeResult {
    fn new(
        account_name: &str,
        home: &CodexHome,
        auth: Option<&AuthFile>,
        outcome: Result<CodexUsage, (String, bool)>,
    ) -> Self {
        let (usage, unauthorized, error) = match outcome {
            Ok(usage) => (Some(usage), false, None),
            Err((error, unauthorized)) => (None, unauthorized, Some(error)),
        };
        Self {
            account_name: account_name.to_string(),
            codex_home: home.path().display().to_string(),
            probed_at: Utc::now(),
            usage,
            unauthorized,
            error,
            last_refresh: auth.and_then(AuthFile::last_refresh),
            email_hint: auth
                .and_then(AuthFile::claims)
                .and_then(|claims| claims.email),
        }
    }

    pub fn primary(&self) -> Option<&CodexWindow> {
        self.usage
            .as_ref()?
            .rate_limit
            .as_ref()?
            .primary_window
            .as_ref()
    }

    pub fn secondary(&self) -> Option<&CodexWindow> {
        self.usage
            .as_ref()?
            .rate_limit
            .as_ref()?
            .secondary_window
            .as_ref()
    }

    /// Least-remaining across the windows the backend reported. `None` means we
    /// have no reading at all, which is different from "no headroom".
    pub fn remaining(&self) -> Option<f64> {
        self.usage.as_ref()?.rate_limit.as_ref()?.remaining()
    }

    /// Whether the backend would currently accept work on this account.
    pub fn is_viable(&self) -> bool {
        if self.error.is_some() {
            return false;
        }
        let Some(rate_limit) = self.usage.as_ref().and_then(|u| u.rate_limit.as_ref()) else {
            // A 200 with no rate_limit block means no limit is being enforced.
            return self.usage.is_some();
        };
        if rate_limit.limit_reached == Some(true) {
            return false;
        }
        // `allowed` is authoritative when present; absence is not a rejection.
        rate_limit.allowed.unwrap_or(true)
    }

    /// Per-model buckets, e.g. GPT-5.3-Codex-Spark. These are metered
    /// separately from the account-wide window, so an account can be capped on
    /// one and wide open on the other.
    pub fn additional(&self) -> &[AdditionalRateLimit] {
        self.usage
            .as_ref()
            .map(|usage| usage.additional_rate_limits.as_slice())
            .unwrap_or(&[])
    }

    /// Remaining headroom for a named bucket, matched loosely against either
    /// `limit_name` or `metered_feature` so callers can just say "spark".
    pub fn remaining_for(&self, limit: &str) -> Option<f64> {
        let needle = limit.to_ascii_lowercase();
        let entry = self.additional().iter().find(|candidate| {
            candidate
                .limit_name
                .as_deref()
                .is_some_and(|name| name.to_ascii_lowercase().contains(&needle))
                || candidate
                    .metered_feature
                    .as_deref()
                    .is_some_and(|feature| feature.to_ascii_lowercase().contains(&needle))
        })?;
        entry.rate_limit.as_ref()?.remaining()
    }

    pub fn limit_reason(&self) -> Option<&str> {
        self.usage
            .as_ref()?
            .rate_limit_reached_type
            .as_ref()?
            .kind
            .as_deref()
    }

    pub fn email(&self) -> Option<&str> {
        self.usage
            .as_ref()
            .and_then(|u| u.email.as_deref())
            .or(self.email_hint.as_deref())
    }

    pub fn plan(&self) -> Option<&str> {
        self.usage.as_ref()?.plan_type.as_deref()
    }
}

/// How many times to re-attempt after a Cloudflare challenge. The first retry
/// already carries the `__cf_bm` cookie the challenge just set, which is
/// usually enough.
const CHALLENGE_RETRIES: u32 = 3;

/// Probe one Codex profile with a caller-supplied client so a sweep shares one
/// cookie jar. Never fails for an expected failure — a dead account is data the
/// daemon needs, not an abort.
async fn probe_with(client: &reqwest::Client, account: &CodexAccount) -> CodexProbeResult {
    let home = account.home();
    match home.read() {
        Ok(auth) => {
            let outcome = fetch_usage(client, &account.name, &auth).await;
            CodexProbeResult::new(&account.name, &home, Some(&auth), outcome)
        }
        Err(err) => {
            CodexProbeResult::new(&account.name, &home, None, Err((err.to_string(), false)))
        }
    }
}

/// The network half of a probe. The error carries whether the backend rejected
/// the token (HTTP 401), which tells the caller to refresh.
async fn fetch_usage(
    client: &reqwest::Client,
    account_name: &str,
    auth: &AuthFile,
) -> Result<CodexUsage, (String, bool)> {
    let access_token = auth
        .access_token()
        .ok_or_else(|| ("auth.json has no access_token".to_string(), false))?;

    let account_id = auth.effective_account_id();
    let mut attempt = 0;
    let response = loop {
        let mut request = client
            .get(usage_url())
            .bearer_auth(access_token)
            .header("originator", ORIGINATOR)
            .header("User-Agent", "codex-cli");
        if let Some(account_id) = &account_id {
            request = request.header("chatgpt-account-id", account_id);
        }

        let response = request
            .send()
            .await
            .map_err(|err| (err.to_string(), false))?;

        if crate::openai::is_edge_challenge(&response) && attempt < CHALLENGE_RETRIES {
            attempt += 1;
            // Back off a little; the retry now carries the cookie the
            // challenge set, which usually clears it.
            tokio::time::sleep(std::time::Duration::from_millis(400 * u64::from(attempt))).await;
            continue;
        }
        break response;
    };

    let status = response.status();
    if !status.is_success() {
        let version = format!("{:?}", response.version());
        let challenged = crate::openai::is_edge_challenge(&response);
        if std::env::var("TOKEMAN_CODEX_DEBUG").is_ok() {
            eprintln!(
                "--- {account_name} {status} {version} (attempts: {})",
                attempt + 1
            );
            for (name, value) in response.headers() {
                eprintln!("    {name}: {}", value.to_str().unwrap_or("<binary>"));
            }
        }
        let body = response.text().await.unwrap_or_default();
        let detail = if challenged {
            format!(
                "HTTP {status} ({version}): Cloudflare challenge after {} attempts — not an auth failure",
                attempt + 1
            )
        } else {
            // Flattened so a JSON error body stays readable in a table row.
            format!(
                "HTTP {status} ({version}): {}",
                crate::text::one_line(&body, 160)
            )
        };
        return Err((detail, status == reqwest::StatusCode::UNAUTHORIZED));
    }

    let body = response
        .text()
        .await
        .map_err(|err| (err.to_string(), false))?;
    decode_usage(&body).map_err(|err| (err, false))
}

/// Probe every configured account, sharing one client so the whole sweep looks
/// like a single well-behaved session to Cloudflare.
///
/// The first request is made alone: it is the one that may be challenged, and
/// letting it settle seeds the cookie jar for the rest, which then run
/// concurrently.
pub async fn probe_all(accounts: &[CodexAccount]) -> Vec<CodexProbeResult> {
    let client = match crate::openai::http_client(30) {
        Ok(client) => client,
        Err(err) => {
            return accounts
                .iter()
                .map(|account| {
                    CodexProbeResult::new(
                        &account.name,
                        &account.home(),
                        None,
                        Err((err.to_string(), false)),
                    )
                })
                .collect();
        }
    };
    let mut results = Vec::with_capacity(accounts.len());
    let Some((first, rest)) = accounts.split_first() else {
        return results;
    };
    results.push(probe_with(&client, first).await);
    results.extend(
        futures::future::join_all(rest.iter().map(|account| probe_with(&client, account))).await,
    );
    results
}
