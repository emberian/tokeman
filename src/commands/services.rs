//! `tokeman agy|glm|kimi|devin|grok ...`: register, import, read and (agy)
//! seat the accounts of the services tokeman does not rotate.

use anyhow::{Result, bail};
use std::path::Path;

use super::view;
use crate::account::Fleet;
use crate::paths::tilde;
use crate::providers::antigravity::{LoginSession, run, seat};
use crate::providers::{self, ServiceAccount};
use crate::quota::{Reading, Service};
use crate::util::one_line;

/// A registration: the account as stored, and whether it replaced one.
#[derive(Debug, Clone, PartialEq)]
pub struct Registered {
    pub account: ServiceAccount,
    pub replaced: bool,
}

/// Register an API-key account that reads its key from `key_file`, and read
/// it once. A file that cannot be read now is refused rather than left to
/// fail every poll.
pub async fn add(
    service: Service,
    name: &str,
    key_file: &Path,
    label: Option<String>,
) -> Result<(Registered, Reading)> {
    let key_file = std::path::absolute(key_file)?;
    let account = ServiceAccount {
        label,
        ..ServiceAccount::with_key_file(service, name, &tilde(&key_file))
    };
    account.api_key()?;
    let replaced = register(account.clone())?;
    let mut readings = read(std::slice::from_ref(&account)).await;
    let reading = readings
        .pop()
        .unwrap_or_else(|| Reading::empty(service, name, super::now()));
    Ok((Registered { account, replaced }, reading))
}

/// Unregister `name`, which must be an account of `service`.
pub fn remove(service: Service, name: &str) -> Result<()> {
    Fleet::update(|fleet| {
        match fleet.service_account(name) {
            Some(account) if account.service == service => {}
            Some(account) => bail!(
                "'{name}' is a {} account; use `tokeman {} remove {name}`",
                account.service.label(),
                command_word(account.service)
            ),
            None => bail!("no {} account named '{name}'", service.label()),
        }
        fleet.remove_service(name);
        Ok(())
    })?;
    Ok(())
}

/// The word that selects `service` on the command line.
pub fn command_word(service: Service) -> &'static str {
    match service {
        Service::Claude => "claude",
        Service::Codex => "codex",
        Service::Antigravity => "agy",
        Service::Zai => "glm",
        Service::Kimi => "kimi",
        Service::Devin => "devin",
        Service::Grok => "grok",
    }
}

/// Register the login the Devin or grok CLI keeps on this machine
/// (`tokeman devin|grok import`), and read it once. The account holds the
/// path of the CLI's own login file, never a copy of what is in it. It is
/// named `name`, else by the email the login signs in as; a name that is not
/// that email keeps the email as its label.
pub async fn import_login(
    service: Service,
    name: Option<&str>,
    file: Option<&Path>,
    label: Option<String>,
) -> Result<(Registered, Reading)> {
    let file = match (file, service) {
        (Some(file), _) => std::path::absolute(file)?,
        (None, Service::Devin) => providers::devin::credentials_file(),
        (None, Service::Grok) => providers::grok::auth_file(),
        (None, other) => bail!("{} has no login file to import", other.label()),
    };
    if !file.is_file() {
        bail!(
            "no {} login at {} (sign in with `{}` first, or name the file with --file)",
            service.label(),
            file.display(),
            match service {
                Service::Devin => "devin auth login",
                _ => "grok login",
            }
        );
    }
    let located = ServiceAccount::with_key_file(service, service.name(), &tilde(&file));
    let email = match service {
        Service::Devin => Some(
            providers::devin::email(providers::client(), &located)
                .await
                .map_err(|error| anyhow::anyhow!("could not read the Devin login: {error}"))?,
        ),
        _ => providers::grok::email(&file)?,
    };
    let name = name.map(str::trim).filter(|name| !name.is_empty());
    let account = ServiceAccount {
        name: name
            .or(email.as_deref())
            .unwrap_or(service.name())
            .to_owned(),
        // An alias keeps the email it stands for, which is also how Grok
        // finds its login in a file that holds several.
        label: label.or_else(|| email.filter(|email| name.is_some_and(|name| name != email))),
        ..located
    };
    let replaced = register(account.clone())?;
    let mut readings = read(std::slice::from_ref(&account)).await;
    let reading = readings
        .pop()
        .unwrap_or_else(|| Reading::empty(service, &account.name, super::now()));
    Ok((Registered { account, replaced }, reading))
}

