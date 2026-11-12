//! Read-only quota for subscriptions tokeman does not rotate: Antigravity,
//! the z.ai GLM Coding Plan, and Kimi Code. Each service is polled on its own
//! usage endpoint and its answer becomes a [`Usage`]; nothing here installs a
//! credential anywhere.
//!
//! Each service module exposes `pub async fn read(client: &reqwest::Client,
//! account: &ServiceAccount, now: i64) -> Usage` and never panics or returns
//! `Err`: a failure is a `Usage` with `error` set.

pub mod agy;
pub mod antigravity;
pub mod kimi;
pub mod zai;

use std::sync::OnceLock;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::paths;
use crate::quota::Window;
pub use crate::quota::{Amount, Balance, Service};

/// Every request a service read makes is bounded by this.
pub const READ_TIMEOUT: Duration = Duration::from_secs(15);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

/// One HTTP client for every service read; `reqwest::Client` is a handle,
/// so clones share it.
pub fn client() -> &'static reqwest::Client {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            .connect_timeout(CONNECT_TIMEOUT)
            .timeout(READ_TIMEOUT)
            .user_agent(concat!("tokeman/", env!("CARGO_PKG_VERSION")))
            .build()
            .unwrap_or_default()
    })
}

/// Read one account on its service's endpoint.
pub async fn read(client: &reqwest::Client, account: &ServiceAccount, now: i64) -> Usage {
    match account.service {
        Service::Zai => zai::read(client, account, now).await,
        Service::Kimi => kimi::read(client, account, now).await,
        Service::Antigravity => antigravity::read(client, account, now).await,
        Service::Claude | Service::Codex => Usage::failed(
            account.service,
            &account.name,
            now,
            format!(
                "{} accounts are not [[services]] entries",
                account.service.label()
            ),
        ),
    }
}

/// Read every account concurrently. Each read is cut off at
/// [`READ_TIMEOUT`] plus a margin, whatever its requests do, so the whole
/// sweep is bounded; the result keeps the order of `accounts`.
pub async fn read_all(
    client: &reqwest::Client,
    accounts: &[ServiceAccount],
    now: i64,
) -> Vec<Usage> {
    let bound = READ_TIMEOUT * 2;
    futures::future::join_all(accounts.iter().map(|account| async move {
        match tokio::time::timeout(bound, read(client, account, now)).await {
            Ok(usage) => usage,
            Err(_) => Usage::failed(
                account.service,
                &account.name,
                now,
                format!("no answer within {}s", bound.as_secs()),
            ),
        }
    }))
    .await
}

/// How a service account authenticates. One `[[services]]` table in
/// tokens.toml; the secret is either inline (tokens.toml is 0600) or read
/// from a file the user already keeps, such as `~/.zai-key`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServiceAccount {
    /// Unique across services; usually the account's email or the service name.
    pub name: String,
    pub service: Service,
    /// An API key, inline.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
    /// A file holding the API key; may start with `~/`. Read on every poll,
    /// so rotating the file needs no tokeman change.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key_file: Option<String>,
    /// OAuth (Antigravity): Google refresh tokens do not rotate, so tokeman
    /// keeps its own copy and never writes back to the app's stores.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refresh_token: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub access_token: Option<String>,
    /// Epoch milliseconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
}

impl ServiceAccount {
    /// An API-key account reading its key from `key_file`.
    pub fn with_key_file(service: Service, name: &str, key_file: &str) -> Self {
        Self {
            name: name.to_owned(),
            service,
            key: None,
            key_file: Some(key_file.to_owned()),
            refresh_token: None,
            access_token: None,
            expires_at: None,
            label: None,
        }
    }

