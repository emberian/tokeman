//! The fleet: every account tokeman may hand to Claude, the Codex profiles it
//! keeps alive, and the two settings tables, all in `tokens.toml`.
//!
//! Every write is a locked read-modify-write through [`Fleet::update`]. A
//! login's refresh token is single-use, so a save from a stale copy would lose
//! the account for good. Writes edit the existing document rather than
//! re-serialize it, so comments survive.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

use crate::fs::{FileLock, read_if_present, write_atomic};
use crate::paths;
use crate::settings::{LaunchSettings, PolicySettings};

/// Refresh this long before expiry. Claude keeps its cached token until it is
/// refused, then adopts whatever settings.json holds, so the replacement only
/// has to be in place before the old one dies.
pub const REFRESH_MARGIN_MS: i64 = 30 * 60 * 1000;
/// Never refresh a token younger than this, whatever its expiry says.
const MIN_REFRESH_AGE_MS: i64 = 5 * 60 * 1000;
/// An installed access token must outlive this, or a session would adopt a
/// credential refused before its first request completes.
const MIN_INSTALL_LIFETIME_MS: i64 = 2 * 60 * 1000;

/// A browser login: full scope, refreshable, short-lived access token.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Login {
    pub access_token: Option<String>,
    /// Single-use: every refresh returns a replacement.
    pub refresh_token: Option<String>,
    /// Epoch milliseconds.
    pub expires_at: Option<i64>,
    /// Epoch milliseconds when the access token was minted.
    pub obtained_at: Option<i64>,
    pub scopes: Vec<String>,
    /// Why the refresh token was refused for good. While set, the account
    /// falls back to its setup key until `tokeman login` runs again.
    pub error: Option<String>,
}

impl Login {
    pub fn is_refreshable(&self) -> bool {
        self.refresh_token.is_some() && self.error.is_none()
    }
}

/// What the token endpoint returns for a login or a refresh.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Grant {
    pub access_token: String,
    pub refresh_token: Option<String>,
    pub expires_at: Option<i64>,
    pub scopes: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Account {
    pub name: String,
    /// A `claude setup-token`: long-lived, inference-only.
    pub setup_key: Option<String>,
    pub login: Option<Login>,
}

/// The credential tokeman hands Claude, or probes with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Credential<'a> {
    Setup(&'a str),
    Login {
        token: &'a str,
        /// Epoch milliseconds.
        expires_at: Option<i64>,
        scopes: &'a [String],
    },
}

impl<'a> Credential<'a> {
    pub fn value(self) -> &'a str {
        match self {
            Self::Setup(value) | Self::Login { token: value, .. } => value,
        }
    }

    pub fn is_login(self) -> bool {
        matches!(self, Self::Login { .. })
    }

    pub fn expires_at(self) -> Option<i64> {
        match self {
            Self::Setup(_) => None,
            Self::Login { expires_at, .. } => expires_at,
        }
    }

    pub fn scopes(self) -> &'a [String] {
        match self {
            Self::Setup(_) => &[],
            Self::Login { scopes, .. } => scopes,
        }
    }
}

