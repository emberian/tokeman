//! The operations `tokeman codex` exposes, as functions returning values.
//! Printing belongs to the CLI; the daemon's sweep returns events to log.

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Utc};
use std::collections::BTreeSet;
use std::fmt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use super::Client;
use super::appserver::{self, AccountInfo, AppServer, remaining_from_rate_limits};
use super::auth::{self, CodexHome, IdClaims};
use super::probe::{self, Probe, State};
use super::refresh::{self, DEFAULT_MAX_AGE_DAYS, Failure, RefreshError};
use crate::account::{CodexAccount, Fleet};
use crate::paths::{expand_tilde, tilde};
use crate::quota::Window;

/// Register a profile directory. It must already hold an auth.json.
pub fn add(name: &str, codex_home: &str, label: Option<String>) -> Result<CodexAccount> {
    let home = CodexHome::new(expand_tilde(codex_home));
    if !home.exists() {
        bail!(
            "{} has no auth.json; run `CODEX_HOME={} codex login` first",
            home.path().display(),
            home.path().display()
        );
    }
    let account = CodexAccount {
        name: name.to_owned(),
        codex_home: codex_home.to_owned(),
        label,
        ..CodexAccount::default()
    };
    Fleet::update(|fleet| {
        fleet.upsert_codex(account.clone());
        Ok(())
    })?;
    Ok(account)
}

pub fn remove(name: &str) -> Result<()> {
    Fleet::update(|fleet| {
        if !fleet.remove_codex(name) {
            bail!("no codex account named {name}");
        }
        Ok(())
    })?;
    Ok(())
}

/// A profile directory found on disk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Discovered {
    pub name: String,
    pub path: PathBuf,
    pub registered: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Discovery {
    pub profiles: Vec<Discovered>,
    /// How many `discover --adopt` registered.
    pub adopted: usize,
}

/// `~/.codex` and every directory under `~/.codex-homes` and `~/.codex-lanes`
/// that holds an auth.json.
pub fn discovery_roots() -> Vec<PathBuf> {
    let Some(home) = dirs::home_dir() else {
        return Vec::new();
    };
    let mut roots = vec![home.join(".codex")];
    for parent in [home.join(".codex-homes"), home.join(".codex-lanes")] {
        let Ok(entries) = std::fs::read_dir(&parent) else {
            continue;
        };
        roots.extend(
            entries
                .flatten()
                .map(|entry| entry.path())
                .filter(|path| path.is_dir()),
        );
    }
    roots.retain(|root| root.join("auth.json").exists());
    roots.sort();
    roots
}

/// The account name a profile directory suggests; `~/.codex` is `default`.
pub fn profile_name(path: &Path) -> String {
    match path.file_name().and_then(|name| name.to_str()) {
        Some(".codex") => "default".to_owned(),
        Some(name) => name.to_owned(),
        None => path.display().to_string(),
    }
}

/// Which of `roots` the fleet already knows.
pub fn survey(roots: &[PathBuf], fleet: &Fleet) -> Vec<Discovered> {
    roots
        .iter()
        .map(|root| Discovered {
            name: profile_name(root),
            path: root.clone(),
            registered: fleet.codex.iter().any(|account| account.home() == *root),
        })
        .collect()
}

/// Find profiles on disk; with `adopt`, register the unknown ones.
pub fn discover(adopt: bool) -> Result<Discovery> {
    let roots = discovery_roots();
    let profiles = survey(&roots, &Fleet::load()?);
    if !adopt || profiles.iter().all(|profile| profile.registered) {
        return Ok(Discovery {
            profiles,
            adopted: 0,
        });
    }
    let (_, adopted) = Fleet::update(|fleet| {
        let unknown: Vec<_> = survey(&roots, fleet)
            .into_iter()
            .filter(|profile| !profile.registered)
            .collect();
        for profile in &unknown {
            fleet.upsert_codex(CodexAccount {
                name: profile.name.clone(),
                codex_home: tilde(&profile.path),
                ..CodexAccount::default()
            });
        }
        Ok(unknown.len())
    })?;
    Ok(Discovery {
        profiles: survey(&roots, &Fleet::load()?),
        adopted,
    })
}

/// One access token seeded into a profile by `import`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Imported {
    pub name: String,
    pub home: PathBuf,
    pub plan: Option<String>,
    /// Epoch seconds when the access token expires.
    pub expires_at: Option<i64>,
    pub registered: bool,
}

