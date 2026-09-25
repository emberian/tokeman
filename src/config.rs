use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Token {
    pub name: String,
    /// Long-lived `claude setup-token` credential. It is inference-only, so a
    /// Claude session running on it cannot send /feedback, use Remote Control,
    /// or read profile usage. Empty for an account enrolled only through
    /// `tokeman login`.
    #[serde(default)]
    pub key: String,
    /// OAuth access token from `tokeman login` (or the older `usage capture`).
    /// Unlike `key` it can carry `user:profile`, but it expires within hours.
    #[serde(default, alias = "usage_key", skip_serializing_if = "Option::is_none")]
    pub access_token: Option<String>,
    /// Refresh material from `tokeman login`. Single-use: every refresh
    /// returns a replacement, and redeeming a stale one kills the grant, which
    /// is why every write to this file goes through [`Config::update`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refresh_token: Option<String>,
    /// Access-token expiry, epoch milliseconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<i64>,
    /// When the access token was minted, epoch milliseconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub obtained_at: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scopes: Option<Vec<String>>,
    /// Why the refresh token was permanently refused. While set, the account
    /// falls back to its setup token until `tokeman login` runs again.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub login_error: Option<String>,
}

/// Refresh this long before expiry. Claude keeps using its cached token until
/// the token is refused, then adopts whatever `settings.json` holds, so the
/// replacement only has to be in place before the old one dies.
pub const REFRESH_MARGIN_MS: i64 = 30 * 60 * 1000;

/// Never refresh a token younger than this, whatever its expiry says. Bounds
/// the refresh rate if the provider ever issues tokens shorter than the margin.
const MIN_REFRESH_AGE_MS: i64 = 5 * 60 * 1000;

/// An installed access token must outlive this, or a session would adopt a
/// credential that is refused before its first request completes.
const MIN_INSTALL_LIFETIME_MS: i64 = 2 * 60 * 1000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CredentialKind {
    /// Refreshable `tokeman login` access token.
    Login,
    /// `claude setup-token` key.
    Setup,
}

/// The credential tokeman would hand Claude (or use to probe) for an account.
#[derive(Debug, Clone, Copy)]
pub struct Credential<'a> {
    pub value: &'a str,
    pub kind: CredentialKind,
    /// Epoch milliseconds; `None` for setup tokens, which do not expire soon.
    pub expires_at: Option<i64>,
    pub scopes: &'a [String],
}

impl Token {
    pub fn setup_key(&self) -> Option<&str> {
        Some(self.key.as_str()).filter(|key| !key.is_empty())
    }

    /// A refresh token that has not been refused.
    pub fn is_refreshable(&self) -> bool {
        self.refresh_token.is_some() && self.login_error.is_none()
    }

    /// The login access token, if it is refreshable and still has room to be
    /// adopted by a session.
    fn login_credential(&self, now_ms: i64) -> Option<Credential<'_>> {
        if !self.is_refreshable() {
            return None;
        }
        let value = self.access_token.as_deref()?;
        if self
            .expires_at
            .is_some_and(|expires_at| expires_at - now_ms < MIN_INSTALL_LIFETIME_MS)
        {
            return None;
        }
        Some(Credential {
            value,
            kind: CredentialKind::Login,
            expires_at: self.expires_at,
            scopes: self.scopes.as_deref().unwrap_or_default(),
        })
    }

    /// What rotation installs for this account: the full-scope login token
    /// when one is live, otherwise the inference-only setup token.
    pub fn credential(&self, now_ms: i64) -> Option<Credential<'_>> {
        self.login_credential(now_ms).or_else(|| {
            self.setup_key().map(|value| Credential {
                value,
                kind: CredentialKind::Setup,
                expires_at: None,
                scopes: &[],
            })
        })
    }

    /// Token for the read-only profile-usage endpoint. A captured access token
    /// without refresh material still counts until it expires.
    pub fn usage_credential(&self, now_ms: i64) -> Option<&str> {
        let value = self.access_token.as_deref()?;
        if self
            .expires_at
            .is_some_and(|expires_at| expires_at <= now_ms)
        {
            return None;
        }
        Some(value)
    }

    pub fn needs_refresh(&self, now_ms: i64) -> bool {
        if !self.is_refreshable() {
            return false;
        }
        let Some(expires_at) = self.expires_at else {
            // No recorded expiry: refresh once so the sweep learns it.
            return self.access_token.is_none() || self.obtained_at.is_none();
        };
        let old_enough = self
            .obtained_at
            .is_none_or(|obtained_at| now_ms - obtained_at >= MIN_REFRESH_AGE_MS);
        expires_at <= now_ms || (expires_at - now_ms <= REFRESH_MARGIN_MS && old_enough)
    }

    /// Whether `value` is one of this account's current credentials.
    pub fn holds(&self, value: &str) -> bool {
        self.setup_key() == Some(value) || self.access_token.as_deref() == Some(value)
    }

    /// Store a fresh login grant. The provider may or may not rotate the
    /// refresh token; keep the old one only when it did not send a new one.
    pub fn apply_login(&mut self, bundle: crate::claude_login::Bundle, now_ms: i64) {
        self.access_token = Some(bundle.access_token);
        if bundle.refresh_token.is_some() {
            self.refresh_token = bundle.refresh_token;
        }
        self.expires_at = bundle.expires_at;
        self.obtained_at = Some(now_ms);
        if !bundle.scopes.is_empty() {
            self.scopes = Some(bundle.scopes);
        }
        self.login_error = None;
    }
}