impl Account {
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            ..Self::default()
        }
    }

    /// The login access token, if the grant is refreshable and the token has
    /// room left to be adopted by a session.
    fn login_credential(&self, now_ms: i64) -> Option<Credential<'_>> {
        let login = self.login.as_ref().filter(|login| login.is_refreshable())?;
        let token = login.access_token.as_deref()?;
        if login
            .expires_at
            .is_some_and(|expires_at| expires_at - now_ms < MIN_INSTALL_LIFETIME_MS)
        {
            return None;
        }
        Some(Credential::Login {
            token,
            expires_at: login.expires_at,
            scopes: &login.scopes,
        })
    }

    /// What rotation installs: the full-scope login when one is live,
    /// otherwise the setup key.
    pub fn credential(&self, now_ms: i64) -> Option<Credential<'_>> {
        self.login_credential(now_ms)
            .or_else(|| self.setup_key.as_deref().map(Credential::Setup))
    }

    /// A token for the read-only profile endpoint. A captured access token
    /// without refresh material still counts until it expires.
    pub fn profile_token(&self, now_ms: i64) -> Option<&str> {
        let login = self.login.as_ref()?;
        if login
            .expires_at
            .is_some_and(|expires_at| expires_at <= now_ms)
        {
            return None;
        }
        login.access_token.as_deref()
    }

    pub fn is_refreshable(&self) -> bool {
        self.login.as_ref().is_some_and(Login::is_refreshable)
    }

    pub fn needs_refresh(&self, now_ms: i64) -> bool {
        let Some(login) = self.login.as_ref().filter(|login| login.is_refreshable()) else {
            return false;
        };
        let Some(expires_at) = login.expires_at else {
            // No recorded expiry: refresh once so the sweep learns it.
            return login.access_token.is_none() || login.obtained_at.is_none();
        };
        let old_enough = login
            .obtained_at
            .is_none_or(|obtained_at| now_ms - obtained_at >= MIN_REFRESH_AGE_MS);
        expires_at <= now_ms || (expires_at - now_ms <= REFRESH_MARGIN_MS && old_enough)
    }

    /// Whether `value` is one of this account's current credentials.
    pub fn holds(&self, value: &str) -> bool {
        self.setup_key.as_deref() == Some(value)
            || self
                .login
                .as_ref()
                .and_then(|login| login.access_token.as_deref())
                == Some(value)
    }

    /// Store a grant. The provider may or may not rotate the refresh token;
    /// the old one is kept only when no new one came.
    pub fn apply_grant(&mut self, grant: Grant, now_ms: i64) {
        let login = self.login.get_or_insert_with(Login::default);
        login.access_token = Some(grant.access_token);
        if grant.refresh_token.is_some() {
            login.refresh_token = grant.refresh_token;
        }
        login.expires_at = grant.expires_at;
        login.obtained_at = Some(now_ms);
        if !grant.scopes.is_empty() {
            login.scopes = grant.scopes;
        }
        login.error = None;
    }

    /// Keep a captured access token for profile reads. A live login already
    /// covers them, and replacing its access token would desynchronize it from
    /// its refresh token, so it is left alone.
    pub fn capture_profile_token(&mut self, token: String) {
        if self.is_refreshable() {
            return;
        }
        self.login = Some(Login {
            access_token: Some(token),
            ..Login::default()
        });
    }
}

/// A Codex profile directory (a `CODEX_HOME`) tokeman probes, refreshes and
/// seats accounts into. Only the directory is stored: Codex owns `auth.json`
/// and rotates the single-use refresh token inside it.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct CodexAccount {
    pub name: String,
    /// Used as `CODEX_HOME`; may start with `~/`.
    pub codex_home: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    /// Excluded from automatic seating; still probed.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub parked: bool,
    /// Seeded from an access token with no refresh token: cannot be renewed,
    /// so its eventual 401 is not a revocation to refresh away.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub access_only: bool,
}

impl CodexAccount {
    pub fn home(&self) -> PathBuf {
        paths::expand_tilde(&self.codex_home)
    }
}

/// The whole of tokens.toml.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Fleet {
    pub accounts: Vec<Account>,
    pub codex: Vec<CodexAccount>,
    pub launch: LaunchSettings,
    pub policy: PolicySettings,
}

/// Held while a [`Fleet`] loaded with it may be saved.
pub struct FleetLock {
    _lock: FileLock,
}

impl Fleet {
    pub fn account(&self, name: &str) -> Option<&Account> {
        self.accounts.iter().find(|account| account.name == name)
    }

    pub fn account_mut(&mut self, name: &str) -> Option<&mut Account> {
        self.accounts
            .iter_mut()
            .find(|account| account.name == name)
    }

    /// The account holding this credential value.
    pub fn account_holding(&self, value: &str) -> Option<&Account> {
        self.accounts.iter().find(|account| account.holds(value))
    }

