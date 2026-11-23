//! `$CODEX_HOME/auth.json`: the ChatGPT OAuth bundle, read and written in
//! place so the keys tokeman does not model (`agent_identity`,
//! `personal_access_token`, `bedrock_api_key`, ...) survive a write.

use anyhow::{Context, Result, bail};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use chrono::{DateTime, Utc};
use serde::Serialize;
use serde_json::{Map, Value, json};
use std::path::{Path, PathBuf};

use crate::fs::{FileLock, resolve_link_chain, write_atomic};

/// A Codex profile directory: whatever `CODEX_HOME` would point at.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodexHome {
    path: PathBuf,
}

impl CodexHome {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    /// `~/.codex`, Codex's own fallback.
    pub fn default_home() -> Result<Self> {
        let home = crate::paths::home();
        if !home.is_absolute() {
            bail!("could not determine home directory");
        }
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
        if !raw.is_object() {
            bail!("{} is not a JSON object", path.display());
        }
        Ok(AuthFile { raw })
    }

    /// Acquire the advisory lock for this profile's resolved `auth.json`
    /// (`<resolved>.json.lock`), matching upstream `openai/codex` so tokeman
    /// and Codex serialize read-modify-write updates to a shared auth file.
    pub fn lock(&self) -> Result<FileLock> {
        let resolved = resolve_link_chain(&self.auth_path())?;
        let lock_path = resolved.with_extension("json.lock");
        FileLock::acquire_announced(
            &lock_path,
            &format!("Codex auth lock {}", lock_path.display()),
        )
    }

    /// Replace the OAuth bundle, keeping every other key. Atomic: a torn
    /// auth.json loses the only copy of the refresh token.
    pub fn write_tokens(&self, tokens: &TokenBundle) -> Result<()> {
        let _lock = self.lock()?;
        let existing = if self.exists() {
            Some(self.read()?)
        } else {
            None
        };
        let same_account = existing
            .as_ref()
            .and_then(AuthFile::effective_account_id)
            .is_some_and(|prior| tokens.account_id.as_deref() == Some(prior.as_str()));
        let mut raw = existing.map_or_else(|| json!({}), |file| file.raw);
        let object = raw
            .as_object_mut()
            .context("auth.json is not a JSON object")?;
        object.insert("auth_mode".into(), json!("chatgpt"));
        object.entry("OPENAI_API_KEY").or_insert(Value::Null);

        let mut slot = match object.remove("tokens") {
            Some(Value::Object(map)) => map,
            _ => Map::new(),
        };
        slot.insert("access_token".into(), json!(tokens.access_token));
        slot.insert("refresh_token".into(), json!(tokens.refresh_token));
        match &tokens.id_token {
            Some(id_token) => {
                slot.insert("id_token".into(), json!(id_token));
            }
            None if !same_account => {
                slot.remove("id_token");
            }
            None => {}
        }
        match &tokens.account_id {
            Some(account_id) => {
                slot.insert("account_id".into(), json!(account_id));
            }
            None if !same_account => {
                slot.remove("account_id");
            }
            None => {}
        }
        object.insert("tokens".into(), Value::Object(slot));
        object.insert(
            "last_refresh".into(),
            json!(tokens.last_refresh.unwrap_or_else(Utc::now)),
        );

        write_atomic(
            &self.auth_path(),
            serde_json::to_string_pretty(&raw)?.as_bytes(),
            0o600,
        )
    }
}

/// The parsed contents of an `auth.json`.
#[derive(Clone, PartialEq, Eq)]
pub struct AuthFile {
    raw: Value,
}

impl std::fmt::Debug for AuthFile {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AuthFile")
            .field("account_id", &self.effective_account_id())
            .field("last_refresh", &self.last_refresh())
            .finish_non_exhaustive()
    }
}

impl AuthFile {
    fn token_str(&self, key: &str) -> Option<&str> {
        self.raw.get("tokens")?.get(key)?.as_str()
    }

    pub fn access_token(&self) -> Option<&str> {
        self.token_str("access_token")
    }