    /// The API key: inline, else the trimmed contents of `key_file`.
    pub fn api_key(&self) -> Result<String> {
        if let Some(key) = self
            .key
            .as_deref()
            .map(str::trim)
            .filter(|key| !key.is_empty())
        {
            return Ok(key.to_owned());
        }
        let Some(file) = self.key_file.as_deref() else {
            bail!("no key or key_file configured for '{}'", self.name);
        };
        let path = paths::expand_tilde(file);
        let contents = std::fs::read_to_string(&path)
            .with_context(|| format!("failed to read the key file {}", path.display()))?;
        let key = contents.trim();
        if key.is_empty() {
            bail!("the key file {} is empty", path.display());
        }
        Ok(key.to_owned())
    }
}

/// A label for a window of `span_secs`: `5h`, `weekly`, `monthly`.
pub fn span_label(span_secs: i64) -> String {
    const HOUR: i64 = 3600;
    const DAY: i64 = 24 * HOUR;
    match span_secs {
        s if s == 7 * DAY => "weekly".into(),
        s if s == DAY => "daily".into(),
        s if (28 * DAY..=31 * DAY).contains(&s) => "monthly".into(),
        s if s % DAY == 0 => format!("{}d", s / DAY),
        s if s % HOUR == 0 => format!("{}h", s / HOUR),
        s => format!("{}m", s / 60),
    }
}

/// One limit a service reports.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Meter {
    /// The service's grouping, when it has one (Antigravity: "Gemini Models",
    /// "Claude and GPT models"). `None` for single-group services.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group: Option<String>,
    /// Short label, e.g. "5h", "weekly", "monthly".
    pub label: String,
    /// Fraction used (may exceed 1.0) and the reset (0 = no reset pending,
    /// e.g. an idle window that has not started).
    pub window: Window,
    /// The window's length when the service says it, in seconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub span_secs: Option<i64>,
    /// Absolute amounts when the service reports them (credits, requests).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub amount: Option<Amount>,
}

/// One poll of one service account.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Usage {
    pub service: Service,
    pub account: String,
    /// Epoch seconds.
    pub at: i64,
    /// The plan as the service names it ("max", "g1-ultra-tier", "Vivace").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan: Option<String>,
    #[serde(default)]
    pub meters: Vec<Meter>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub balances: Vec<Balance>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl Usage {
    pub fn failed(service: Service, account: &str, at: i64, error: impl Into<String>) -> Self {
        Self {
            service,
            account: account.to_owned(),
            at,
            plan: None,
            meters: Vec::new(),
            balances: Vec::new(),
            error: Some(error.into()),
        }
    }

    /// The meter with the least room, for one-line summaries.
    pub fn tightest(&self) -> Option<&Meter> {
        self.meters
            .iter()
            .max_by(|a, b| a.window.used.total_cmp(&b.window.used))
    }

    /// The fraction left on the tightest meter, counting only the meters
    /// whose group contains `group` (any case) when one is given. `None` for
    /// a failed read or when no meter matches.
    pub fn room(&self, group: Option<&str>) -> Option<f64> {
        if self.error.is_some() {
            return None;
        }
        let wanted = group.map(str::to_lowercase);
        self.meters
            .iter()
            .filter(|meter| match (&wanted, &meter.group) {
                (None, _) => true,
                (Some(wanted), Some(group)) => group.to_lowercase().contains(wanted),
                (Some(_), None) => false,
            })
            .map(|meter| meter.window.remaining())
            .min_by(f64::total_cmp)
    }
}

/// The accounts of `service` that answered, most room first; ties keep the
/// order of `usages`.
pub fn by_room<'a>(
    usages: &'a [Usage],
    service: Service,
    group: Option<&str>,
) -> Vec<(&'a Usage, f64)> {
    let mut ranked: Vec<_> = usages
        .iter()
        .filter(|usage| usage.service == service)
        .filter_map(|usage| Some((usage, usage.room(group)?)))
        .collect();
    ranked.sort_by(|a, b| b.1.total_cmp(&a.1));
    ranked
}