    /// Add an account, or replace the setup key of one that exists. A login
    /// grant on the existing account is kept: its refresh token is the only copy.
    pub fn add_setup_key(&mut self, name: &str, key: String) {
        match self.account_mut(name) {
            Some(account) => account.setup_key = Some(key),
            None => self.accounts.push(Account {
                name: name.to_owned(),
                setup_key: Some(key),
                login: None,
            }),
        }
    }

    pub fn ensure_account(&mut self, name: &str) -> &mut Account {
        if self.account(name).is_none() {
            self.accounts.push(Account::new(name));
        }
        self.account_mut(name).expect("account was just ensured")
    }

    pub fn remove_account(&mut self, name: &str) -> bool {
        let before = self.accounts.len();
        self.accounts.retain(|account| account.name != name);
        self.accounts.len() < before
    }

    pub fn codex_account(&self, name: &str) -> Option<&CodexAccount> {
        self.codex.iter().find(|account| account.name == name)
    }

    pub fn upsert_codex(&mut self, account: CodexAccount) {
        self.codex.retain(|existing| existing.name != account.name);
        self.codex.push(account);
    }

    pub fn remove_codex(&mut self, name: &str) -> bool {
        let before = self.codex.len();
        self.codex.retain(|account| account.name != name);
        self.codex.len() < before
    }

    pub fn load() -> Result<Self> {
        let path = paths::accounts_file();
        let Some(contents) = read_if_present(&path)? else {
            return Ok(Self::default());
        };
        Self::parse(&contents).with_context(|| format!("failed to parse {}", path.display()))
    }

    pub fn load_locked() -> Result<(Self, FleetLock)> {
        let lock = FleetLock {
            _lock: FileLock::acquire_announced(
                &paths::accounts_lock(),
                "the account file lock (another tokeman is writing tokens.toml)",
            )?,
        };
        Ok((Self::load()?, lock))
    }

    /// Change the fleet under the lock and save it.
    pub fn update<T>(mutate: impl FnOnce(&mut Self) -> Result<T>) -> Result<(Self, T)> {
        let (mut fleet, lock) = Self::load_locked()?;
        let result = mutate(&mut fleet)?;
        fleet.save(&lock)?;
        Ok((fleet, result))
    }

    pub fn save(&self, _lock: &FleetLock) -> Result<()> {
        self.policy.validate()?;
        let path = paths::accounts_file();
        let existing = read_if_present(&path)?.unwrap_or_default();
        write_atomic(&path, self.render(&existing)?.as_bytes(), 0o600)
    }

    pub fn parse(contents: &str) -> Result<Self> {
        let file: File = toml::from_str(contents)?;
        file.policy.validate()?;
        Ok(Self {
            accounts: file.tokens.into_iter().map(Account::from).collect(),
            codex: file.codex_accounts,
            launch: file.settings,
            policy: file.policy,
        })
    }

