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
use crate::codex::appserver::{self, AccountInfo, AppServer};
use crate::codex::auth::{self, CodexHome, IdClaims};
use crate::codex::mover::{self, By, Look, MoveFloors, Outcome, Seat, Terms};
use crate::codex::probe::{self, State};
use crate::codex::refresh::{self, DEFAULT_MAX_AGE_DAYS, RefreshError};
use crate::codex::resets::{self, CreditList};
use crate::codex::seats::Seats;
use crate::daemon::codex::Standing;
use crate::paths::{expand_tilde, tilde};
use crate::quota::{ReadError, Reading};

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
    let home = crate::paths::home();
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
        None => crate::paths::home().join(".codex-homes"),
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
        .map(|(mut probe, list)| {
            if let Some(Ok(list)) = &list {
                list.date(&mut probe);
            }
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
    /// The push was not written to the seat file: nothing will renew the
    /// token the running Codex now holds.
    Unrecorded { message: String },
}

impl fmt::Display for Warning {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::RefreshFailed { account, error } => {
                write!(f, "could not refresh {account} first: {error}")
            }
            Self::Unrecorded { message } => write!(
                f,
                "the seat was not recorded, so its token will not be renewed before it expires: {message}"
            ),
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
        &Seats::default(),
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
    seats: &Seats,
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

    let account = fleet
        .codex_account(&chosen.account)
        .context("chosen account vanished from the fleet")?;
    let announced = mover::announce(client, account, target, seats, By::Use).await?;
    if let Some(error) = announced.refresh_error {
        result.warnings.push(Warning::RefreshFailed {
            account: chosen.account.clone(),
            error,
        });
    }
    if let Some(message) = announced.record_error {
        result.warnings.push(Warning::Unrecorded { message });
    }
    result.moved = Some(Moved {
        before: announced.before,
        after: announced.after,
    });
    Ok(result)
}

/// How `tokeman codex watch` runs.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct WatchOptions {
    /// The CODEX_HOME to follow (default `~/.codex`; a dry run without one
    /// looks at every home the daemon would).
    pub codex_home: Option<String>,
    /// One leaving floor for every window, instead of the `[rotation]` ones.
    pub min_remaining: Option<f64>,
    /// Look once and return.
    pub once: bool,
    /// Say what the daemon's look would do now; move and write nothing.
    pub dry_run: bool,
}

/// What `watch` reports as it goes.
#[derive(Debug, Clone, PartialEq)]
pub enum WatchEvent {
    /// A dry run: whether the daemon looks by itself.
    Daemon(Standing),
    /// A dry run: the newest recorded move.
    LastMove(mover::Move),
    /// Fewer than two unparked profiles: a Codex has nowhere to move.
    NowhereToMove {
        unparked: usize,
    },
    Watching {
        target: PathBuf,
        floors: MoveFloors,
    },
    Look(Look),
}

fn floors_for(fleet: &Fleet, min_remaining: Option<f64>) -> Result<MoveFloors> {
    let floors = MoveFloors::of(fleet);
    match min_remaining {
        None => Ok(floors),
        Some(min) if (0.0..1.0).contains(&min) => Ok(floors.leaving_at(min)),
        Some(min) => bail!("--min-remaining takes a fraction from 0 up to 1, not {min}"),
    }
}