/// `<name> <access-token-jwt>` lines; blank lines and `#` comments skipped.
pub fn parse_import(text: &str) -> Result<Vec<(String, String)>> {
    let mut entries = Vec::new();
    for (index, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((name, token)) = line.split_once(char::is_whitespace) else {
            bail!("line {}: expected `<name> <access-token>`", index + 1);
        };
        entries.push((name.trim().to_owned(), token.trim().to_owned()));
    }
    Ok(entries)
}

/// Seed one access-only profile per line of `text` under `root`, registering
/// each as an `access_only` account when `register` is set.
pub fn import_into(text: &str, root: &Path, register: bool) -> Result<Vec<Imported>> {
    let mut imported = Vec::new();
    let mut accounts = Vec::new();
    for (name, token) in parse_import(text)? {
        let claims = IdClaims::decode(&token)
            .with_context(|| format!("{name}: token is not a decodable JWT"))?;
        let Some(account_id) = claims.chatgpt_account_id.clone() else {
            bail!("{name}: token carries no chatgpt_account_id");
        };
        let dir = root.join(auth::slugify(&name));
        auth::seat_access_only(&token, &account_id, &CodexHome::new(&dir))?;
        let plan = claims.chatgpt_plan_type.clone();
        let expiry = match claims.expires_at {
            Some(exp) => format!("access token expires {}", fmt_epoch(exp)),
            None => "access-only".to_owned(),
        };
        accounts.push(CodexAccount {
            name: name.clone(),
            codex_home: tilde(&dir),
            label: Some(format!(
                "access-only import, {}, {expiry}",
                plan.as_deref().unwrap_or("?")
            )),
            access_only: true,
            ..CodexAccount::default()
        });
        imported.push(Imported {
            name,
            home: dir,
            plan,
            expires_at: claims.expires_at,
            registered: register,
        });
    }
    if imported.is_empty() {
        bail!("no tokens found");
    }
    if register {
        Fleet::update(|fleet| {
            for account in accounts {
                fleet.upsert_codex(account);
            }
            Ok(())
        })?;
    }
    Ok(imported)
}

/// Import from a file; profiles go under `homes_root` (default `~/.codex-homes`).
pub fn import(from: &Path, homes_root: Option<&str>, register: bool) -> Result<Vec<Imported>> {
    let text = std::fs::read_to_string(from)
        .with_context(|| format!("failed to read {}", from.display()))?;
    let root = match homes_root {
        Some(root) => expand_tilde(root),
        None => dirs::home_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join(".codex-homes"),
    };
    import_into(&text, &root, register).with_context(|| format!("importing {}", from.display()))
}

fn fmt_epoch(epoch: i64) -> String {
    DateTime::<Utc>::from_timestamp(epoch, 0)
        .map(|at| at.format("%Y-%m-%d %H:%MZ").to_string())
        .unwrap_or_else(|| epoch.to_string())
}

/// Probe every configured account.
pub async fn list(client: &Client, fleet: &Fleet) -> Vec<Probe> {
    probe::probe_all(client, &fleet.codex).await
}

pub const ROW_HEADER: &str =
    "ACCOUNT      EMAIL                      PLAN    PRIMARY          SECONDARY        STATE";

/// `45% (2h10m)`, or `-` for no window.
pub fn window_cell(window: Option<Window>, now: i64) -> String {
    let Some(window) = window else {
        return "-".to_owned();
    };
    let used = format!("{:.0}%", window.used * 100.0);
    let left = window.resets_at - now;
    if window.resets_at > 0 && left > 0 {
        format!("{used} ({})", duration_cell(left))
    } else {
        used
    }
}

pub fn duration_cell(seconds: i64) -> String {
    let days = seconds / 86_400;
    let hours = (seconds % 86_400) / 3_600;
    let minutes = (seconds % 3_600) / 60;
    if days > 0 {
        format!("{days}d{hours}h")
    } else if hours > 0 {
        format!("{hours}h{minutes}m")
    } else {
        format!("{minutes}m")
    }
}

/// A probe as one table row under [`ROW_HEADER`], then one indented line
/// per per-model bucket.
pub fn rows(probe: &Probe, now: i64) -> Vec<String> {
    let mut lines = vec![format!(
        "{:<12} {:<26} {:<7} {:<16} {:<16} {}",
        probe.account,
        probe.email().unwrap_or("-"),
        probe.plan().map(|plan| plan.as_str()).unwrap_or("-"),
        window_cell(probe.primary(), now),
        window_cell(probe.secondary(), now),
        probe.state(),
    )];
    for extra in probe.extra() {
        lines.push(format!(
            "  └ {:<24} {:<16} {}",
            extra.name.as_deref().unwrap_or("?"),
            window_cell(extra.limit.as_ref().and_then(|limit| limit.primary), now),
            if extra.is_limited() { "limited" } else { "ok" },
        ));
    }
    lines
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refreshed {
    /// A new bundle is on disk.
    Refreshed,
    /// Young enough to leave alone.
    Fresh,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RefreshReport {
    pub account: String,
    pub outcome: Result<Refreshed, RefreshError>,
}

impl fmt::Display for RefreshReport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.outcome {
            Ok(Refreshed::Refreshed) => write!(f, "{:<12} refreshed", self.account),
            Ok(Refreshed::Fresh) => write!(f, "{:<12} still fresh", self.account),
            Err(error) => write!(f, "{:<12} FAILED: {error}", self.account),
        }
    }
}

