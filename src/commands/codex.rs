//! `tokeman codex ...`: register, discover and import profiles, read them,
//! refresh them, move a Codex onto another account, spend a reset credit.

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Utc};
use serde::Serialize;
use std::fmt;
use std::path::{Path, PathBuf};

use super::view;
use crate::account::{CodexAccount, Fleet};
use crate::codex::Client;
use crate::codex::appserver::{self, AccountInfo, AppServer, tightest_from_rate_limits};
use crate::codex::auth::{self, CodexHome, IdClaims};
use crate::codex::probe::{self, State};
use crate::codex::refresh::{self, DEFAULT_MAX_AGE_DAYS, RefreshError};
use crate::codex::resets::{self, CreditList};
use crate::paths::{expand_tilde, tilde};
use crate::quota::{ReadError, Reading, Span};

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

pub(crate) fn fmt_epoch(epoch: i64) -> String {
    DateTime::<Utc>::from_timestamp(epoch, 0)
        .map(|at| at.format("%Y-%m-%d %H:%MZ").to_string())
        .unwrap_or_else(|| epoch.to_string())
}

/// `tokeman codex list`: probe every profile, read the credit list of each
/// that holds credits, and keep the probes in the history.
pub async fn list(fleet: &Fleet) -> Result<Vec<Listed>> {
    let listed = list_detailed(&Client::new()?, fleet).await;
    let probes: Vec<Reading> = listed.iter().map(|entry| entry.probe.clone()).collect();
    view::record(&probes);
    Ok(listed)
}

/// A probe and, for an account holding reset credits, the credit list.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Listed {
    #[serde(flatten)]
    pub probe: Reading,
    /// Read only for an account the usage body says holds credits.
    pub reset_credit_list: Option<CreditList>,
    /// Why the credit list could not be read; the usage body's count stands.
    pub reset_credit_error: Option<ReadError>,
}