/// Follow the Codex running on a CODEX_HOME and move it between turns once
/// it is below its leaving floor and another profile has the landing room:
/// the daemon's look ([`mover::consider`]), run on every push from Codex
/// instead of on a timer. `on_event` hears each look; a look that moved
/// carries the probes to keep and the move to record.
pub async fn watch(
    client: &Client,
    fleet: &Fleet,
    seats: &Seats,
    options: &WatchOptions,
    mut on_event: impl FnMut(WatchEvent),
) -> Result<()> {
    let floors = floors_for(fleet, options.min_remaining)?;
    let terms = Terms {
        floors,
        seats: seats.clone(),
        by: By::Watch,
    };
    let unparked = mover::landings(fleet).len();
    if unparked < 2 {
        on_event(WatchEvent::NowhereToMove { unparked });
        return Ok(());
    }
    if options.dry_run {
        let homes = match &options.codex_home {
            Some(home) => vec![target_home(Some(home))?],
            None => mover::homes(fleet),
        };
        for look in mover::sweep(client, fleet, &homes, &terms, true).await {
            on_event(WatchEvent::Look(look));
        }
        return Ok(());
    }
    let target = target_home(options.codex_home.as_deref())?;
    let socket = appserver::control_socket_path(target.path());
    if !socket.exists() {
        bail!(
            "no app-server control socket at {}; start Codex, or run `codex app-server --listen unix://`",
            tilde(&socket)
        );
    }
    let mut server = Some(AppServer::connect(&socket).await?);
    on_event(WatchEvent::Watching {
        target: target.path().to_path_buf(),
        floors,
    });
    loop {
        let mut connected = match server.take() {
            Some(connected) => connected,
            None => AppServer::connect(&socket).await?,
        };
        let seat = Seat::read(&mut connected).await?;
        // A move is announced on a connection of its own.
        let low = !crate::policy::above(&seat.reading, floors.leave, None);
        if low {
            connected.close().await;
        } else {
            server = Some(connected);
        }
        let look = mover::consider(client, fleet, &target, &seat, &terms, false).await;
        let moved = matches!(look.outcome, Outcome::Moved { .. });
        on_event(WatchEvent::Look(look));
        if options.once {
            return Ok(());
        }
        if moved {
            // Judge the account it landed on now.
            continue;
        }
        let mut connected = match server.take() {
            Some(connected) => connected,
            None => AppServer::connect(&socket).await?,
        };
        // Codex pushes this whenever its view changes; nothing to poll. The
        // push is sparse, so the next turn of the loop reads the whole view.
        connected
            .next_notification("account/rateLimits/updated")
            .await?;
        server = Some(connected);
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
        None => {
            let reseat = reseat(
                &client,
                fleet,
                options.codex_home.as_deref(),
                account,
                options.dry_run,
                options.force,
            )
            .await?;
            if let Some(moved) = &reseat.moved {
                let from = moved.before.as_ref().and_then(|info| info.email.clone());
                keep_move(&mover::Move {
                    at: super::now(),
                    home: tilde(&reseat.target),
                    from,
                    to: reseat.account.clone(),
                    remaining: None,
                    span: None,
                    by: By::Use,
                });
            }
            Ok(Used::Reseated(reseat))
        }
    }
}

/// Log a move and append it to the move history.
fn keep_move(moved: &mover::Move) {
    crate::daemon::log::log(&moved.to_string());
    if let Err(error) = mover::record(moved) {
        crate::notice::notice(&format!("the move was not recorded: {error:#}"));
    }
}