    /// Empty counts as absent: access-only profiles store `""` here.
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
            .map(|at| at.with_timezone(&Utc))
    }

    /// Older than `max_age_days` at `now`, or never refreshed. Codex refreshes
    /// on its own at about this age; staying ahead keeps a parked profile off
    /// the 401 path.
    pub fn is_stale(&self, max_age_days: i64, now: DateTime<Utc>) -> bool {
        match self.last_refresh() {
            Some(when) => now - when > chrono::Duration::days(max_age_days),
            None => true,
        }
    }

    /// The non-secret claims of the id_token.
    pub fn claims(&self) -> Option<IdClaims> {
        IdClaims::decode(self.id_token()?)
    }

    /// The id Codex sends as `chatgpt-account-id`: the stored value, else the
    /// id_token claim.
    pub fn effective_account_id(&self) -> Option<String> {
        match self.account_id() {
            Some(id) => Some(id.to_owned()),
            None => self.claims()?.chatgpt_account_id,
        }
    }

    /// The movable bundle, if the file holds both tokens.
    pub fn bundle(&self) -> Option<TokenBundle> {
        Some(TokenBundle {
            access_token: self.access_token()?.to_owned(),
            refresh_token: self.refresh_token()?.to_owned(),
            id_token: self.id_token().map(str::to_owned),
            account_id: self.effective_account_id(),
            last_refresh: self.last_refresh(),
        })
    }
}

/// The ChatGPT OAuth material tokeman moves between profiles.
#[derive(Clone, PartialEq, Eq)]
pub struct TokenBundle {
    pub access_token: String,
    pub refresh_token: String,
    pub id_token: Option<String>,
    pub account_id: Option<String>,
    pub last_refresh: Option<DateTime<Utc>>,
}

impl std::fmt::Debug for TokenBundle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TokenBundle")
            .field("account_id", &self.account_id)
            .field("last_refresh", &self.last_refresh)
            .finish_non_exhaustive()
    }
}

/// The claims Codex reads out of the id_token.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize)]
pub struct IdClaims {
    pub email: Option<String>,
    pub chatgpt_account_id: Option<String>,
    pub chatgpt_plan_type: Option<String>,
    /// Epoch seconds.
    pub expires_at: Option<i64>,
}

impl IdClaims {
    pub fn decode(jwt: &str) -> Option<Self> {
        let payload = jwt.split('.').nth(1)?.trim_end_matches('=');
        let bytes = URL_SAFE_NO_PAD.decode(payload).ok()?;
        let value: Value = serde_json::from_slice(&bytes).ok()?;
        // Codex's own claims live under this URI.
        let auth = value.get("https://api.openai.com/auth");
        let profile = value.get("https://api.openai.com/profile");
        let text = |v: Option<&Value>, key: &str| v?.get(key)?.as_str().map(str::to_owned);
        let expires_at = value.get("exp").and_then(|exp| {
            exp.as_i64().or_else(|| {
                exp.as_f64()
                    .filter(|f| f.is_finite() && *f >= i64::MIN as f64 && *f <= i64::MAX as f64)
                    .map(|f| f as i64)
            })
        });
        Some(Self {
            email: text(Some(&value), "email").or_else(|| text(profile, "email")),
            chatgpt_account_id: text(auth, "chatgpt_account_id"),
            chatgpt_plan_type: text(auth, "chatgpt_plan_type"),
            expires_at,
        })
    }
}