/// Probe every profile, then read the credit list of each account the usage
/// body says holds credits. A failed list leaves the usage body's count
/// standing.
pub async fn list_detailed(client: &Client, fleet: &Fleet) -> Vec<Listed> {
    let probes = probe::probe_all(client, &fleet.codex).await;
    let lists = futures::future::join_all(probes.iter().map(|probe| async {
        let holds = probe
            .codex()
            .and_then(|codex| codex.reset_credits)
            .is_some_and(|count| count.available > 0);
        let account = fleet.codex_account(&probe.account)?;
        if !holds {
            return None;
        }
        Some(resets::list(client, account).await)
    }))
    .await;
    probes
        .into_iter()
        .zip(lists)
        .map(|(probe, list)| {
            let (reset_credit_list, reset_credit_error) = match list {
                None => (None, None),
                Some(Ok(list)) => (Some(list), None),
                Some(Err(error)) => (None, Some(error)),
            };
            Listed {
                probe,
                reset_credit_list,
                reset_credit_error,
            }
        })
        .collect()
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
    let probes = probe::probe_all(client, &fleet.codex).await;
    let chosen = match account {
        Some(name) => probes
            .iter()
            .find(|probe| probe.account == name)
            .with_context(|| format!("no codex account named {name}"))?,
        None => view::codex_ranked(&probes, fleet, None)
            .first()
            .map(|(reading, _)| *reading)
            .context("no viable codex account to migrate to")?,
    };
    let mut warnings = Vec::new();
    if !chosen.accepts_work() {
        if !force {
            bail!(
                "{} is not viable right now: {} (pass --force to migrate anyway)",
                chosen.account,
                State::of(chosen)
            );
        }
        warnings.push(Warning::Forced {
            account: chosen.account.clone(),
            state: State::of(chosen),
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
    /// The tightest window the running Codex reports, and what is left in it.
    Headroom {
        remaining: f64,
        span: Span,
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
        if let Some((remaining, span)) = pending.as_ref().and_then(tightest_from_rate_limits) {
            on_event(WatchEvent::Headroom { remaining, span });
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

/// `tokeman codex refresh`.
pub async fn refresh(
    fleet: &Fleet,
    name: Option<&str>,
    force: bool,
    max_age_days: i64,
) -> Result<Vec<RefreshReport>> {
    refresh_accounts(&Client::new()?, fleet, name, force, max_age_days).await
}

/// What `tokeman codex use` did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Used {
    /// The account's credentials were copied into a profile directory.
    Seated(Seated),
    /// A running Codex was moved (or, on a dry run, would be).
    Reseated(Reseat),
}

/// Where `tokeman codex use` puts the account.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct UseOptions {
    /// Copy the credentials into this profile directory and stop there.
    pub into: Option<String>,
    /// The CODEX_HOME whose running Codex to move (default `~/.codex`).
    pub codex_home: Option<String>,
    pub dry_run: bool,
    pub force: bool,
}

/// `tokeman codex use [<account>]`: with `into`, copy the account's
/// credentials into another profile; otherwise move the running Codex onto
/// the account (default: the best viable one) over its app-server.
pub async fn use_account(
    fleet: &Fleet,
    account: Option<&str>,
    options: &UseOptions,
) -> Result<Used> {
    let client = Client::new()?;
    match &options.into {
        Some(into) => {
            let account = account.context("name the account to copy into the profile")?;
            Ok(Used::Seated(seat(&client, fleet, account, into).await?))
        }
        None => Ok(Used::Reseated(
            reseat(
                &client,
                fleet,
                options.codex_home.as_deref(),
                account,
                options.dry_run,
                options.force,
            )
            .await?,
        )),
    }
}

/// `tokeman codex watch`.
pub async fn watch_running(
    fleet: &Fleet,
    codex_home: Option<&str>,
    min_remaining: f64,
    once: bool,
    on_event: impl FnMut(WatchEvent),
) -> Result<()> {
    watch(
        &Client::new()?,
        fleet,
        codex_home,
        min_remaining,
        once,
        on_event,
    )
    .await
}

/// What `tokeman codex reset` would do, checked against the backend, to show
/// before asking.
pub async fn reset_plan(
    fleet: &Fleet,
    name: &str,
    credit: Option<&str>,
) -> Result<resets::ResetPlan> {
    let account = fleet
        .codex_account(name)
        .with_context(|| format!("no codex account named {name}"))?;
    resets::plan(
        &Client::new()?,
        account,
        credit,
        &crate::claims::Journal::default(),
    )
    .await
}

/// Forget `name`'s pending credit claim; the claim that was forgotten.
pub fn reset_clear(name: &str) -> Result<Option<crate::claims::Pending>> {
    crate::claims::Journal::default().clear(crate::quota::Service::Codex, name)
}

/// Send the claim `plan` describes.
pub async fn reset_claim(plan: &resets::ResetPlan) -> Result<resets::Claim> {
    resets::claim(
        &Client::new()?,
        plan,
        &crate::claims::Journal::default(),
        super::now(),
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codex::auth::tests::{id_token, jwt};
    use crate::codex::probe::tests::{Backend, ok, profile, weekly_only_body};
    use crate::testing::codex_account;
    use axum::http::StatusCode;
    use serde_json::json;

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

    #[tokio::test]
    async fn the_detailed_list_reads_credits_only_where_the_usage_body_counts_some() {
        let dir = tempfile::tempdir().unwrap();
        let holder = profile(dir.path(), "holder", "r1");
        let none = profile(dir.path(), "none", "r2");
        let mut empty = weekly_only_body(5.0);
        empty["rate_limit_reset_credits"]["available_count"] = json!(0);
        let backend = Backend::default()
            .usage_answers(vec![ok(weekly_only_body(5.0)), ok(empty)])
            .credit_answers(vec![(StatusCode::INTERNAL_SERVER_ERROR, "oops".into())]);
        let client = backend.client().await;
        let listed = list_detailed(&client, &fleet_of(vec![holder, none])).await;
        assert_eq!(backend.requests("/credits").len(), 1);
        assert_eq!(listed[0].reset_credit_list, None);
        assert_eq!(
            listed[0].reset_credit_error,
            Some(ReadError::Http {
                status: 500,
                body: "oops".into()
            })
        );
        assert_eq!(listed[1].reset_credit_list, None);
        assert_eq!(listed[1].reset_credit_error, None);
        let json = serde_json::to_value(&listed[1]).unwrap();
        assert_eq!(json["account"], "none");
        assert_eq!(json["service"], "codex");
        assert_eq!(json["detail"]["codex"]["reset_credits"]["available"], 0);
        assert_eq!(json["limits"][0]["span"], "weekly");
        assert_eq!(json["limits"][0]["scope"], "all");
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
