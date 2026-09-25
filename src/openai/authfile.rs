//! Reading and writing `$CODEX_HOME/auth.json`.
//!
//! Codex stores more in this file than the OAuth bundle — `agent_identity`,
//! `personal_access_token`, and `bedrock_api_key` all live alongside it. We
//! mutate the parsed JSON in place instead of round-tripping through a typed
//! struct so that fields tokeman does not model survive a write untouched.

use anyhow::{Context, Result, bail};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use chrono::{DateTime, Utc};
use serde::Serialize;
use serde_json::{Value, json};
use std::path::{Path, PathBuf};

/// A Codex profile directory: whatever `CODEX_HOME` would point at.
#[derive(Debug, Clone)]
pub struct CodexHome {
    path: PathBuf,
}

impl CodexHome {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    /// The default `~/.codex`, matching Codex's own fallback.
    pub fn default_home() -> Result<Self> {
        let home = dirs::home_dir().context("could not determine home directory")?;
        Ok(Self::new(home.join(".codex")))
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn auth_path(&self) -> PathBuf {
        self.path.join("auth.json")
    }

    pub fn exists(&self) -> bool {
        self.auth_path().exists()
    }

    pub fn read(&self) -> Result<AuthFile> {
        let path = self.auth_path();
        let contents = std::fs::read_to_string(&path)
            .with_context(|| format!("failed to read {}", path.display()))?;
        let raw: Value = serde_json::from_str(&contents)
            .with_context(|| format!("failed to parse {}", path.display()))?;
        Ok(AuthFile { raw })
    }

    /// Replace the ChatGPT OAuth bundle, preserving every other key.
    ///
    /// The write is atomic because a torn `auth.json` is unrecoverable: the
    /// refresh token it holds is the only way back to a working session.
    pub fn write_tokens(&self, tokens: &TokenBundle) -> Result<()> {
        let mut raw = if self.exists() {
            self.read()?.raw
        } else {
            json!({})
        };

        let obj = raw
            .as_object_mut()
            .context("auth.json is not a JSON object")?;
        obj.insert("auth_mode".into(), json!("chatgpt"));
        obj.entry("OPENAI_API_KEY").or_insert(Value::Null);

        let slot = obj.entry("tokens").or_insert_with(|| json!({}));
        if !slot.is_object() {
            *slot = json!({});
        }
        let slot = slot
            .as_object_mut()
            .expect("tokens slot was just made an object");
        slot.insert("access_token".into(), json!(tokens.access_token));
        slot.insert("refresh_token".into(), json!(tokens.refresh_token));
        if let Some(id_token) = &tokens.id_token {
            slot.insert("id_token".into(), json!(id_token));
        }
        if let Some(account_id) = &tokens.account_id {
            slot.insert("account_id".into(), json!(account_id));
        }
        obj.insert(
            "last_refresh".into(),
            json!(tokens.last_refresh.unwrap_or_else(Utc::now)),
        );

        crate::private_fs::write_atomic(
            &self.auth_path(),
            serde_json::to_string_pretty(&raw)?.as_bytes(),
            0o600,
        )
    }
}

/// The parsed contents of an `auth.json`.
#[derive(Debug, Clone)]
pub struct AuthFile {
    raw: Value,
}

impl AuthFile {
    fn token_str(&self, key: &str) -> Option<&str> {
        self.raw.get("tokens")?.get(key)?.as_str()
    }

    pub fn access_token(&self) -> Option<&str> {
        self.token_str("access_token")
    }

    /// Empty counts as absent: access-only profiles store `""` here, and
    /// presenting that to the token endpoint would only earn a remote error.
    pub fn refresh_token(&self) -> Option<&str> {
        self.token_str("refresh_token")
            .filter(|token| !token.is_empty())
    }

    pub fn id_token(&self) -> Option<&str> {
        self.token_str("id_token")
    }

    pub fn account_id(&self) -> Option<&str> {
        self.token_str("account_id")
    }

    pub fn last_refresh(&self) -> Option<DateTime<Utc>> {
        let raw = self.raw.get("last_refresh")?.as_str()?;
        DateTime::parse_from_rfc3339(raw)
            .ok()
            .map(|dt| dt.with_timezone(&Utc))
    }

