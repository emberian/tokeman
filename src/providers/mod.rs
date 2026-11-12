//! Read-only quota for subscriptions tokeman does not rotate: Antigravity,
//! the z.ai GLM Coding Plan, and Kimi Code. Each service is polled on its own
//! usage endpoint and its answer becomes a [`Reading`]; nothing here
//! installs a credential anywhere.
//!
//! Each service module exposes `pub async fn read(client: &reqwest::Client,
//! account: &ServiceAccount, now: i64) -> Reading` and never panics or
//! returns `Err`: a failure is a `Reading` with `error` set.

pub mod antigravity;
pub mod kimi;
pub mod zai;

use std::sync::OnceLock;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::paths;
use crate::quota::{Amount, Limit, ReadError, Reading, Scope, Service, Source, Span, Window};

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
pub async fn read(client: &reqwest::Client, account: &ServiceAccount, now: i64) -> Reading {
    match account.service {
        Service::Zai => zai::read(client, account, now).await,
        Service::Kimi => kimi::read(client, account, now).await,
        Service::Antigravity => antigravity::read(client, account, now).await,
        Service::Claude | Service::Codex => failed(
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
) -> Vec<Reading> {
    let bound = READ_TIMEOUT * 2;
    futures::future::join_all(accounts.iter().map(|account| async move {
        match tokio::time::timeout(bound, read(client, account, now)).await {
            Ok(usage) => usage,
            Err(_) => Reading::failed(
                account.service,
                &account.name,
                now,
                ReadError::Network {
                    message: format!("no answer within {}s", bound.as_secs()),
                },
            ),
        }
    }))
    .await
}

/// How a service account authenticates. One `[[services]]` table in
/// tokens.toml; the secret is either inline (tokens.toml is 0600) or read
/// from a file the user already keeps, such as `~/.zai-key`.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
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

/// `<set>` for a secret that is held; never the secret.
pub(crate) fn held<T>(secret: &Option<T>) -> Option<&'static str> {
    secret.as_ref().map(|_| "<set>")
}

impl std::fmt::Debug for ServiceAccount {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ServiceAccount")
            .field("name", &self.name)
            .field("service", &self.service)
            .field("key", &held(&self.key))
            .field("key_file", &self.key_file)
            .field("refresh_token", &held(&self.refresh_token))
            .field("access_token", &held(&self.access_token))
            .field("expires_at", &self.expires_at)
            .field("label", &self.label)
            .finish()
    }
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

/// One limit as a usage endpoint reported it: scoped to the service's
/// `group` when it has one, account-wide otherwise, and named by its length
/// when the service says it.
pub(crate) fn limit(
    group: Option<String>,
    label: impl Into<String>,
    span_secs: Option<i64>,
    window: Window,
    amount: Option<Amount>,
) -> Limit {
    Limit {
        label: label.into(),
        amount,
        ..Limit::new(
            Span::from_seconds(span_secs),
            group.map_or(Scope::All, Scope::Group),
            window,
            Source::Poll,
        )
    }
}

/// A read that failed, in the service's or tokeman's own words.
pub(crate) fn failed(
    service: Service,
    account: &str,
    at: i64,
    message: impl Into<String>,
) -> Reading {
    Reading::failed(
        service,
        account,
        at,
        ReadError::Other {
            message: message.into(),
        },
    )
}

/// The key or grant could not be read or was refused for good.
pub(crate) fn credential(message: impl Into<String>) -> ReadError {
    ReadError::Credential {
        message: message.into(),
    }
}

/// The request never completed.
pub(crate) fn network(message: impl Into<String>) -> ReadError {
    ReadError::Network {
        message: message.into(),
    }
}

/// The answer was not in a shape this build reads.
pub(crate) fn decode(message: impl Into<String>) -> ReadError {
    ReadError::Decode {
        message: message.into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
        assert!(usages.iter().all(|usage| usage.limits.is_empty()));
    }

    #[test]
    fn debug_output_names_which_secrets_are_held_and_shows_none() {
        let account = ServiceAccount {
            key: Some("sk-inline-secret".into()),
            refresh_token: Some("1//refresh-secret".into()),
            access_token: Some("ya29.access-secret".into()),
            expires_at: Some(5),
            ..ServiceAccount::with_key_file(Service::Antigravity, "first@example.com", "~/.key")
        };
        let shown = format!("{account:?} {:?}", [account.clone()]);
        assert!(!shown.contains("secret"), "{shown}");
        assert!(
            shown.contains(r#"name: "first@example.com""#)
                && shown.contains(r#"refresh_token: Some("<set>")"#)
                && shown.contains(r#"key_file: Some("~/.key")"#),
            "{shown}"
        );
    }
}