/// Refresh one account, or all of them. One failure does not stop the rest.
pub async fn refresh_accounts(
    client: &Client,
    fleet: &Fleet,
    name: Option<&str>,
    force: bool,
    max_age_days: i64,
) -> Result<Vec<RefreshReport>> {
    let accounts: Vec<&CodexAccount> = match name {
        Some(name) => vec![
            fleet
                .codex_account(name)
                .with_context(|| format!("no codex account named {name}"))?,
        ],
        None => fleet.codex.iter().collect(),
    };
    if accounts.is_empty() {
        bail!("no codex accounts configured");
    }
    let mut reports = Vec::with_capacity(accounts.len());
    for account in accounts {
        let home = CodexHome::new(account.home());
        let outcome = refresh::refresh(client, &home, force, max_age_days, Utc::now())
            .await
            .map(|bundle| match bundle {
                Some(_) => Refreshed::Refreshed,
                None => Refreshed::Fresh,
            });
        reports.push(RefreshReport {
            account: account.name.clone(),
            outcome,
        });
    }
    Ok(reports)
}

/// The viable, unparked probe scoring highest; `score` returning `None`
/// rules it out. Ties go to the later account.
pub fn pick<'a>(
    probes: &'a [Probe],
    fleet: &Fleet,
    score: impl Fn(&Probe) -> Option<f64>,
) -> Option<&'a Probe> {
    probes
        .iter()
        .filter(|probe| probe.is_viable())
        .filter(|probe| {
            fleet
                .codex_account(&probe.account)
                .is_none_or(|account| !account.parked)
        })
        .filter_map(|probe| score(probe).map(|value| (probe, value)))
        .max_by(|a, b| a.1.total_cmp(&b.1))
        .map(|(probe, _)| probe)
}

/// The viable account with the most headroom; with `limit`, the most left in
/// that per-model bucket, which is metered apart from the account-wide one.
pub fn best_of<'a>(probes: &'a [Probe], fleet: &Fleet, limit: Option<&str>) -> Option<&'a Probe> {
    match limit {
        Some(limit) => pick(probes, fleet, |probe| {
            probe.remaining_for(limit).filter(|left| *left > 0.0)
        }),
        None => pick(probes, fleet, |probe| {
            Some(probe.remaining().unwrap_or(0.0))
        }),
    }
}

/// Probe the fleet and pick the best account; its `codex_home` is the answer.
pub async fn best(client: &Client, fleet: &Fleet, limit: Option<&str>) -> Result<Probe> {
    let probes = list(client, fleet).await;
    match best_of(&probes, fleet, limit) {
        Some(winner) => Ok(winner.clone()),
        None => match limit {
            Some(limit) => bail!("no viable codex account with headroom for {limit}"),
            None => bail!("no viable codex account (all limited or erroring)"),
        },
    }
}

/// Something that went wrong on the way but did not stop the operation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Warning {
    /// Refreshing before the move failed; the stored token was used as is.
    RefreshFailed {
        account: String,
        error: RefreshError,
    },
    /// `--force` moved onto an account that is not viable.
    Forced { account: String, state: State },
}