/// A Codex profile directory tokeman can probe, refresh, and seat accounts into.
///
/// We store the *directory*, never the tokens: Codex owns `auth.json` and
/// rotates the refresh token inside it. Duplicating that material into
/// `tokens.toml` would create a second writer racing Codex for a
/// single-redemption token, and losing that race is unrecoverable
/// (`refresh_token_reused`).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CodexAccount {
    pub name: String,
    /// Absolute path used as `CODEX_HOME`.
    pub codex_home: String,
    /// Optional human label, e.g. "personal pro" vs "lunar.town team".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    /// Excluded from automatic seating while true; still probed.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub parked: bool,
    /// Seeded from an access token with no refresh token. Such a profile cannot
    /// be renewed and will die when the access token expires, so the daemon
    /// must not treat its eventual 401 as a revocation to refresh away.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub access_only: bool,
}

impl CodexAccount {
    pub fn home(&self) -> crate::openai::authfile::CodexHome {
        crate::openai::authfile::CodexHome::new(expand_tilde(&self.codex_home))
    }
}

pub(crate) fn expand_tilde(path: &str) -> PathBuf {
    if let Some(rest) = path.strip_prefix("~/")
        && let Some(home) = dirs::home_dir()
    {
        return home.join(rest);
    }
    PathBuf::from(path)
}

/// The inverse of [`expand_tilde`], for display and for storing portable paths.
pub(crate) fn tilde(path: &std::path::Path) -> String {
    let display = path.display().to_string();
    match dirs::home_dir().map(|home| home.display().to_string()) {
        Some(home) if display.starts_with(&home) => display.replacen(&home, "~", 1),
        _ => display,
    }
}

fn default_probe_interval() -> u64 {
    30
}

fn default_normal_min_5h() -> f64 {
    0.10
}

/// Rotate while a tenth of the week is still unspent.
///
/// This used to be 0.05. A switch does not stop the outgoing account being
/// drained: sessions bound to it keep their launch credential until they exit,
/// and that tail was measured at 1% of the weekly quota at the median but 28%
/// at the worst across 70 switches. A 5% floor cannot absorb that, which is how
/// an account tokeman had already rotated away from still reached 96% used.
/// Ten percent leaves room for the tail to land in.
fn default_normal_min_7d() -> f64 {
    0.10
}

fn default_sip_min_5h() -> f64 {
    0.02
}

fn default_sip_min_7d() -> f64 {
    0.03
}

fn default_normal_rotation_interval() -> u64 {
    120
}

/// Normalized headroom held back per live session already bound to a candidate.
///
/// A Claude process keeps the credential it resolved at startup, so sessions
/// bound to an account are committed future burn that the quota probe has not
/// observed yet. Landing new work on an account that three sessions are
/// already draining spends headroom twice. Small on purpose: this only reorders
/// otherwise-viable candidates, and 0.0 restores the previous behavior.
fn default_per_session_reserve() -> f64 {
    0.02
}

