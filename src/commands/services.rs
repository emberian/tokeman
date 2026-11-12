//! `tokeman agy|glm|kimi ...`: register, import, read and (agy) seat the
//! accounts of the services tokeman does not rotate.

use anyhow::{Result, bail};
use std::path::Path;

use super::view;
use crate::account::Fleet;
use crate::paths::tilde;
use crate::providers::{self, ServiceAccount, agy};
use crate::quota::{Reading, Service};

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
    }
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

/// The accounts of one service, read now, and the one agy holds.
pub async fn list(fleet: &Fleet, service: Service) -> (Vec<Reading>, Option<String>) {
    let readings = view::probe(fleet, Some(service)).await;
    let seated = match service {
        Service::Antigravity => seated(&fleet.services).await,
        _ => None,
    };
    (readings, seated)
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
    tokio::task::spawn_blocking(move || agy::seated(&accounts))
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

/// What `tokeman agy use` did, as lines to show.
#[derive(Debug, Clone, PartialEq)]
pub struct AgyUse {
    pub lines: Vec<String>,
    /// Nothing was written because a running client would undo it.
    pub refused: bool,
}

/// Sign the agy CLI in as `name`, or put back the login the last use
/// replaced when `name` is `None`.
pub async fn agy_use(
    fleet: &Fleet,
    name: Option<&str>,
    force: bool,
    dry_run: bool,
) -> Result<AgyUse> {
    let _ = (force, dry_run);
    let seating = match name {
        Some(name) => {
            let account = fleet
                .service_account(name)
                .ok_or_else(|| anyhow::anyhow!("no Antigravity account named '{name}'"))?;
            agy::seat(providers::client(), account).await?
        }
        None => agy::seat_previous().await?,
    };
    Ok(AgyUse {
        lines: vec![format!("agy signs in as {}", seating.email)],
        refused: false,
    })
}