impl fmt::Display for Warning {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::RefreshFailed { account, error } => {
                write!(f, "could not refresh {account} first: {error}")
            }
            Self::Forced { account, state } => {
                write!(
                    f,
                    "{account} is {state}; migrating anyway because --force was given"
                )
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Seated {
    pub account: String,
    pub into: PathBuf,
    pub warnings: Vec<Warning>,
}

/// Copy an account's bundle into another profile, refreshing it first so the
/// target never starts on a token about to 401.
pub async fn seat(
    client: &Client,
    fleet: &Fleet,
    account_name: &str,
    into: &str,
) -> Result<Seated> {
    let account = fleet
        .codex_account(account_name)
        .with_context(|| format!("no codex account named {account_name}"))?;
    let source = CodexHome::new(account.home());
    let target = CodexHome::new(expand_tilde(into));
    if source == target {
        bail!("source and target are the same profile");
    }
    let mut warnings = Vec::new();
    if let Err(error) =
        refresh::refresh(client, &source, false, DEFAULT_MAX_AGE_DAYS, Utc::now()).await
    {
        warnings.push(Warning::RefreshFailed {
            account: account_name.to_owned(),
            error,
        });
    }
    let bundle = source
        .read()?
        .bundle()
        .with_context(|| format!("{} has no usable OAuth bundle", source.path().display()))?;
    auth::seat(&bundle, &target)?;
    Ok(Seated {
        account: account_name.to_owned(),
        into: target.path().to_path_buf(),
        warnings,
    })
}

/// `codex_home` if given, else `~/.codex`.
fn target_home(codex_home: Option<&str>) -> Result<CodexHome> {
    match codex_home {
        Some(path) => Ok(CodexHome::new(expand_tilde(path))),
        None => CodexHome::default_home(),
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reseat {
    pub target: PathBuf,
    pub socket: PathBuf,
    pub account: String,
    pub warnings: Vec<Warning>,
    /// `None` for a dry run.
    pub moved: Option<Moved>,
}

/// What the running Codex reported before and after the announcement. A
/// successful RPC only means the request was accepted; `after` is the proof.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Moved {
    pub before: Option<AccountInfo>,
    pub after: Option<AccountInfo>,
}

/// Move the Codex running against `codex_home` onto `account` (default: the
/// best viable one) by announcing the new credentials over its app-server.
pub async fn reseat(
    client: &Client,
    fleet: &Fleet,
    codex_home: Option<&str>,
    account: Option<&str>,
    dry_run: bool,
    force: bool,
) -> Result<Reseat> {
    reseat_home(
        client,
        fleet,
        &target_home(codex_home)?,
        account,
        dry_run,
        force,
    )
    .await
}

async fn reseat_home(
    client: &Client,
    fleet: &Fleet,
    target: &CodexHome,
    account: Option<&str>,
    dry_run: bool,
    force: bool,
) -> Result<Reseat> {
    let probes = list(client, fleet).await;
    let chosen = match account {
        Some(name) => probes
            .iter()
            .find(|probe| probe.account == name)
            .with_context(|| format!("no codex account named {name}"))?,
        None => best_of(&probes, fleet, None).context("no viable codex account to migrate to")?,
    };
    let mut warnings = Vec::new();
    if !chosen.is_viable() {
        if !force {
            bail!(
                "{} is not viable right now: {} (pass --force to migrate anyway)",
                chosen.account,
                chosen.state()
            );
        }
        warnings.push(Warning::Forced {
            account: chosen.account.clone(),
            state: chosen.state(),
        });
    }
    let socket = appserver::control_socket_path(target.path());
    let mut result = Reseat {
        target: target.path().to_path_buf(),
        socket: socket.clone(),
        account: chosen.account.clone(),
        warnings,
        moved: None,
    };
    if dry_run {
        return Ok(result);
    }

    let source = CodexHome::new(
        fleet
            .codex_account(&chosen.account)
            .context("chosen account vanished from the fleet")?
            .home(),
    );
    // The app-server takes the token as is and does not refresh it.
    if let Err(error) =
        refresh::refresh(client, &source, false, DEFAULT_MAX_AGE_DAYS, Utc::now()).await
    {
        result.warnings.push(Warning::RefreshFailed {
            account: chosen.account.clone(),
            error,
        });
    }
    let auth = source.read()?;
    let access_token = auth
        .access_token()
        .context("chosen account has no access_token")?;
    let account_id = auth
        .effective_account_id()
        .context("chosen account has no chatgpt_account_id")?;
    let plan = auth.claims().and_then(|claims| claims.chatgpt_plan_type);

    let mut server = AppServer::connect(&socket).await?;
    let before = server.read_account().await.ok();
    server
        .reseat(access_token, &account_id, plan.as_deref())
        .await?;
    let after = server.read_account().await.ok();
    server.close().await;
    result.moved = Some(Moved { before, after });
    Ok(result)
}

/// What `watch` reports as it goes.
#[derive(Debug, Clone, PartialEq)]
pub enum WatchEvent {
    Watching {
        target: PathBuf,
        min_remaining: f64,
    },
    Headroom {
        remaining: f64,
    },
    /// Below the threshold; a migration follows.
    Migrating,
    Migrated(Reseat),
}

/// Follow the Codex running against `codex_home` and move it once its
/// headroom drops below `min_remaining`. The swap lands between turns, so
/// the threshold is above zero: waiting for 100% costs the user a failed turn.
pub async fn watch(
    client: &Client,
    fleet: &Fleet,
    codex_home: Option<&str>,
    min_remaining: f64,
    once: bool,
    mut on_event: impl FnMut(WatchEvent),
) -> Result<()> {
    let target = target_home(codex_home)?;
    let socket = appserver::control_socket_path(target.path());
    if !socket.exists() {
        bail!(
            "no app-server control socket at {}; start Codex, or run `codex app-server --listen unix://`",
            tilde(&socket)
        );
    }
    let mut server = AppServer::connect(&socket).await?;
    on_event(WatchEvent::Watching {
        target: target.path().to_path_buf(),
        min_remaining,
    });
    // Evaluate the current limits before waiting for a push.
    let mut pending = server.rate_limits().await.ok();
    loop {
        if let Some(remaining) = pending.as_ref().and_then(remaining_from_rate_limits) {
            on_event(WatchEvent::Headroom { remaining });
            if remaining < min_remaining {
                on_event(WatchEvent::Migrating);
                server.close().await;
                let moved = reseat_home(client, fleet, &target, None, false, false).await?;
                on_event(WatchEvent::Migrated(moved));
                if once {
                    return Ok(());
                }
                server = AppServer::connect(&socket).await?;
                pending = server.rate_limits().await.ok();
                continue;
            }
        }
        if once {
            return Ok(());
        }
        // Codex pushes this whenever its view changes; nothing to poll.
        pending = Some(
            server
                .next_notification("account/rateLimits/updated")
                .await?,
        );
    }
}

/// Codex accounts need attention on the order of days; a half hour keeps
/// the request rate far below anything the edge treats as abusive.
pub const SWEEP_INTERVAL: Duration = Duration::from_secs(30 * 60);

/// What one maintenance sweep did, for the daemon's log.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MaintenanceEvent {
    Refreshed {
        account: String,
    },
    /// A standing login failure cleared.
    UsableAgain {
        account: String,
    },
    /// A permanent failure began; reported once until it clears.
    NeedsLogin {
        account: String,
        home: PathBuf,
        failure: Failure,
    },
    RefreshFailed {
        account: String,
        error: RefreshError,
    },
    /// No HTTP client could be built; the sweep did nothing.
    Skipped {
        message: String,
    },
}

impl fmt::Display for MaintenanceEvent {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Refreshed { account } => write!(f, "codex: refreshed {account}"),
            Self::UsableAgain { account } => write!(f, "codex: {account} is usable again"),
            Self::NeedsLogin {
                account,
                home,
                failure,
            } => write!(
                f,
                "codex: {account} needs an interactive `CODEX_HOME={} codex login` ({})",
                home.display(),
                failure.label()
            ),
            Self::RefreshFailed { account, error } => {
                write!(f, "codex: refresh failed for {account}: {error}")
            }
            Self::Skipped { message } => write!(f, "codex: sweep skipped: {message}"),
        }
    }
}