/// What `tokeman agy import` found and read.
#[derive(Debug, Clone, PartialEq)]
pub struct Imported {
    pub registered: Vec<Registered>,
    pub readings: Vec<Reading>,
    /// The imported account agy is signed in as.
    pub seated: Option<String>,
}

/// Register the Antigravity accounts signed in on this machine (agy's
/// Keychain item or token file, the app's state database), and read them.
pub async fn import_antigravity() -> Result<Imported> {
    // Shells out to `security` with its own bound.
    let found = tokio::task::spawn_blocking(providers::antigravity::discover).await??;
    let mut registered = Vec::new();
    for account in &found {
        registered.push(Registered {
            replaced: register(account.clone())?,
            account: account.clone(),
        });
    }
    Ok(Imported {
        registered,
        readings: read(&found).await,
        seated: seated(&found).await,
    })
}

/// The name of the account in `accounts` the agy CLI is signed in as. Asks
/// the Keychain only when an Antigravity account is registered.
pub async fn seated(accounts: &[ServiceAccount]) -> Option<String> {
    if !accounts
        .iter()
        .any(|account| account.service == Service::Antigravity)
    {
        return None;
    }
    let accounts = accounts.to_vec();
    tokio::task::spawn_blocking(move || seat::seated(&accounts))
        .await
        .ok()?
}

/// Register `account`, replacing one of the same name; whether it replaced.
fn register(account: ServiceAccount) -> Result<bool> {
    let (_, replaced) = Fleet::update(|fleet| {
        if let Some(existing) = fleet.service_account(&account.name)
            && existing.service != account.service
        {
            bail!(
                "'{}' is already a {} account; remove it first or pick another name",
                account.name,
                existing.service.label()
            );
        }
        let existing = fleet.service_account(&account.name);
        let replaced = existing.is_some();
        // A re-import or re-add brings fresh credentials, not a new label.
        let label = account
            .label
            .clone()
            .or_else(|| existing.and_then(|existing| existing.label.clone()));
        fleet.upsert_service(ServiceAccount { label, ..account });
        Ok(replaced)
    })?;
    Ok(replaced)
}

/// Read `accounts` now and keep the reads in the history.
async fn read(accounts: &[ServiceAccount]) -> Vec<Reading> {
    let readings = providers::read_all(providers::client(), accounts, super::now()).await;
    view::record(&readings);
    readings
}

/// What `tokeman agy login` registered and read.
#[derive(Debug, Clone, PartialEq)]
pub struct LoggedIn {
    pub registered: Registered,
    pub reading: Reading,
    /// Whether `agy`'s Keychain slot currently holds this account.
    pub seated: bool,
}

/// Start a browser login for an Antigravity account (`tokeman agy login`).
/// `agy`'s Keychain slot is never touched.
pub async fn begin_agy_login(name: Option<&str>) -> Result<LoginSession> {
    providers::antigravity::begin_login(name).await
}

fn resolve_login_account(
    requested: Option<&str>,
    mut granted: ServiceAccount,
) -> Result<ServiceAccount> {
    let email = granted.name.clone();
    match requested.map(str::trim).filter(|name| !name.is_empty()) {
        None => Ok(granted),
        Some(wanted) if wanted.contains('@') => {
            if !wanted.eq_ignore_ascii_case(&email) {
                bail!("signed in as {email}, not {wanted}; nothing was stored");
            }
            Ok(granted)
        }
        Some(alias) => {
            granted.name = alias.to_owned();
            granted.label = Some(email);
            Ok(granted)
        }
    }
}

/// Exchange the authorization code from [`begin_agy_login`], register the
/// account in `tokens.toml`, and read its quota once.
pub async fn finish_agy_login(
    name: Option<&str>,
    session: &LoginSession,
    code: &str,
) -> Result<LoggedIn> {
    let now_ms = super::now() * 1000;
    let granted =
        providers::antigravity::exchange_code(providers::client(), session, code, now_ms).await?;
    let account = resolve_login_account(name, granted)?;
    let replaced = register(account.clone())?;
    let slice = std::slice::from_ref(&account);
    let mut readings = read(slice).await;
    let reading = readings
        .pop()
        .unwrap_or_else(|| Reading::empty(Service::Antigravity, &account.name, super::now()));
    let is_seated = seated(slice).await.as_deref() == Some(account.name.as_str());
    Ok(LoggedIn {
        registered: Registered { account, replaced },
        reading,
        seated: is_seated,
    })
}