/// Link `target/auth.json` to `source`'s resolved `auth.json` so both share one
/// single-use `refresh_token` and one `.json.lock` advisory lock instead of
/// splitting refresh state across two files. If `target/auth.json` is an
/// existing regular file and `auth.json.bak` does not exist yet, backs it up
/// once (`0600`) and returns the backup path.
pub fn link_profile(source: &CodexHome, target: &CodexHome) -> Result<Option<PathBuf>> {
    let source_bundle = source
        .read()?
        .bundle()
        .with_context(|| format!("{} has no usable OAuth bundle", source.path().display()))?;
    if source_bundle.access_token.is_empty() || source_bundle.refresh_token.is_empty() {
        bail!("refusing to link an incomplete token bundle");
    }
    let resolved_source = resolve_link_chain(&source.auth_path())?;
    let resolved_source = if resolved_source.is_absolute() {
        resolved_source
    } else {
        std::env::current_dir()
            .context("failed to determine current directory")?
            .join(resolved_source)
    };
    let target_auth = target.auth_path();
    let mut backup = None;
    if let Ok(meta) = std::fs::symlink_metadata(&target_auth)
        && !meta.file_type().is_symlink()
    {
        let backup_path = target.path().join("auth.json.bak");
        if std::fs::symlink_metadata(&backup_path).is_err() {
            let bytes = std::fs::read(&target_auth)
                .with_context(|| format!("failed to read {}", target_auth.display()))?;
            write_atomic(&backup_path, &bytes, 0o600)?;
            backup = Some(backup_path);
        }
    }
    #[cfg(unix)]
    {
        crate::fs::write_symlink_atomic(&target_auth, &resolved_source)?;
    }
    #[cfg(not(unix))]
    {
        seat(&source_bundle, target)?;
    }
    Ok(backup)
}

/// Copy a bundle into another profile.
pub fn seat(bundle: &TokenBundle, target: &CodexHome) -> Result<()> {
    if bundle.access_token.is_empty() || bundle.refresh_token.is_empty() {
        bail!("refusing to seat an incomplete token bundle");
    }
    target.write_tokens(bundle)
}

/// Seed a profile from an access token alone. The token carries the claims
/// Codex reads, so it doubles as the id_token; with no refresh token the
/// profile dies when it expires, and the account must be marked `access_only`.
pub fn seat_access_only(access_token: &str, account_id: &str, target: &CodexHome) -> Result<()> {
    if access_token.is_empty() {
        bail!("refusing to seat an empty access token");
    }
    target.write_tokens(&TokenBundle {
        access_token: access_token.to_owned(),
        refresh_token: String::new(),
        id_token: Some(access_token.to_owned()),
        account_id: Some(account_id.to_owned()),
        last_refresh: Some(Utc::now()),
    })
}