/// What the daemon carries between sweeps: the client, so the cookie jar
/// persists, and the accounts whose login failure has already been reported.
#[derive(Debug, Default)]
pub struct MaintenanceState {
    client: Option<Client>,
    needs_login: BTreeSet<String>,
}

impl MaintenanceState {
    /// The accounts waiting on an interactive `codex login`.
    pub fn needs_login(&self) -> &BTreeSet<String> {
        &self.needs_login
    }
}

/// The daemon's sweep over the fleet's Codex accounts; the events are for
/// its log.
pub async fn maintenance(fleet: &Fleet, state: &mut MaintenanceState) -> Vec<MaintenanceEvent> {
    if fleet.codex.is_empty() {
        return Vec::new();
    }
    let MaintenanceState {
        client,
        needs_login,
    } = state;
    let client = match client {
        Some(client) => client,
        None => match Client::new() {
            Ok(built) => client.insert(built),
            Err(error) => {
                return vec![MaintenanceEvent::Skipped {
                    message: format!("{error:#}"),
                }];
            }
        },
    };
    sweep(client, &fleet.codex, needs_login).await
}

/// Keep every profile refreshed. The backend is asked rather than the clock:
/// a session revoked elsewhere leaves `last_refresh` recent while the token
/// is already rejected, and that rejection forces a refresh. `needs_login`
/// carries the standing failures between sweeps so each is reported when it
/// starts and when it clears, not every half hour.
pub async fn sweep(
    client: &Client,
    accounts: &[CodexAccount],
    needs_login: &mut BTreeSet<String>,
) -> Vec<MaintenanceEvent> {
    let mut events = Vec::new();
    if accounts.is_empty() {
        return events;
    }
    let probes = probe::probe_all(client, accounts).await;
    let rejected: BTreeSet<&str> = probes
        .iter()
        .filter(|probe| probe.is_unauthorized())
        .map(|probe| probe.account.as_str())
        .collect();
    for account in accounts {
        // No refresh material: its eventual 401 is expected, not a fault.
        if account.access_only {
            continue;
        }
        let home = CodexHome::new(account.home());
        let force = rejected.contains(account.name.as_str());
        let outcome =
            refresh::refresh(client, &home, force, DEFAULT_MAX_AGE_DAYS, Utc::now()).await;
        let permanent = outcome
            .as_ref()
            .err()
            .map(|error| error.failure)
            .filter(|failure| failure.is_permanent());
        if permanent.is_none() && needs_login.remove(&account.name) {
            events.push(MaintenanceEvent::UsableAgain {
                account: account.name.clone(),
            });
        }
        match (outcome, permanent) {
            (Ok(Some(_)), _) => events.push(MaintenanceEvent::Refreshed {
                account: account.name.clone(),
            }),
            (Ok(None), _) => {}
            (Err(_), Some(failure)) => {
                if needs_login.insert(account.name.clone()) {
                    events.push(MaintenanceEvent::NeedsLogin {
                        account: account.name.clone(),
                        home: home.path().to_path_buf(),
                        failure,
                    });
                }
            }
            (Err(error), None) => events.push(MaintenanceEvent::RefreshFailed {
                account: account.name.clone(),
                error,
            }),
        }
    }
    events
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codex::auth::tests::{bundle, id_token, jwt};
    use crate::codex::probe::tests::{Backend, challenge, ok, profile, status, usage_body};
    use crate::testing::codex_account;
    use axum::http::StatusCode;
    use serde_json::json;

    /// Push an account's `last_refresh` back by `days`.
    fn age(account: &CodexAccount, days: i64) {
        let path = CodexHome::new(account.home()).auth_path();
        let mut raw: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        raw["last_refresh"] = json!(Utc::now() - chrono::Duration::days(days));
        std::fs::write(&path, raw.to_string()).unwrap();
    }

    fn fleet_of(accounts: Vec<CodexAccount>) -> Fleet {
        Fleet {
            codex: accounts,
            ..Fleet::default()
        }
    }

    #[test]
    fn survey_marks_registered_roots_and_names_the_default_profile() {
        let dir = tempfile::tempdir().unwrap();
        let default = dir.path().join(".codex");
        let lane = dir.path().join(".codex-homes").join("work");
        let fleet = fleet_of(vec![codex_account("work", &lane.display().to_string())]);
        let found = survey(&[default.clone(), lane.clone()], &fleet);
        assert_eq!(
            found,
            vec![
                Discovered {
                    name: "default".into(),
                    path: default,
                    registered: false
                },
                Discovered {
                    name: "work".into(),
                    path: lane,
                    registered: true
                },
            ]
        );
    }

    #[test]
    fn import_lines_are_parsed_strictly_and_seed_access_only_profiles() {
        assert!(parse_import("solo-token-without-name\n").is_err());
        let parsed = parse_import("# comment\n\n  ops@x.com   tok1 \nb\ttok2\n").unwrap();
        assert_eq!(
            parsed,
            vec![
                ("ops@x.com".to_owned(), "tok1".to_owned()),
                ("b".to_owned(), "tok2".to_owned())
            ]
        );
        let dir = tempfile::tempdir().unwrap();
        let text = format!("ops@x.com {}\n", id_token("ops@x.com", "acct-ops", "team"));
        let imported = import_into(&text, dir.path(), false).unwrap();
        assert_eq!(imported.len(), 1);
        assert_eq!(imported[0].home, dir.path().join("ops-x"));
        assert_eq!(imported[0].plan.as_deref(), Some("team"));
        assert_eq!(imported[0].expires_at, Some(1_900_000_000));
        assert!(!imported[0].registered);
        let auth = CodexHome::new(&imported[0].home).read().unwrap();
        assert_eq!(auth.account_id(), Some("acct-ops"));
        assert_eq!(auth.refresh_token(), None);
        // A token without the account claim, and an empty file, are errors.
        let no_account = format!("x {}\n", jwt(json!({"email": "x"})));
        assert!(import_into(&no_account, dir.path(), false).is_err());
        assert!(import_into("# nothing\n", dir.path(), false).is_err());
        assert!(import_into("x not-a-jwt\n", dir.path(), false).is_err());
    }

    #[test]
    fn cells_format_usage_and_time_left() {
        assert_eq!(window_cell(None, 0), "-");
        assert_eq!(window_cell(Some(Window::new(0.456, 0)), 0), "46%");
        assert_eq!(
            window_cell(Some(Window::new(0.1, 1_000 + 7_800)), 1_000),
            "10% (2h10m)"
        );
        assert_eq!(window_cell(Some(Window::new(0.1, 500)), 1_000), "10%");
        assert_eq!(duration_cell(90_000), "1d1h");
        assert_eq!(duration_cell(120), "2m");
    }

    #[tokio::test]
    async fn rows_follow_the_header_and_list_extra_buckets() {
        let dir = tempfile::tempdir().unwrap();
        let account = profile(dir.path(), "one", "r1");
        let backend = Backend::default().usage_answers(vec![ok(usage_body(30.0, 60.0))]);
        let client = backend.client().await;
        let probes = list(&client, &fleet_of(vec![account])).await;
        let now = probes[0].at;
        let lines = rows(&probes[0], now);
        assert_eq!(lines.len(), 2);
        assert!(
            lines[0].starts_with("one          a@b.c                      plus    30% (1h0m)"),
            "{}",
            lines[0]
        );
        assert!(lines[0].ends_with(" ok"));
        assert!(lines[1].contains("codex_spark"));
        assert!(lines[1].ends_with("limited"));
        assert_eq!(
            ROW_HEADER,
            format!(
                "{:<12} {:<26} {:<7} {:<16} {:<16} STATE",
                "ACCOUNT", "EMAIL", "PLAN", "PRIMARY", "SECONDARY"
            )
        );
    }

    fn probe_with(name: &str, remaining: f64, spark: Option<f64>) -> Probe {
        let at = DateTime::from_timestamp(0, 0).unwrap();
        let mut body = usage_body(100.0 * (1.0 - remaining), 0.0);
        body["email"] = json!(format!("{name}@x.y"));
        match spark {
            Some(left) => {
                body["additional_rate_limits"][0]["rate_limit"]["limit_reached"] = json!(false);
                body["additional_rate_limits"][0]["rate_limit"]["allowed"] = json!(true);
                body["additional_rate_limits"][0]["rate_limit"]["primary_window"]["used_percent"] =
                    json!(100.0 * (1.0 - left));
            }
            None => body["additional_rate_limits"] = json!([]),
        }
        Probe {
            account: name.to_owned(),
            codex_home: PathBuf::from(format!("/homes/{name}")),
            at: 0,
            usage: Some(probe::decode_usage(&body.to_string(), at).unwrap()),
            error: None,
            last_refresh: None,
            email_hint: None,
        }
    }

    #[test]
    fn best_skips_parked_and_unviable_and_ranks_buckets_separately() {
        let mut probes = vec![
            probe_with("roomy", 0.9, None),
            probe_with("parked", 0.95, Some(0.5)),
            probe_with("spark", 0.3, Some(0.4)),
            probe_with("dead", 1.0, Some(1.0)),
        ];
        probes[3].usage = None;
        probes[3].error = Some(probe::ProbeError::Http {
            status: 401,
            body: String::new(),
        });
        let fleet = fleet_of(vec![CodexAccount {
            parked: true,
            ..codex_account("parked", "/homes/parked")
        }]);
        assert_eq!(best_of(&probes, &fleet, None).unwrap().account, "roomy");
        assert_eq!(
            best_of(&probes, &fleet, Some("spark")).unwrap().account,
            "spark"
        );
        assert_eq!(best_of(&probes, &fleet, Some("nothing-like-it")), None);
        let mut limited = probe_with("limited", 0.99, None);
        limited
            .usage
            .as_mut()
            .unwrap()
            .limit
            .as_mut()
            .unwrap()
            .limit_reached = Some(true);
        assert_eq!(best_of(&[limited], &fleet, None), None);
        // Ties go to the later account.
        let tied = vec![probe_with("a", 0.5, None), probe_with("b", 0.5, None)];
        assert_eq!(best_of(&tied, &fleet, None).unwrap().account, "b");
    }

    #[tokio::test]
    async fn maintenance_refreshes_rejected_accounts_and_reports_login_needs_once() {
        let dir = tempfile::tempdir().unwrap();
        let rejected = profile(dir.path(), "rejected", "r-old");
        let fine = profile(dir.path(), "fine", "r-fine");
        let orphan = profile(dir.path(), "orphan", "");
        age(&orphan, 8);
        let access_only = CodexAccount {
            access_only: true,
            ..profile(dir.path(), "import", "")
        };
        let accounts = vec![rejected.clone(), fine, orphan.clone(), access_only];
        let new_id = id_token("rejected@x.y", "acct-rejected", "pro");
        let backend = Backend::default()
            .usage_answers(vec![
                status(StatusCode::UNAUTHORIZED, "{}"),
                ok(usage_body(0.0, 0.0)),
            ])
            .token_answers(vec![(
                StatusCode::OK,
                json!({"access_token": "access-new", "refresh_token": "r-new", "id_token": new_id})
                    .to_string(),
            )]);
        let client = backend.client().await;
        let mut needs_login = BTreeSet::new();

        let events = sweep(&client, &accounts, &mut needs_login).await;
        assert_eq!(
            events,
            vec![
                MaintenanceEvent::Refreshed {
                    account: "rejected".into()
                },
                MaintenanceEvent::NeedsLogin {
                    account: "orphan".into(),
                    home: orphan.home(),
                    failure: Failure::NoRefreshToken,
                },
            ]
        );
        assert_eq!(needs_login.iter().collect::<Vec<_>>(), vec!["orphan"]);
        let auth = CodexHome::new(rejected.home()).read().unwrap();
        assert_eq!(auth.access_token(), Some("access-new"));
        assert_eq!(auth.refresh_token(), Some("r-new"));
        assert_eq!(
            auth.account_id(),
            Some("acct-rejected"),
            "the account id survives a refresh"
        );
        let token_requests = backend.requests("/token");
        assert_eq!(token_requests.len(), 1);
        let sent: serde_json::Value = serde_json::from_str(&token_requests[0].body).unwrap();
        assert_eq!(
            sent,
            json!({"client_id": "test-client", "grant_type": "refresh_token", "refresh_token": "r-old"})
        );
        assert_eq!(
            backend.requests("/usage").len(),
            4,
            "access-only accounts are still probed"
        );

        // The standing failure is not repeated; once fixed, it clears once.
        let events = sweep(&client, &accounts, &mut needs_login).await;
        assert!(events.is_empty(), "{events:?}");
        CodexHome::new(orphan.home())
            .write_tokens(&bundle("a", "r"))
            .unwrap();
        let events = sweep(&client, &accounts, &mut needs_login).await;
        assert_eq!(
            events,
            vec![MaintenanceEvent::UsableAgain {
                account: "orphan".into()
            }]
        );
        assert!(needs_login.is_empty());
        assert!(sweep(&client, &[], &mut needs_login).await.is_empty());
    }

    #[tokio::test]
    async fn maintenance_reports_transient_failures_every_sweep_and_a_reused_token_once() {
        let dir = tempfile::tempdir().unwrap();
        let account = profile(dir.path(), "one", "r1");
        let backend = Backend::default()
            .usage_answers(vec![status(StatusCode::UNAUTHORIZED, "{}")])
            .token_answers(vec![
                (StatusCode::BAD_GATEWAY, "upstream".into()),
                (
                    StatusCode::BAD_REQUEST,
                    r#"{"error":"refresh_token_reused"}"#.into(),
                ),
            ]);
        let client = backend.client().await;
        let mut needs_login = BTreeSet::new();
        let accounts = vec![account];
        let events = sweep(&client, &accounts, &mut needs_login).await;
        assert!(matches!(
            &events[..],
            [MaintenanceEvent::RefreshFailed { account, error }]
                if account == "one" && error.failure == Failure::Transient
        ));
        assert!(needs_login.is_empty());
        let events = sweep(&client, &accounts, &mut needs_login).await;
        assert!(matches!(
            &events[..],
            [MaintenanceEvent::NeedsLogin {
                failure: Failure::Reused,
                ..
            }]
        ));
        assert!(sweep(&client, &accounts, &mut needs_login).await.is_empty());
        assert_eq!(
            events[0].to_string(),
            format!(
                "codex: one needs an interactive `CODEX_HOME={} codex login` ({})",
                accounts[0].home().display(),
                Failure::Reused.label()
            )
        );
    }

    #[tokio::test]
    async fn a_challenged_probe_does_not_force_a_refresh() {
        let dir = tempfile::tempdir().unwrap();
        let accounts = vec![profile(dir.path(), "one", "r1")];
        let backend = Backend::default().usage_answers(vec![challenge()]);
        let client = backend.client().await;
        let events = sweep(&client, &accounts, &mut BTreeSet::new()).await;
        assert!(events.is_empty());
        assert!(backend.requests("/token").is_empty());
    }

    #[test]
    fn report_rows_and_warnings_read_plainly() {
        let report = RefreshReport {
            account: "one".into(),
            outcome: Ok(Refreshed::Fresh),
        };
        assert_eq!(report.to_string(), "one          still fresh");
        let warning = Warning::Forced {
            account: "one".into(),
            state: State::Limited { reason: None },
        };
        assert_eq!(
            warning.to_string(),
            "one is limited; migrating anyway because --force was given"
        );
    }
}