/// Which login `tokeman agy use` puts in `agy`'s slot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgyPick<'a> {
    /// This registered Antigravity account.
    Named(&'a str),
    /// The registered Antigravity account with the most room left.
    Best { group: Option<&'a str> },
    /// The login the last `use` replaced.
    Previous,
}

/// Rank Antigravity accounts from `readings`. With no explicit `group`,
/// accounts with room in every group rank first; if every account has at least
/// one exhausted group, fall back to `startup_group` (`agy`'s configured model
/// group) so an exhausted unused group never blocks `agy use`, `agy best`, or
/// `agy launch`.
pub(crate) fn rank_agy<'a>(
    readings: &'a [Reading],
    group: Option<&str>,
    startup_group: &'static str,
) -> (Vec<(&'a Reading, f64)>, Option<String>) {
    let rank = |scope: Option<&str>| {
        view::ranked(readings, view::ANY_ROOM, None, view::RankOn::Limits(scope))
    };
    let primary = rank(group);
    if !primary.is_empty() || group.is_some() {
        return (primary, group.map(str::to_owned));
    }
    let fallback = rank(Some(startup_group));
    if fallback.is_empty() {
        (fallback, None)
    } else {
        (fallback, Some(startup_group.to_owned()))
    }
}

/// Pick the best Antigravity account from `readings`.
fn pick_best_agy<'a>(
    fleet: &'a Fleet,
    readings: &[Reading],
    group: Option<&str>,
    startup_group: &'static str,
) -> Result<(&'a ServiceAccount, f64, Option<String>)> {
    let (ranked, chosen_group) = rank_agy(readings, group, startup_group);
    if let Some((reading, room)) = ranked.into_iter().next() {
        let account = fleet
            .service_account(&reading.account)
            .filter(|account| account.service == Service::Antigravity)
            .ok_or_else(|| anyhow::anyhow!("no Antigravity account named '{}'", reading.account))?;
        return Ok((account, room, chosen_group));
    }
    let unread: Vec<String> = readings
        .iter()
        .filter_map(|reading| {
            let error = reading.error.as_ref()?;
            Some(format!(
                "{}: {}",
                reading.account,
                one_line(&error.to_string(), 120)
            ))
        })
        .collect();
    let scope_note = match group {
        Some(group) => format!(" for group '{group}'"),
        None => String::new(),
    };
    if unread.is_empty() {
        bail!("no Antigravity account has room in a readable limit{scope_note}");
    }
    bail!(
        "no Antigravity account has room in a readable limit{scope_note} ({})",
        unread.join("; ")
    )
}

/// Sign the agy CLI in as `pick`. Running clients are looked at first: an
/// open Antigravity app is told to follow, an interactive agy refuses the
/// switch unless `force`. A refusal is a report, not an error.
pub async fn agy_use(
    fleet: &Fleet,
    pick: AgyPick<'_>,
    options: seat::UseOptions,
) -> Result<seat::UseReport> {
    let (target, chosen_room) = match pick {
        AgyPick::Previous => (seat::UseTarget::Previous, None),
        AgyPick::Named(name) => (
            seat::UseTarget::Account(
                fleet
                    .service_account(name)
                    .filter(|account| account.service == Service::Antigravity)
                    .ok_or_else(|| anyhow::anyhow!("no Antigravity account named '{name}'"))?,
            ),
            None,
        ),
        AgyPick::Best { group } => {
            let accounts: Vec<ServiceAccount> = fleet
                .services
                .iter()
                .filter(|account| account.service == Service::Antigravity)
                .cloned()
                .collect();
            if accounts.is_empty() {
                bail!(
                    "no Antigravity accounts are registered; run `tokeman agy import` or `tokeman agy login`"
                );
            }
            let readings = providers::read_all_opts(
                providers::client(),
                &accounts,
                super::now(),
                !options.dry_run,
            )
            .await;
            if !options.dry_run {
                view::record(&readings);
            }
            let (account, room, chosen_group) =
                pick_best_agy(fleet, &readings, group, seat::startup_group())?;
            (
                seat::UseTarget::Account(account),
                Some((room, chosen_group)),
            )
        }
    };
    let mut report = seat::use_account(providers::client(), target, options).await?;
    if let Some((room, group)) = chosen_room {
        report.room = Some(room);
        report.group = group;
    }
    Ok(report)
}