/// A directory name for an account: `ops@example.com` is `ops-example`.
pub fn slugify(name: &str) -> String {
    let (local, domain) = name.split_once('@').unwrap_or((name, ""));
    let domain_head = domain.split('.').next().unwrap_or("");
    let raw = if domain_head.is_empty() {
        local.to_owned()
    } else {
        format!("{local}-{domain_head}")
    };
    let mapped: String = raw
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    let collapsed = mapped
        .split('-')
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join("-");
    if collapsed.is_empty() {
        "account".to_owned()
    } else {
        collapsed
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// An unsigned JWT with `claims` as its payload.
    pub(crate) fn jwt(claims: Value) -> String {
        let header = URL_SAFE_NO_PAD.encode(br#"{"alg":"none"}"#);
        let payload = URL_SAFE_NO_PAD.encode(serde_json::to_vec(&claims).unwrap());
        format!("{header}.{payload}.sig")
    }

    /// A bundle refreshed just now with no id_token or account id.
    pub(crate) fn bundle(access_token: &str, refresh_token: &str) -> TokenBundle {
        TokenBundle {
            access_token: access_token.into(),
            refresh_token: refresh_token.into(),
            id_token: None,
            account_id: None,
            last_refresh: Some(Utc::now()),
        }
    }

    pub(crate) fn id_token(email: &str, account_id: &str, plan: &str) -> String {
        jwt(json!({
            "email": email,
            "exp": 1_900_000_000,
            "https://api.openai.com/auth": {
                "chatgpt_account_id": account_id,
                "chatgpt_plan_type": plan,
            },
        }))
    }

    #[test]
    fn id_claims_come_from_the_namespaced_payload() {
        let claims = IdClaims::decode(&id_token("a@b.c", "acct-1", "plus")).unwrap();
        assert_eq!(claims.email.as_deref(), Some("a@b.c"));
        assert_eq!(claims.chatgpt_account_id.as_deref(), Some("acct-1"));
        assert_eq!(claims.chatgpt_plan_type.as_deref(), Some("plus"));
        assert_eq!(claims.expires_at, Some(1_900_000_000));
        assert_eq!(IdClaims::decode("not-a-jwt"), None);
        assert_eq!(IdClaims::decode("a.!!!.c"), None);
    }

    #[test]
    fn writing_tokens_keeps_unmodelled_keys_and_lands_owner_only() {
        let dir = tempfile::tempdir().unwrap();
        let home = CodexHome::new(dir.path().join("profile"));
        assert!(!home.exists());
        std::fs::create_dir_all(home.path()).unwrap();
        std::fs::write(
            home.auth_path(),
            r#"{"agent_identity":{"id":"x"},"tokens":{"access_token":"old","extra":"kept"},"OPENAI_API_KEY":"sk"}"#,
        )
        .unwrap();
        home.write_tokens(&TokenBundle {
            account_id: Some("acct".into()),
            last_refresh: Some(
                DateTime::parse_from_rfc3339("2026-01-02T03:04:05Z")
                    .unwrap()
                    .into(),
            ),
            ..bundle("new-access", "new-refresh")
        })
        .unwrap();
        let auth = home.read().unwrap();
        assert_eq!(auth.access_token(), Some("new-access"));
        assert_eq!(auth.refresh_token(), Some("new-refresh"));
        assert_eq!(auth.account_id(), Some("acct"));
        assert_eq!(auth.id_token(), None);
        assert_eq!(auth.raw["agent_identity"]["id"], "x");
        assert_eq!(auth.raw["tokens"]["extra"], "kept");
        assert_eq!(auth.raw["OPENAI_API_KEY"], "sk");
        assert_eq!(auth.raw["auth_mode"], "chatgpt");
        assert_eq!(
            auth.last_refresh().unwrap().to_rfc3339(),
            "2026-01-02T03:04:05+00:00"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(home.auth_path())
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600);
        }
    }

    #[test]
    fn a_non_object_tokens_slot_is_replaced() {
        let dir = tempfile::tempdir().unwrap();
        let home = CodexHome::new(dir.path());
        std::fs::write(home.auth_path(), r#"{"tokens":"garbage"}"#).unwrap();
        home.write_tokens(&TokenBundle {
            last_refresh: None,
            ..bundle("a", "r")
        })
        .unwrap();
        assert_eq!(home.read().unwrap().access_token(), Some("a"));
        std::fs::write(home.auth_path(), "[1,2]").unwrap();
        assert!(home.read().is_err());
    }

    #[test]
    fn an_empty_refresh_token_is_absent_and_blocks_seating() {
        let dir = tempfile::tempdir().unwrap();
        let home = CodexHome::new(dir.path());
        seat_access_only(&id_token("a@b.c", "acct-9", "pro"), "acct-9", &home).unwrap();
        let auth = home.read().unwrap();
        assert_eq!(auth.refresh_token(), None);
        assert_eq!(auth.bundle(), None);
        assert_eq!(auth.effective_account_id().as_deref(), Some("acct-9"));
        assert_eq!(auth.claims().unwrap().email.as_deref(), Some("a@b.c"));
        assert!(!auth.is_stale(7, Utc::now()));
        assert!(auth.is_stale(7, Utc::now() + chrono::Duration::days(8)));
        assert!(seat_access_only("", "acct", &home).is_err());
        assert!(seat(&bundle("a", ""), &home).is_err());
    }

    #[test]
    fn the_account_id_falls_back_to_the_id_token_claim() {
        let dir = tempfile::tempdir().unwrap();
        let home = CodexHome::new(dir.path());
        std::fs::write(
            home.auth_path(),
            json!({"tokens": {"access_token": "a", "refresh_token": "r", "id_token": id_token("e@x.y", "from-claim", "team")}}).to_string(),
        )
        .unwrap();
        let auth = home.read().unwrap();
        assert_eq!(auth.effective_account_id().as_deref(), Some("from-claim"));
        assert!(auth.is_stale(7, Utc::now()), "no last_refresh is stale");
        let bundle = auth.bundle().unwrap();
        assert_eq!(bundle.account_id.as_deref(), Some("from-claim"));
        let other = CodexHome::new(dir.path().join("other"));
        seat(&bundle, &other).unwrap();
        assert_eq!(other.read().unwrap().account_id(), Some("from-claim"));
        // A stored id is what Codex sends, whatever the id_token says.
        other
            .write_tokens(&TokenBundle {
                account_id: Some("stored".into()),
                ..bundle
            })
            .unwrap();
        let auth = other.read().unwrap();
        assert_eq!(
            auth.claims().unwrap().chatgpt_account_id.as_deref(),
            Some("from-claim")
        );
        assert_eq!(auth.effective_account_id().as_deref(), Some("stored"));
    }

    #[test]
    fn slugs_keep_the_domain_head_only() {
        assert_eq!(slugify("ops@example.com"), "ops-example");
        assert_eq!(slugify("plain name"), "plain-name");
        assert_eq!(slugify("a.b@c"), "a-b-c");
        assert_eq!(slugify("@@@"), "account");
        assert_eq!(slugify("..foo..@bar"), "foo-bar");
    }

    #[test]
    fn switching_accounts_clears_prior_id_token_and_link_profile_backs_up_once() {
        let dir = tempfile::tempdir().unwrap();
        let source = CodexHome::new(dir.path().join("source"));
        source
            .write_tokens(&TokenBundle {
                id_token: Some(id_token("a@b.c", "acct-a", "plus")),
                account_id: Some("acct-a".into()),
                ..bundle("access-a", "refresh-a")
            })
            .unwrap();

        // Writing a different account with no id_token clears acct-a's id_token.
        let other = CodexHome::new(dir.path().join("other"));
        other
            .write_tokens(&TokenBundle {
                id_token: Some(id_token("a@b.c", "acct-a", "plus")),
                account_id: Some("acct-a".into()),
                ..bundle("access-a", "refresh-a")
            })
            .unwrap();
        other
            .write_tokens(&TokenBundle {
                account_id: Some("acct-b".into()),
                ..bundle("access-b", "refresh-b")
            })
            .unwrap();
        assert_eq!(other.read().unwrap().id_token(), None);
        assert_eq!(other.read().unwrap().account_id(), Some("acct-b"));

        // Linking into a profile with an existing regular auth.json backs it up
        // once and makes auth.json a symlink sharing source's refresh state.
        let backup = link_profile(&source, &other).unwrap();
        assert_eq!(backup, Some(other.path().join("auth.json.bak")));
        #[cfg(unix)]
        assert!(
            std::fs::symlink_metadata(other.auth_path())
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(other.read().unwrap().access_token(), Some("access-a"));
        source
            .write_tokens(&TokenBundle {
                account_id: Some("acct-a".into()),
                ..bundle("access-a2", "refresh-a2")
            })
            .unwrap();
        assert_eq!(other.read().unwrap().access_token(), Some("access-a2"));
        assert_eq!(link_profile(&source, &other).unwrap(), None);
    }

    #[test]
    fn debug_output_of_a_bundle_or_an_auth_file_shows_no_token() {
        let dir = tempfile::tempdir().unwrap();
        let home = CodexHome::new(dir.path());
        let bundle = TokenBundle {
            id_token: Some(id_token("a@b.c", "acct-1", "plus")),
            account_id: Some("acct-1".into()),
            ..bundle("access-secret", "refresh-secret")
        };
        home.write_tokens(&bundle).unwrap();
        let auth = home.read().unwrap();
        let shown = format!("{bundle:?} {auth:?} {:?}", Some(&bundle));
        assert!(!shown.contains("secret"), "{shown}");
        assert!(
            !shown.contains(bundle.id_token.as_deref().unwrap()),
            "{shown}"
        );
        assert!(shown.contains("acct-1"), "{shown}");
    }
}