fn default_sip_rotation_interval() -> u64 {
    20
}

/// Policy for hot-rotating Claude Code's shared OAuth token.
///
/// Values are fractions of capacity *remaining*, not utilization. Once no token
/// satisfies both normal floors, tokeman enters sip-and-drain mode and samples
/// more frequently while allowing each token to drain to the lower floors.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RotationSettings {
    #[serde(default = "default_normal_min_5h")]
    pub normal_min_5h_remaining: f64,
    #[serde(default = "default_normal_min_7d")]
    pub normal_min_7d_remaining: f64,
    #[serde(default = "default_sip_min_5h")]
    pub sip_min_5h_remaining: f64,
    #[serde(default = "default_sip_min_7d")]
    pub sip_min_7d_remaining: f64,
    #[serde(default = "default_normal_rotation_interval")]
    pub normal_probe_interval_secs: u64,
    #[serde(default = "default_sip_rotation_interval")]
    pub sip_probe_interval_secs: u64,
    #[serde(default = "default_per_session_reserve")]
    pub per_session_reserve: f64,
}

impl Default for RotationSettings {
    fn default() -> Self {
        Self {
            normal_min_5h_remaining: default_normal_min_5h(),
            normal_min_7d_remaining: default_normal_min_7d(),
            sip_min_5h_remaining: default_sip_min_5h(),
            sip_min_7d_remaining: default_sip_min_7d(),
            normal_probe_interval_secs: default_normal_rotation_interval(),
            sip_probe_interval_secs: default_sip_rotation_interval(),
            per_session_reserve: default_per_session_reserve(),
        }
    }
}