/// Get agy ready to run as `name` beside the normal login: plain `agy` when
/// the slot already holds that account, otherwise on the account's own home
/// with a token file from tokeman's grant. The caller execs it.
pub async fn agy_run(fleet: &Fleet, name: &str, args: Vec<String>) -> Result<run::Launch> {
    let account = fleet
        .service_account(name)
        .filter(|account| account.service == Service::Antigravity)
        .ok_or_else(|| anyhow::anyhow!("no Antigravity account named '{name}'"))?;
    let over_ssh = std::env::var_os("SSH_TTY").is_some()
        || std::env::var_os("SSH_CLIENT").is_some()
        || std::env::var_os("SSH_CONNECTION").is_some();
    let seated = if over_ssh {
        false
    } else {
        // Shells out to `security` with its own bound; an unreadable slot is
        // "not this account", which takes the home of its own.
        let held = account.clone();
        tokio::task::spawn_blocking(move || {
            seat::slot()
                .ok()
                .flatten()
                .is_some_and(|slot| slot.holds(&held))
        })
        .await?
    };
    run::prepare(providers::client(), account, seated, args).await
}

/// The agy sessions `tokeman agy run` started that are still running.
pub async fn agy_sessions(fleet: &Fleet) -> Result<Vec<run::Session>> {
    run::sessions(&fleet.services).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::quota::{ReadError, Window};

    fn agy_account(name: &str) -> ServiceAccount {
        ServiceAccount {
            name: name.to_owned(),
            service: Service::Antigravity,
            key: None,
            key_file: None,
            refresh_token: Some("1//r".into()),
            access_token: None,
            expires_at: None,
            label: None,
        }
    }

    fn agy_reading(account: &str, gemini_used: f64, claude_used: f64) -> Reading {
        Reading {
            limits: vec![
                providers::limit(
                    Some("Gemini Models".into()),
                    "weekly",
                    Some(7 * 86_400),
                    Window::new(gemini_used, 0),
                    None,
                ),
                providers::limit(
                    Some("Claude and GPT models".into()),
                    "weekly",
                    Some(7 * 86_400),
                    Window::new(claude_used, 0),
                    None,
                ),
            ],
            ..Reading::empty(Service::Antigravity, account, 0)
        }
    }

    #[test]
    fn best_agy_for_use_prefers_all_groups_and_falls_back_to_the_startup_group() {
        let fleet = Fleet {
            services: vec![agy_account("a@example.com"), agy_account("b@example.com")],
            ..Fleet::default()
        };
        // Only b has room in every group (a spent Gemini).
        let readings = [
            agy_reading("a@example.com", 1.0, 0.0),
            agy_reading("b@example.com", 0.1, 0.2),
        ];
        let (chosen, room, group) = pick_best_agy(&fleet, &readings, None, "gemini").unwrap();
        assert_eq!(chosen.name, "b@example.com");
        assert!((room - 0.8).abs() < 1e-9);
        assert_eq!(group, None);

        // Both exhausted Claude, b still has 85% Gemini: fallback to startup_group lands on b.
        let readings = [
            agy_reading("a@example.com", 1.0, 1.0),
            agy_reading("b@example.com", 0.15, 1.0),
        ];
        let (chosen, room, group) = pick_best_agy(&fleet, &readings, None, "gemini").unwrap();
        assert_eq!(chosen.name, "b@example.com");
        assert!((room - 0.85).abs() < 1e-9);
        assert_eq!(group.as_deref(), Some("gemini"));

        // Explicit --group claude refuses when Claude is spent everywhere.
        let err = pick_best_agy(&fleet, &readings, Some("claude"), "gemini")
            .unwrap_err()
            .to_string();
        assert!(err.contains("for group 'claude'"), "{err}");

        // Read errors are named when nothing ranks.
        let failed = [Reading::failed(
            Service::Antigravity,
            "a@example.com",
            0,
            ReadError::Network {
                message: "offline".into(),
            },
        )];
        let err = pick_best_agy(&fleet, &failed, None, "gemini")
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("a@example.com") && err.contains("offline"),
            "{err}"
        );
    }

    #[test]
    fn login_account_keeps_email_catches_wrong_account_and_labels_aliases() {
        let granted = agy_account("real@example.com");
        assert_eq!(
            resolve_login_account(None, granted.clone()).unwrap().name,
            "real@example.com"
        );
        assert_eq!(
            resolve_login_account(Some("REAL@example.com"), granted.clone())
                .unwrap()
                .name,
            "real@example.com"
        );
        let wrong = resolve_login_account(Some("other@example.com"), granted.clone())
            .unwrap_err()
            .to_string();
        assert!(wrong.contains("signed in as real@example.com"), "{wrong}");
        let aliased = resolve_login_account(Some("personal"), granted).unwrap();
        assert_eq!(aliased.name, "personal");
        assert_eq!(aliased.label.as_deref(), Some("real@example.com"));
    }
}
