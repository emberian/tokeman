//! Read-only quota for subscriptions tokeman does not rotate: Antigravity,
//! the z.ai GLM Coding Plan, and Kimi Code. Each service is polled on its own
//! usage endpoint and its answer becomes a [`Usage`]; nothing here installs a
//! credential anywhere.
//!
//! Each service module exposes `pub async fn read(client: &reqwest::Client,
//! account: &ServiceAccount, now: i64) -> Usage` and never panics or returns
//! `Err`: a failure is a `Usage` with `error` set.

// Service modules (`antigravity`, `kimi`, `zai`) are declared here by the
// change that adds each one.

use serde::{Deserialize, Serialize};

use crate::quota::Window;

/// A subscription service tokeman can read quota from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Service {
    Antigravity,
    Zai,
    Kimi,
}

impl Service {
    pub const ALL: [Service; 3] = [Service::Antigravity, Service::Zai, Service::Kimi];

    pub fn name(self) -> &'static str {
        match self {
            Self::Antigravity => "antigravity",
            Self::Zai => "zai",
            Self::Kimi => "kimi",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Antigravity => "Antigravity",
            Self::Zai => "GLM (z.ai)",
            Self::Kimi => "Kimi Code",
        }
    }
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

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Amount {
    pub used: f64,
    pub limit: f64,
    /// "credits", "requests", "tokens", ...
    pub unit: String,
}

/// A money balance beside the limits (Kimi's booster wallet).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Balance {
    pub label: String,
    /// Minor units (cents).
    pub left_minor: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total_minor: Option<i64>,
    pub currency: String,
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
}