    /// Codex refreshes proactively once `last_refresh` is roughly this old.
    /// Staying ahead of it is what keeps a parked account off the 401 path,
    /// where the account-id guard would otherwise bite during a swap.
    pub fn is_stale(&self, max_age_days: i64) -> bool {
        match self.last_refresh() {
            Some(when) => Utc::now() - when > chrono::Duration::days(max_age_days),
            None => true,
        }
    }

    /// Non-secret claims carried by the id_token.
    pub fn claims(&self) -> Option<IdClaims> {
        IdClaims::decode(self.id_token()?)
    }

    /// The `account_id` Codex would send as `chatgpt-account-id`, preferring
    /// the stored value and falling back to the id_token claim.
    pub fn effective_account_id(&self) -> Option<String> {
        if let Some(id) = self.account_id() {
            return Some(id.to_string());
        }
        self.claims()?.chatgpt_account_id
    }

    pub fn bundle(&self) -> Option<TokenBundle> {
        Some(TokenBundle {
            access_token: self.access_token()?.to_string(),
            refresh_token: self.refresh_token()?.to_string(),
            id_token: self.id_token().map(str::to_string),
            account_id: self.effective_account_id(),
            last_refresh: self.last_refresh(),
        })
    }
}

/// The ChatGPT OAuth material tokeman moves between profiles.
#[derive(Debug, Clone, Serialize)]
pub struct TokenBundle {
    pub access_token: String,
    pub refresh_token: String,
    pub id_token: Option<String>,
    pub account_id: Option<String>,
    pub last_refresh: Option<DateTime<Utc>>,
}

/// Claims Codex itself reads out of the id_token.
#[derive(Debug, Clone, Default, Serialize)]
pub struct IdClaims {
    pub email: Option<String>,
    pub chatgpt_account_id: Option<String>,
    pub chatgpt_plan_type: Option<String>,
    pub expires_at: Option<i64>,
}

impl IdClaims {
    pub fn decode(jwt: &str) -> Option<Self> {
        let payload = jwt.split('.').nth(1)?;
        let bytes = URL_SAFE_NO_PAD.decode(payload).ok()?;
        let value: Value = serde_json::from_slice(&bytes).ok()?;
        // Codex namespaces its own claims under this URI.
        let auth = value.get("https://api.openai.com/auth");
        let str_at = |v: Option<&Value>, key: &str| v?.get(key)?.as_str().map(str::to_string);
        Some(Self {
            email: str_at(Some(&value), "email"),
            chatgpt_account_id: str_at(auth, "chatgpt_account_id"),
            chatgpt_plan_type: str_at(auth, "chatgpt_plan_type"),
            expires_at: value.get("exp").and_then(Value::as_i64),
        })
    }
}

/// Copy a bundle into another profile. Used to seat an account in a lane.
pub fn seat(bundle: &TokenBundle, target: &CodexHome) -> Result<()> {
    if bundle.access_token.is_empty() || bundle.refresh_token.is_empty() {
        bail!("refusing to seat an incomplete token bundle");
    }
    target.write_tokens(bundle)
}

/// Seed a profile from an access token alone, with no refresh token.
///
/// For externally-supplied ChatGPT access tokens handed over without their
/// refresh token. The access token already carries the email/plan/account
/// claims codex reads, so it doubles as the id_token. This is explicitly a
/// stopgap: the profile works until the access token expires (a few days) and
/// has no refresh material to renew with, so it will 401 and stay dead after
/// that. tokeman marks such accounts so a sweep does not mistake the eventual
/// 401 for a revocation it can fix.
pub fn seat_access_only(access_token: &str, account_id: &str, target: &CodexHome) -> Result<()> {
    if access_token.is_empty() {
        bail!("refusing to seat an empty access token");
    }
    let bundle = TokenBundle {
        access_token: access_token.to_string(),
        refresh_token: String::new(),
        id_token: Some(access_token.to_string()),
        account_id: Some(account_id.to_string()),
        last_refresh: Some(Utc::now()),
    };
    target.write_tokens(&bundle)
}