/// A response body cut to one short line, for an error message.
pub(crate) fn snippet(body: &str) -> String {
    const MAX_CHARS: usize = 160;
    let single = body.split_whitespace().collect::<Vec<_>>().join(" ");
    match single.char_indices().nth(MAX_CHARS) {
        Some((cut, _)) => format!("{}…", &single[..cut]),
        None => single,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn usage(account: &str, meters: &[(&str, f64)]) -> Usage {
        Usage {
            meters: meters
                .iter()
                .map(|(group, used)| Meter {
                    group: Some((*group).into()),
                    label: "weekly".into(),
                    window: Window::new(*used, 0),
                    span_secs: None,
                    amount: None,
                })
                .collect(),
            error: None,
            ..Usage::failed(Service::Antigravity, account, 0, "")
        }
    }

    #[test]
    fn accounts_rank_by_their_tightest_meter_within_the_asked_group() {
        let usages = [
            usage(
                "a",
                &[("Gemini Models", 0.9), ("Claude and GPT models", 0.1)],
            ),
            usage(
                "b",
                &[("Gemini Models", 0.3), ("Claude and GPT models", 0.5)],
            ),
            Usage::failed(Service::Antigravity, "down", 0, "refused"),
            Usage {
                service: Service::Zai,
                ..usage("glm", &[("x", 0.0)])
            },
        ];
        let names = |group: Option<&str>| -> Vec<&str> {
            by_room(&usages, Service::Antigravity, group)
                .iter()
                .map(|(usage, _)| usage.account.as_str())
                .collect()
        };
        // Overall, a's Gemini limit (10% left) is tighter than b's worst (50%).
        assert_eq!(names(None), ["b", "a"]);
        assert_eq!(names(Some("claude")), ["a", "b"]);
        assert_eq!(names(Some("gemini")), ["b", "a"]);
        assert_eq!(names(Some("no such group")), Vec::<&str>::new());
        let ranked = by_room(&usages, Service::Antigravity, Some("claude"));
        assert!((ranked[0].1 - 0.9).abs() < 1e-9);
    }

    #[test]
    fn the_key_comes_inline_or_trimmed_from_its_file() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("key");
        std::fs::write(&file, "  sk-file-key\n").unwrap();
        let mut account =
            ServiceAccount::with_key_file(Service::Kimi, "kimi", file.to_str().unwrap());
        assert_eq!(account.api_key().unwrap(), "sk-file-key");
        account.key = Some("sk-inline".into());
        assert_eq!(account.api_key().unwrap(), "sk-inline");
        account.key = Some("  ".into());
        std::fs::write(&file, "\n").unwrap();
        assert!(account.api_key().unwrap_err().to_string().contains("empty"));
        account.key_file = None;
        assert!(account.api_key().is_err());
    }

    #[test]
    fn spans_read_as_words() {
        assert_eq!(span_label(5 * 3600), "5h");
        assert_eq!(span_label(7 * 86_400), "weekly");
        assert_eq!(span_label(30 * 86_400), "monthly");
        assert_eq!(span_label(2 * 86_400), "2d");
        assert_eq!(span_label(90 * 60), "90m");
    }

    #[tokio::test]
    async fn read_all_keeps_order_and_answers_for_every_account() {
        let accounts = [
            ServiceAccount::with_key_file(Service::Zai, "glm", "/nonexistent/a"),
            ServiceAccount::with_key_file(Service::Kimi, "second", "/nonexistent/b"),
            ServiceAccount::with_key_file(Service::Kimi, "kimi", "/nonexistent/c"),
        ];
        let usages = read_all(&reqwest::Client::new(), &accounts, 7).await;
        let names: Vec<_> = usages
            .iter()
            .map(|usage| (usage.service, usage.account.as_str(), usage.at))
            .collect();
        assert_eq!(
            names,
            [
                (Service::Zai, "glm", 7),
                (Service::Kimi, "second", 7),
                (Service::Kimi, "kimi", 7)
            ]
        );
        assert!(usages.iter().all(|usage| usage.error.is_some()));
    }
}