impl RotationSettings {
    pub fn validate(&self) -> Result<()> {
        for (name, value) in [
            ("normal_min_5h_remaining", self.normal_min_5h_remaining),
            ("normal_min_7d_remaining", self.normal_min_7d_remaining),
            ("sip_min_5h_remaining", self.sip_min_5h_remaining),
            ("sip_min_7d_remaining", self.sip_min_7d_remaining),
        ] {
            if !(0.0..1.0).contains(&value) {
                bail!("rotation.{name} must be at least 0 and less than 1");
            }
        }
        if self.sip_min_5h_remaining > self.normal_min_5h_remaining {
            bail!("rotation.sip_min_5h_remaining must not exceed the normal floor");
        }
        if self.sip_min_7d_remaining > self.normal_min_7d_remaining {
            bail!("rotation.sip_min_7d_remaining must not exceed the normal floor");
        }
        if self.normal_probe_interval_secs == 0 || self.sip_probe_interval_secs == 0 {
            bail!("rotation probe intervals must be at least one second");
        }
        if !(0.0..1.0).contains(&self.per_session_reserve) {
            bail!("rotation.per_session_reserve must be at least 0 and less than 1");
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LaunchSettings {
    /// Extra arguments to pass to claude (e.g. ["--model", "opus"])
    #[serde(default)]
    pub launch_args: Vec<String>,
    /// Whether --dangerously-skip-permissions is enabled
    #[serde(default)]
    pub dangerous_mode: bool,
    /// Preferred terminal emulator (auto-detect if None)
    #[serde(default)]
    pub terminal: Option<String>,
    /// Override path to the claude binary
    #[serde(default)]
    pub claude_bin: Option<String>,
    /// Probe interval in seconds for tray mode
    #[serde(default = "default_probe_interval")]
    pub probe_interval_secs: u64,
}

impl LaunchSettings {
    /// The Claude binary and arguments to launch with. `TOKEMAN_CLAUDE_BIN`
    /// overrides the configured binary, which overrides `claude` on PATH.
    pub fn command(&self, extra_args: impl IntoIterator<Item = String>) -> (String, Vec<String>) {
        let binary = std::env::var("TOKEMAN_CLAUDE_BIN")
            .ok()
            .or_else(|| self.claude_bin.clone())
            .unwrap_or_else(|| "claude".into());
        let mut args = self.launch_args.clone();
        args.extend(extra_args);
        const SKIP: &str = "--dangerously-skip-permissions";
        if self.dangerous_mode && !args.iter().any(|arg| arg == SKIP) {
            args.push(SKIP.into());
        }
        (binary, args)
    }
}

impl Default for LaunchSettings {
    fn default() -> Self {
        Self {
            launch_args: Vec::new(),
            dangerous_mode: false,
            terminal: None,
            claude_bin: None,
            probe_interval_secs: default_probe_interval(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Config {
    #[serde(default)]
    pub tokens: Vec<Token>,
    #[serde(default)]
    pub settings: LaunchSettings,
    #[serde(default)]
    pub rotation: RotationSettings,
    /// Codex profiles. Unlike `tokens`, these hold no secrets.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub codex_accounts: Vec<CodexAccount>,
}

impl Config {
    pub fn path() -> Result<PathBuf> {
        let base = std::env::var("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|_| {
                dirs::home_dir()
                    .unwrap_or_else(|| PathBuf::from("."))
                    .join(".config")
            });
        Ok(base.join("tokeman").join("tokens.toml"))
    }

    pub fn load() -> Result<Self> {
        let path = Self::path()?;
        if !path.exists() {
            return Ok(Self::default());
        }
        let contents = std::fs::read_to_string(&path)
            .with_context(|| format!("failed to read {}", path.display()))?;
        let config: Config = toml::from_str(&contents)
            .with_context(|| format!("failed to parse {}", path.display()))?;
        config.rotation.validate()?;
        Ok(config)
    }

    /// Lock the config and load the current file, for a mutation that
    /// cannot be expressed as a synchronous closure (e.g. one that awaits a
    /// network call). Save with [`Config::save`] before dropping the lock.
    pub fn load_locked() -> Result<(Self, ConfigLock)> {
        let lock = ConfigLock::acquire()?;
        Ok((Self::load()?, lock))
    }

    /// Apply `mutate` to the current on-disk config and save it, atomically
    /// with respect to every other tokeman writer. Returns the saved config.
    pub fn update<T>(mutate: impl FnOnce(&mut Self) -> Result<T>) -> Result<(Self, T)> {
        let (mut config, lock) = Self::load_locked()?;
        let result = mutate(&mut config)?;
        config.save(&lock)?;
        Ok((config, result))
    }

    /// Persist the config, preserving comments.
    ///
    /// This edits the existing document rather than re-serializing the struct.
    /// `toml::to_string_pretty` round-trips through serde, which has no concept
    /// of comments, so saving after any mutation silently deleted every
    /// annotation in `tokens.toml` — including hand-written provenance notes
    /// about where each credential came from and when it expires. That data is
    /// not reconstructible from anything tokeman stores, so the writer has to
    /// be non-destructive by construction.
    pub fn save(&self, _lock: &ConfigLock) -> Result<()> {
        self.rotation.validate()?;
        let path = Self::path()?;
        let contents = self.render(&std::fs::read_to_string(&path).unwrap_or_default())?;
        crate::private_fs::write_atomic(&path, contents.as_bytes(), 0o600)
    }

    /// Merge this config into `existing` TOML text, keeping its comments.
    ///
    /// Entries are matched by `name`, so an entry that already exists keeps the
    /// comment block attached to it even when its values change. Only entries
    /// tokeman actually removed disappear.
    fn render(&self, existing: &str) -> Result<String> {
        use toml_edit::{DocumentMut, Item, Table, value};

        let mut doc: DocumentMut = existing.parse().unwrap_or_default();

        sync_entries(
            &mut doc,
            "tokens",
            &self.tokens,
            |token| &token.name,
            |token, table| {
                set_item(table, "name", value(&token.name));
                set_or_remove(table, "key", token.setup_key().map(value));
                // Renamed to `access_token`; drop the old spelling on first save.
                table.remove("usage_key");
                set_or_remove(
                    table,
                    "access_token",
                    token.access_token.as_deref().map(value),
                );
                set_or_remove(
                    table,
                    "refresh_token",
                    token.refresh_token.as_deref().map(value),
                );
                set_or_remove(table, "expires_at", token.expires_at.map(value));
                set_or_remove(table, "obtained_at", token.obtained_at.map(value));
                set_or_remove(
                    table,
                    "scopes",
                    token.scopes.as_ref().map(|scopes| {
                        value(toml_edit::Array::from_iter(
                            scopes.iter().map(String::as_str),
                        ))
                    }),
                );
                set_or_remove(
                    table,
                    "login_error",
                    token.login_error.as_deref().map(value),
                );
            },
        )?;

        sync_entries(
            &mut doc,
            "codex_accounts",
            &self.codex_accounts,
            |account| &account.name,
            |account, table| {
                set_item(table, "name", value(&account.name));
                set_item(table, "codex_home", value(&account.codex_home));
                set_or_remove(table, "label", account.label.as_deref().map(value));
                set_or_remove(table, "parked", account.parked.then(|| value(true)));
                set_or_remove(
                    table,
                    "access_only",
                    account.access_only.then(|| value(true)),
                );
            },
        )?;

        // Scalar tables: update in place so any comments inside survive.
        let settings = doc
            .entry("settings")
            .or_insert(Item::Table(Table::new()))
            .as_table_mut()
            .context("[settings] is not a table")?;
        set_item(
            settings,
            "launch_args",
            value(toml_edit::Array::from_iter(
                self.settings.launch_args.iter().map(String::as_str),
            )),
        );
        set_item(
            settings,
            "dangerous_mode",
            value(self.settings.dangerous_mode),
        );
        set_item(
            settings,
            "probe_interval_secs",
            value(self.settings.probe_interval_secs as i64),
        );
        for (key, maybe) in [
            ("terminal", self.settings.terminal.as_ref()),
            ("claude_bin", self.settings.claude_bin.as_ref()),
        ] {
            match maybe {
                Some(present) => {
                    set_item(settings, key, value(present));
                }
                None => {
                    settings.remove(key);
                }
            }
        }

        let rotation = doc
            .entry("rotation")
            .or_insert(Item::Table(Table::new()))
            .as_table_mut()
            .context("[rotation] is not a table")?;
        for (key, number) in [
            (
                "normal_min_5h_remaining",
                self.rotation.normal_min_5h_remaining,
            ),
            (
                "normal_min_7d_remaining",
                self.rotation.normal_min_7d_remaining,
            ),
            ("sip_min_5h_remaining", self.rotation.sip_min_5h_remaining),
            ("sip_min_7d_remaining", self.rotation.sip_min_7d_remaining),
        ] {
            set_item(rotation, key, value(number));
        }
        set_item(
            rotation,
            "normal_probe_interval_secs",
            value(self.rotation.normal_probe_interval_secs as i64),
        );
        set_item(
            rotation,
            "sip_probe_interval_secs",
            value(self.rotation.sip_probe_interval_secs as i64),
        );

        Ok(doc.to_string())
    }

    pub fn add_token(&mut self, name: String, key: String) {
        match self.tokens.iter_mut().find(|token| token.name == name) {
            // Re-adding an account replaces its setup key but must not discard
            // a login grant: that refresh token is the only live copy.
            Some(token) => token.key = key,
            None => self.tokens.push(Token {
                name,
                key,
                ..Token::default()
            }),
        }
    }

    /// Record a browser login, creating the account if it is new.
    pub fn set_login_bundle(
        &mut self,
        name: &str,
        bundle: crate::claude_login::Bundle,
        now_ms: i64,
    ) {
        if !self.tokens.iter().any(|token| token.name == name) {
            self.tokens.push(Token {
                name: name.to_owned(),
                ..Token::default()
            });
        }
        let token = self
            .tokens
            .iter_mut()
            .find(|token| token.name == name)
            .expect("account was just ensured");
        token.apply_login(bundle, now_ms);
    }

    /// Attach a captured `/login` access token for usage reads. It has no
    /// refresh material, so it never becomes the installed credential.
    pub fn set_usage_key(&mut self, name: &str, key: String) -> bool {
        let Some(token) = self.tokens.iter_mut().find(|token| token.name == name) else {
            return false;
        };
        if token.is_refreshable() {
            // A live login grant already covers usage reads; replacing its
            // access token would desynchronize it from its refresh token.
            return true;
        }
        token.access_token = Some(key);
        token.expires_at = None;
        token.obtained_at = None;
        true
    }

    pub fn remove_token(&mut self, name: &str) -> bool {
        let before = self.tokens.len();
        self.tokens.retain(|t| t.name != name);
        self.tokens.len() < before
    }

    pub fn upsert_codex_account(&mut self, account: CodexAccount) {
        self.codex_accounts.retain(|a| a.name != account.name);
        self.codex_accounts.push(account);
    }

    pub fn remove_codex_account(&mut self, name: &str) -> bool {
        let before = self.codex_accounts.len();
        self.codex_accounts.retain(|a| a.name != name);
        self.codex_accounts.len() < before
    }

    pub fn codex_account(&self, name: &str) -> Option<&CodexAccount> {
        self.codex_accounts.iter().find(|a| a.name == name)
    }
}

/// Replace a value while keeping its key, and so the comments attached to it.
/// `Table::insert` would swap in a fresh key and silently drop them.
fn set_item(table: &mut toml_edit::Table, key: &str, item: impl Into<toml_edit::Item>) {
    let item = item.into();
    match table.get_mut(key) {
        Some(existing) => *existing = item,
        None => {
            table.insert(key, item);
        }
    }
}

fn set_or_remove(table: &mut toml_edit::Table, key: &str, item: Option<toml_edit::Item>) {
    match item {
        Some(item) => set_item(table, key, item),
        None => {
            table.remove(key);
        }
    }
}

/// Exclusive hold on `tokens.toml` for a read-modify-write.
///
/// Refresh tokens are single-use, so a save from a stale in-memory copy (a
/// tray left open for hours, a `tokeman add` racing the daemon's refresh)
/// would write back a token the provider has already consumed and lose the
/// account. Saving therefore requires this guard, and the guard is only handed
/// out alongside a fresh load.
pub struct ConfigLock {
    _lock: crate::private_fs::FileLock,
}

impl ConfigLock {
    fn acquire() -> Result<Self> {
        let path = Config::path()?.with_extension("lock");
        Ok(Self {
            _lock: crate::private_fs::FileLock::acquire(&path)?,
        })
    }
}

/// Rewrite an array-of-tables in place, reusing each existing table so the
/// comments attached to it survive.
///
/// Matching is by `name`: a renamed-away entry is dropped along with its
/// comment (it is gone from the config), but an entry whose *values* changed
/// keeps everything written about it.
fn sync_entries<T>(
    doc: &mut toml_edit::DocumentMut,
    key: &str,
    entries: &[T],
    name_of: impl Fn(&T) -> &str,
    fill: impl Fn(&T, &mut toml_edit::Table),
) -> Result<()> {
    use toml_edit::{ArrayOfTables, Item, Table};

    if entries.is_empty() {
        doc.remove(key);
        return Ok(());
    }

    let existing = match doc.remove(key) {
        Some(Item::ArrayOfTables(tables)) => tables,
        _ => ArrayOfTables::new(),
    };
    // Keep each table paired with its name so decor follows the right entry.
    let mut by_name: Vec<(Option<String>, Table)> = existing
        .into_iter()
        .map(|table| {
            let name = table
                .get("name")
                .and_then(|item| item.as_str())
                .map(str::to_string);
            (name, table)
        })
        .collect();

    let mut rebuilt = ArrayOfTables::new();
    for entry in entries {
        let wanted = name_of(entry);
        let mut table = match by_name
            .iter()
            .position(|(name, _)| name.as_deref() == Some(wanted))
        {
            Some(index) => by_name.remove(index).1,
            None => Table::new(),
        };
        fill(entry, &mut table);
        rebuilt.push(table);
    }

    doc.insert(key, Item::ArrayOfTables(rebuilt));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const HOUR_MS: i64 = 3_600_000;

    fn login_token(expires_at: i64) -> Token {
        Token {
            name: "acct".into(),
            key: "sk-ant-oat01-setup".into(),
            access_token: Some("sk-ant-oat01-access".into()),
            refresh_token: Some("sk-ant-ort01-refresh".into()),
            expires_at: Some(expires_at),
            obtained_at: Some(expires_at - 8 * HOUR_MS),
            scopes: Some(vec!["user:inference".into(), "user:profile".into()]),
            login_error: None,
        }
    }

    #[test]
    fn a_live_login_is_preferred_over_the_setup_token() {
        let token = login_token(10 * HOUR_MS);
        let credential = token.credential(0).expect("credential");
        assert_eq!(credential.kind, CredentialKind::Login);
        assert_eq!(credential.value, "sk-ant-oat01-access");
        assert!(
            credential
                .scopes
                .iter()
                .any(|scope| scope == "user:profile")
        );
    }

    #[test]
    fn a_refused_or_expiring_login_falls_back_to_the_setup_token() {
        let mut refused = login_token(10 * HOUR_MS);
        refused.login_error = Some("invalid_grant".into());
        assert_eq!(refused.credential(0).unwrap().kind, CredentialKind::Setup);

        // One minute left is too little to hand to a session.
        let expiring = login_token(60_000);
        assert_eq!(expiring.credential(0).unwrap().kind, CredentialKind::Setup);

        let mut login_only = login_token(60_000);
        login_only.key = String::new();
        assert!(login_only.credential(0).is_none());
    }

    #[test]
    fn a_captured_access_token_without_refresh_is_never_installed() {
        let token = Token {
            name: "acct".into(),
            key: "sk-ant-oat01-setup".into(),
            access_token: Some("captured".into()),
            ..Token::default()
        };
        assert_eq!(token.credential(0).unwrap().kind, CredentialKind::Setup);
        assert_eq!(token.usage_credential(0), Some("captured"));
        assert!(!token.needs_refresh(0));
    }

    #[test]
    fn refresh_is_due_inside_the_margin_but_not_for_a_fresh_token() {
        let expires_at = 10 * HOUR_MS;
        let token = login_token(expires_at);
        assert!(!token.needs_refresh(expires_at - REFRESH_MARGIN_MS - 1));
        assert!(token.needs_refresh(expires_at - REFRESH_MARGIN_MS + 1));
        assert!(token.needs_refresh(expires_at + 1));

        // A token minted a minute ago is not refreshed even if its lifetime is
        // shorter than the margin; an expired one always is.
        let mut young = login_token(expires_at);
        young.obtained_at = Some(expires_at - 20 * 60_000);
        assert!(!young.needs_refresh(expires_at - 19 * 60_000));
        assert!(young.needs_refresh(expires_at));
    }

    #[test]
    fn a_refresh_that_omits_the_refresh_token_keeps_the_old_one() {
        let mut token = login_token(HOUR_MS);
        token.login_error = Some("stale".into());
        token.apply_login(
            crate::claude_login::Bundle {
                access_token: "new-access".into(),
                refresh_token: None,
                expires_at: Some(9 * HOUR_MS),
                scopes: Vec::new(),
            },
            HOUR_MS,
        );
        assert_eq!(token.refresh_token.as_deref(), Some("sk-ant-ort01-refresh"));
        assert_eq!(token.access_token.as_deref(), Some("new-access"));
        assert!(
            token.scopes.is_some(),
            "empty scope list must not erase known scopes"
        );
        assert!(token.login_error.is_none());
    }

    #[test]
    fn rendering_keeps_comments_and_migrates_usage_key() {
        let existing = r#"# fleet notes
[[tokens]]
# minted 2026-07-01
name = "acct"
key = "sk-ant-oat01-setup"
usage_key = "old-captured"
"#;
        let mut config: Config = toml::from_str(existing).unwrap();
        assert_eq!(
            config.tokens[0].access_token.as_deref(),
            Some("old-captured")
        );
        config.tokens[0].refresh_token = Some("r".into());
        let rendered = config.render(existing).unwrap();
        assert!(rendered.contains("# minted 2026-07-01"), "{rendered}");
        assert!(rendered.contains("access_token = \"old-captured\""));
        assert!(!rendered.contains("usage_key"));
        assert!(rendered.contains("refresh_token = \"r\""));
        let reparsed: Config = toml::from_str(&rendered).unwrap();
        assert_eq!(reparsed.tokens[0].refresh_token.as_deref(), Some("r"));
    }

    #[test]
    fn re_adding_an_account_keeps_its_login_grant() {
        let mut config = Config::default();
        config.tokens.push(login_token(HOUR_MS));
        config.add_token("acct".into(), "sk-ant-oat01-new-setup".into());
        assert_eq!(config.tokens.len(), 1);
        assert_eq!(config.tokens[0].key, "sk-ant-oat01-new-setup");
        assert!(config.tokens[0].is_refreshable());
    }
}