/// `tokeman codex watch`: [`watch`] against the live backend, keeping what
/// each look read and recording each move. A dry run first says where the
/// daemon stands and what moved last, and writes nothing.
pub async fn watch_running(
    fleet: &Fleet,
    options: &WatchOptions,
    mut on_event: impl FnMut(WatchEvent),
) -> Result<()> {
    if options.dry_run {
        on_event(WatchEvent::Daemon(Standing::of(fleet)));
        if let Some(last) = mover::moves().pop() {
            on_event(WatchEvent::LastMove(last));
        }
    }
    watch(
        &Client::new()?,
        fleet,
        &Seats::default(),
        options,
        |event| {
            if let WatchEvent::Look(look) = &event
                && !options.dry_run
            {
                view::record(&look.probed);
                if let Some(moved) = look.moved(By::Watch, super::now()) {
                    keep_move(&moved);
                }
            }
            on_event(event)
        },
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
    use crate::codex::probe::tests::{Backend, ok, profile, usage_body, weekly_only_body};
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
        let parsed = parse_import("# comment\n\n  ops@example.com   tok1 \nb\ttok2\n").unwrap();
        assert_eq!(
            parsed,
            vec![
                ("ops@example.com".to_owned(), "tok1".to_owned()),
                ("b".to_owned(), "tok2".to_owned())
            ]
        );
        let dir = tempfile::tempdir().unwrap();
        let text = format!("ops@example.com {}\n", id_token("ops@example.com", "acct-ops", "team"));
        let imported = import_into(&text, dir.path(), false).unwrap();
        assert_eq!(imported.len(), 1);
        assert_eq!(imported[0].home, dir.path().join("ops-example"));
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

    /// Two profiles and a Codex running on the first one's account.
    fn watched(
        dir: &Path,
        limits: serde_json::Value,
    ) -> (crate::codex::appserver::tests::FakeAppServer, Fleet) {
        use crate::codex::appserver::tests::{FakeAppServer, limits_answer};
        let fleet = fleet_of(vec![profile(dir, "a", "r-a"), profile(dir, "b", "r-b")]);
        let fake = FakeAppServer::start(vec![
            ("acct-a", "a@x.y", limits),
            ("acct-b", "b@x.y", limits_answer(10.0, 10.0)),
        ]);
        (fake, fleet)
    }

    #[tokio::test]
    async fn watch_once_moves_a_low_codex_and_a_dry_run_only_says_so() {
        use crate::codex::appserver::tests::limits_answer;
        let dir = tempfile::tempdir().unwrap();
        let (fake, fleet) = watched(dir.path(), limits_answer(50.0, 85.0));
        let backend = Backend::default().usage_answers(vec![ok(weekly_only_body(5.0))]);
        let client = backend.client().await;
        let home = fake.home.path().display().to_string();
        let seats = Seats::at(dir.path().join("codex-seats.json"));
        let run = async |options: WatchOptions| {
            let mut events = Vec::new();
            watch(&client, &fleet, &seats, &options, |event| {
                events.push(event)
            })
            .await
            .unwrap();
            events
        };

        let dry = run(WatchOptions {
            codex_home: Some(home.clone()),
            dry_run: true,
            ..WatchOptions::default()
        })
        .await;
        assert!(
            matches!(&dry[..], [WatchEvent::Look(look)]
                if look.outcome == Outcome::WouldMove { to: "b".into() }),
            "{dry:?}"
        );
        assert!(fake.logins().is_empty());

        // A floor under what is left keeps it where it is.
        let kept = run(WatchOptions {
            codex_home: Some(home.clone()),
            min_remaining: Some(0.10),
            once: true,
            ..WatchOptions::default()
        })
        .await;
        assert!(
            matches!(&kept[..], [WatchEvent::Watching { floors, .. }, WatchEvent::Look(look)]
                if look.outcome == Outcome::Healthy && floors.leave.weekly == 0.10),
            "{kept:?}"
        );

        let moved = run(WatchOptions {
            codex_home: Some(home),
            once: true,
            ..WatchOptions::default()
        })
        .await;
        let [WatchEvent::Watching { .. }, WatchEvent::Look(look)] = &moved[..] else {
            panic!("{moved:?}");
        };
        assert!(matches!(&look.outcome, Outcome::Moved { to, .. } if to == "b"));
        assert_eq!(look.seated.as_deref(), Some("a"));
        assert_eq!(look.probed.len(), 1);
        assert_eq!(fake.account_id(), "acct-b");
        assert_eq!(
            look.moved(By::Watch, 1).unwrap().span,
            Some(crate::quota::Span::Weekly)
        );
    }

    #[tokio::test]
    async fn watch_with_one_profile_says_there_is_nowhere_to_move() {
        let dir = tempfile::tempdir().unwrap();
        let mut fleet = fleet_of(vec![profile(dir.path(), "only", "r")]);
        let client = Backend::default().client().await;
        let seats = Seats::at(dir.path().join("codex-seats.json"));
        let mut events = Vec::new();
        let options = WatchOptions {
            dry_run: true,
            ..WatchOptions::default()
        };
        watch(&client, &fleet, &seats, &options, |event| {
            events.push(event)
        })
        .await
        .unwrap();
        assert_eq!(events, vec![WatchEvent::NowhereToMove { unparked: 1 }]);
        // A parked second profile is not somewhere to move.
        fleet.codex.push(CodexAccount {
            parked: true,
            ..profile(dir.path(), "parked", "r")
        });
        events.clear();
        watch(&client, &fleet, &seats, &WatchOptions::default(), |event| {
            events.push(event)
        })
        .await
        .unwrap();
        assert_eq!(events, vec![WatchEvent::NowhereToMove { unparked: 1 }]);
        let bad = WatchOptions {
            min_remaining: Some(1.5),
            ..WatchOptions::default()
        };
        assert!(watch(&client, &fleet, &seats, &bad, |_| {}).await.is_err());
    }

    #[tokio::test]
    async fn use_announces_the_named_account_to_the_running_codex() {
        use crate::codex::appserver::tests::limits_answer;
        let dir = tempfile::tempdir().unwrap();
        let (fake, fleet) = watched(dir.path(), limits_answer(10.0, 10.0));
        let backend = Backend::default().usage_answers(vec![ok(usage_body(10.0, 10.0))]);
        let client = backend.client().await;
        let target = CodexHome::new(fake.home.path());
        let seats = Seats::at(dir.path().join("codex-seats.json"));
        let dry = reseat_home(&client, &fleet, &target, &seats, Some("b"), true, false)
            .await
            .unwrap();
        assert_eq!(dry.moved, None);
        assert!(fake.logins().is_empty());
        let moved = reseat_home(&client, &fleet, &target, &seats, Some("b"), false, false)
            .await
            .unwrap();
        let reported = moved.moved.unwrap();
        assert_eq!(reported.before.unwrap().email.as_deref(), Some("a@x.y"));
        assert_eq!(reported.after.unwrap().email.as_deref(), Some("b@x.y"));
        assert_eq!(fake.logins()[0]["accessToken"], "access-b");
        assert!(moved.warnings.is_empty());
        let pushed = seats.on(&tilde(fake.home.path())).unwrap();
        assert_eq!((pushed.account.as_str(), pushed.by), ("b", By::Use));
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