    /// Render over the existing document so comments and unknown keys stay.
    pub fn render(&self, existing: &str) -> Result<String> {
        use toml_edit::{DocumentMut, Item, Table, value};
        let mut doc: DocumentMut = existing.parse().unwrap_or_default();
        sync_tables(
            &mut doc,
            "tokens",
            &self.accounts,
            |account| &account.name,
            |account, table| {
                let record = Record::from(account.clone());
                set(table, "name", value(&record.name));
                set_opt(table, "key", record.key.as_deref().map(value));
                table.remove("usage_key");
                set_opt(
                    table,
                    "access_token",
                    record.access_token.as_deref().map(value),
                );
                set_opt(
                    table,
                    "refresh_token",
                    record.refresh_token.as_deref().map(value),
                );
                set_opt(table, "expires_at", record.expires_at.map(value));
                set_opt(table, "obtained_at", record.obtained_at.map(value));
                set_opt(
                    table,
                    "scopes",
                    record.scopes.as_ref().map(|scopes| {
                        value(toml_edit::Array::from_iter(
                            scopes.iter().map(String::as_str),
                        ))
                    }),
                );
                set_opt(
                    table,
                    "login_error",
                    record.login_error.as_deref().map(value),
                );
            },
        );
        sync_tables(
            &mut doc,
            "codex_accounts",
            &self.codex,
            |account| &account.name,
            |account, table| {
                set(table, "name", value(&account.name));
                set(table, "codex_home", value(&account.codex_home));
                set_opt(table, "label", account.label.as_deref().map(value));
                set_opt(table, "parked", account.parked.then(|| value(true)));
                set_opt(
                    table,
                    "access_only",
                    account.access_only.then(|| value(true)),
                );
            },
        );
        let settings = doc
            .entry("settings")
            .or_insert(Item::Table(Table::new()))
            .as_table_mut()
            .context("[settings] is not a table")?;
        set(
            settings,
            "launch_args",
            value(toml_edit::Array::from_iter(
                self.launch.launch_args.iter().map(String::as_str),
            )),
        );
        set(
            settings,
            "dangerous_mode",
            value(self.launch.dangerous_mode),
        );
        set(
            settings,
            "probe_interval_secs",
            value(self.launch.probe_interval_secs as i64),
        );
        set_opt(
            settings,
            "terminal",
            self.launch.terminal.as_deref().map(value),
        );
        set_opt(
            settings,
            "claude_bin",
            self.launch.claude_bin.as_deref().map(value),
        );
        let rotation = doc
            .entry("rotation")
            .or_insert(Item::Table(Table::new()))
            .as_table_mut()
            .context("[rotation] is not a table")?;
        let policy = &self.policy;
        for (key, number) in [
            ("normal_min_5h_remaining", policy.normal_min_5h_remaining),
            ("normal_min_7d_remaining", policy.normal_min_7d_remaining),
            ("sip_min_5h_remaining", policy.sip_min_5h_remaining),
            ("sip_min_7d_remaining", policy.sip_min_7d_remaining),
            ("land_min_5h_remaining", policy.land_min_5h_remaining),
            ("land_min_7d_remaining", policy.land_min_7d_remaining),
        ] {
            set(rotation, key, value(number));
        }
        set(
            rotation,
            "normal_probe_interval_secs",
            value(policy.normal_probe_interval_secs as i64),
        );
        set(
            rotation,
            "sip_probe_interval_secs",
            value(policy.sip_probe_interval_secs as i64),
        );
        set(
            rotation,
            "target_model_sample_secs",
            value(policy.target_model_sample_secs as i64),
        );
        Ok(doc.to_string())
    }
}

/// tokens.toml as it is on disk.
#[derive(Debug, Default, Serialize, Deserialize)]
struct File {
    #[serde(default)]
    tokens: Vec<Record>,
    #[serde(default)]
    settings: LaunchSettings,
    #[serde(default, rename = "rotation")]
    policy: PolicySettings,
    #[serde(default)]
    codex_accounts: Vec<CodexAccount>,
}

/// One `[[tokens]]` table: an account's fields, flat.
#[derive(Debug, Default, Serialize, Deserialize)]
struct Record {
    name: String,
    #[serde(default)]
    key: Option<String>,
    #[serde(default, alias = "usage_key")]
    access_token: Option<String>,
    #[serde(default)]
    refresh_token: Option<String>,
    #[serde(default)]
    expires_at: Option<i64>,
    #[serde(default)]
    obtained_at: Option<i64>,
    #[serde(default)]
    scopes: Option<Vec<String>>,
    #[serde(default)]
    login_error: Option<String>,
}

impl From<Record> for Account {
    fn from(record: Record) -> Self {
        let has_login = record.access_token.is_some()
            || record.refresh_token.is_some()
            || record.login_error.is_some();
        Self {
            name: record.name,
            setup_key: record.key.filter(|key| !key.is_empty()),
            login: has_login.then(|| Login {
                access_token: record.access_token,
                refresh_token: record.refresh_token,
                expires_at: record.expires_at,
                obtained_at: record.obtained_at,
                scopes: record.scopes.unwrap_or_default(),
                error: record.login_error,
            }),
        }
    }
}

impl From<Account> for Record {
    fn from(account: Account) -> Self {
        let login = account.login.unwrap_or_default();
        Self {
            name: account.name,
            key: account.setup_key,
            access_token: login.access_token,
            refresh_token: login.refresh_token,
            expires_at: login.expires_at,
            obtained_at: login.obtained_at,
            scopes: (!login.scopes.is_empty()).then_some(login.scopes),
            login_error: login.error,
        }
    }
}

fn set(table: &mut toml_edit::Table, key: &str, item: impl Into<toml_edit::Item>) {
    let item = item.into();
    match table.get_mut(key) {
        Some(existing) => *existing = item,
        None => {
            table.insert(key, item);
        }
    }
}

fn set_opt(table: &mut toml_edit::Table, key: &str, item: Option<toml_edit::Item>) {
    match item {
        Some(item) => set(table, key, item),
        None => {
            table.remove(key);
        }
    }
}

/// Rewrite an array of tables from `entries`, keeping each existing table
/// (and its comments) paired with the entry of the same name.
fn sync_tables<T>(
    doc: &mut toml_edit::DocumentMut,
    key: &str,
    entries: &[T],
    name_of: impl Fn(&T) -> &str,
    fill: impl Fn(&T, &mut toml_edit::Table),
) {
    use toml_edit::{ArrayOfTables, Item, Table};
    if entries.is_empty() {
        doc.remove(key);
        return;
    }
    let existing = match doc.remove(key) {
        Some(Item::ArrayOfTables(tables)) => tables,
        _ => ArrayOfTables::new(),
    };
    let mut by_name: Vec<(Option<String>, Table)> = existing
        .into_iter()
        .map(|table| {
            let name = table
                .get("name")
                .and_then(|item| item.as_str())
                .map(str::to_owned);
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
}

#[cfg(test)]
mod tests {
    use super::*;

    const HOUR_MS: i64 = 3_600_000;

    fn login_account(expires_at: i64) -> Account {
        Account {
            name: "acct".into(),
            setup_key: Some("sk-ant-oat01-setup".into()),
            login: Some(Login {
                access_token: Some("sk-ant-oat01-access".into()),
                refresh_token: Some("sk-ant-ort01-refresh".into()),
                expires_at: Some(expires_at),
                obtained_at: Some(expires_at - 8 * HOUR_MS),
                scopes: vec!["user:inference".into(), "user:profile".into()],
                error: None,
            }),
        }
    }

    #[test]
    fn a_live_login_is_preferred_over_the_setup_key() {
        let account = login_account(10 * HOUR_MS);
        let credential = account.credential(0).unwrap();
        assert!(credential.is_login());
        assert_eq!(credential.value(), "sk-ant-oat01-access");
        assert!(
            credential
                .scopes()
                .iter()
                .any(|scope| scope == "user:profile")
        );
    }

    #[test]
    fn a_refused_or_expiring_login_falls_back_to_the_setup_key() {
        let mut refused = login_account(10 * HOUR_MS);
        refused.login.as_mut().unwrap().error = Some("invalid_grant".into());
        assert_eq!(
            refused.credential(0),
            Some(Credential::Setup("sk-ant-oat01-setup"))
        );
        // One minute left is too little to hand to a session.
        let expiring = login_account(60_000);
        assert!(!expiring.credential(0).unwrap().is_login());
        let mut login_only = login_account(60_000);
        login_only.setup_key = None;
        assert!(login_only.credential(0).is_none());
    }

    #[test]
    fn a_captured_access_token_without_refresh_is_never_installed() {
        let mut account = Account::new("acct");
        account.setup_key = Some("sk-ant-oat01-setup".into());
        account.capture_profile_token("captured".into());
        assert!(!account.credential(0).unwrap().is_login());
        assert_eq!(account.profile_token(0), Some("captured"));
        assert!(!account.needs_refresh(0));
        // A live login is left alone by a capture.
        let mut live = login_account(10 * HOUR_MS);
        live.capture_profile_token("captured".into());
        assert_eq!(live.profile_token(0), Some("sk-ant-oat01-access"));
    }

    #[test]
    fn refresh_is_due_inside_the_margin_but_not_for_a_fresh_token() {
        let expires_at = 10 * HOUR_MS;
        let account = login_account(expires_at);
        assert!(!account.needs_refresh(expires_at - REFRESH_MARGIN_MS - 1));
        assert!(account.needs_refresh(expires_at - REFRESH_MARGIN_MS + 1));
        assert!(account.needs_refresh(expires_at + 1));
        let mut young = login_account(expires_at);
        young.login.as_mut().unwrap().obtained_at = Some(expires_at - 20 * 60_000);
        assert!(!young.needs_refresh(expires_at - 19 * 60_000));
        assert!(young.needs_refresh(expires_at));
    }

    #[test]
    fn a_grant_without_a_refresh_token_keeps_the_old_one() {
        let mut account = login_account(HOUR_MS);
        account.login.as_mut().unwrap().error = Some("stale".into());
        account.apply_grant(
            Grant {
                access_token: "new-access".into(),
                refresh_token: None,
                expires_at: Some(9 * HOUR_MS),
                scopes: Vec::new(),
            },
            HOUR_MS,
        );
        let login = account.login.as_ref().unwrap();
        assert_eq!(login.refresh_token.as_deref(), Some("sk-ant-ort01-refresh"));
        assert_eq!(login.access_token.as_deref(), Some("new-access"));
        assert!(
            !login.scopes.is_empty(),
            "an empty scope list must not erase known scopes"
        );
        assert!(login.error.is_none());
    }

    #[test]
    fn rendering_keeps_comments_and_migrates_usage_key() {
        let existing = "# fleet notes\n[[tokens]]\n# minted 2026-07-01\nname = \"acct\"\nkey = \"sk-ant-oat01-setup\"\nusage_key = \"old-captured\"\n";
        let mut fleet = Fleet::parse(existing).unwrap();
        let login = fleet.accounts[0].login.as_mut().unwrap();
        assert_eq!(login.access_token.as_deref(), Some("old-captured"));
        login.refresh_token = Some("r".into());
        let rendered = fleet.render(existing).unwrap();
        assert!(rendered.contains("# fleet notes"), "{rendered}");
        assert!(rendered.contains("# minted 2026-07-01"), "{rendered}");
        assert!(rendered.contains("access_token = \"old-captured\""));
        assert!(!rendered.contains("usage_key"));
        assert!(rendered.contains("refresh_token = \"r\""));
        let reparsed = Fleet::parse(&rendered).unwrap();
        assert_eq!(
            reparsed.accounts[0]
                .login
                .as_ref()
                .unwrap()
                .refresh_token
                .as_deref(),
            Some("r")
        );
        assert_eq!(reparsed.policy, PolicySettings::default());
    }

    #[test]
    fn an_empty_key_is_no_setup_key_and_an_unknown_rotation_key_is_ignored() {
        let fleet = Fleet::parse(
            "[[tokens]]\nname = \"a\"\nkey = \"\"\n[rotation]\nper_session_reserve = 0.02\n",
        )
        .unwrap();
        assert_eq!(fleet.accounts[0].setup_key, None);
        assert_eq!(fleet.accounts[0].login, None);
    }

    #[test]
    fn re_adding_an_account_keeps_its_login_grant() {
        let mut fleet = Fleet::default();
        fleet.accounts.push(login_account(HOUR_MS));
        fleet.add_setup_key("acct", "sk-ant-oat01-new-setup".into());
        assert_eq!(fleet.accounts.len(), 1);
        assert_eq!(
            fleet.accounts[0].setup_key.as_deref(),
            Some("sk-ant-oat01-new-setup")
        );
        assert!(fleet.accounts[0].is_refreshable());
    }

    #[test]
    fn render_round_trips_every_table() {
        let mut fleet = Fleet::default();
        fleet.accounts.push(login_account(HOUR_MS));
        fleet.upsert_codex(CodexAccount {
            name: "work".into(),
            codex_home: "~/.codex-homes/work".into(),
            label: None,
            parked: true,
            access_only: false,
        });
        fleet.launch.terminal = Some("iTerm2".into());
        let rendered = fleet.render("").unwrap();
        let reparsed = Fleet::parse(&rendered).unwrap();
        assert_eq!(reparsed, fleet);
    }
}
