//! `tokeman services ...`: register, import and read the service accounts.

use anyhow::{Result, bail};

use crate::account::Fleet;
use crate::cli::{ImportSource, ServicesAction};
use crate::paths::tilde;
use crate::providers::{self, Service, ServiceAccount, Usage, agy};
use crate::store::Store;
use crate::ui::text;

pub async fn run(action: Option<ServicesAction>, json: bool) -> Result<()> {
    match action.unwrap_or(ServicesAction::List { json }) {
        ServicesAction::List { json: list_json } => {
            let services = Fleet::load()?.services;
            let usages = read_and_record(&services).await;
            if json || list_json {
                println!("{}", serde_json::to_string_pretty(&usages)?);
            } else {
                text::print_usages(&usages, seated(&services).await.as_deref());
            }
        }
        ServicesAction::Add {
            service,
            name,
            key_file,
            label,
        } => {
            let key_file = std::path::absolute(&key_file)?;
            let account = ServiceAccount {
                label,
                ..ServiceAccount::with_key_file(service.service(), &name, &tilde(&key_file))
            };
            // Refuse a file that cannot be read now rather than fail every poll.
            account.api_key()?;
            let replaced = add(account.clone())?;
            println!(
                "{} {name} ({}) reading its key from {}",
                if replaced { "updated" } else { "registered" },
                account.service.label(),
                account.key_file.as_deref().unwrap_or_default()
            );
            text::print_usages(&read_and_record(std::slice::from_ref(&account)).await, None);
        }
        ServicesAction::Remove { name } => {
            let (_, removed) = Fleet::update(|fleet| Ok(fleet.remove_service(&name)))?;
            if !removed {
                bail!("no service account named '{name}'");
            }
            println!("removed {name}");
        }
        ServicesAction::Import { source } => {
            let found = match source {
                // Shells out to `security` with its own bound.
                ImportSource::Antigravity => {
                    tokio::task::spawn_blocking(providers::antigravity::discover).await??
                }
            };
            if found.is_empty() {
                println!("no signed-in accounts found");
                return Ok(());
            }
            for account in &found {
                let replaced = add(account.clone())?;
                println!(
                    "{} {} ({})",
                    if replaced { "updated" } else { "registered" },
                    account.name,
                    account.service.label()
                );
            }
            text::print_usages(
                &read_and_record(&found).await,
                seated(&found).await.as_deref(),
            );
        }
        ServicesAction::Use { name, previous } => {
            let seating = match name {
                Some(name) if !previous => {
                    let fleet = Fleet::load()?;
                    let account = fleet
                        .service_account(&name)
                        .ok_or_else(|| anyhow::anyhow!("no service account named '{name}'"))?;
                    agy::seat(providers::client(), account).await?
                }
                _ => agy::seat_previous().await?,
            };
            println!("{}", seating_line(&seating));
            let running = agy::running().await;
            if seating.changed && running > 0 {
                println!(
                    "{running} running agy process(es) keep the account they started with; restart them to use {}",
                    seating.email
                );
            }
        }
        ServicesAction::Best { service, group } => {
            let services: Vec<_> = Fleet::load()?
                .services
                .into_iter()
                .filter(|account| account.service == service)
                .collect();
            if services.is_empty() {
                bail!("no {} accounts are registered", service.label());
            }
            let usages = read_and_record(&services).await;
            let ranked = providers::by_room(&usages, service, group.as_deref());
            let seated = seated(&services).await;
            text::print_ranking(&ranked, seated.as_deref());
            for usage in usages.iter().filter(|usage| usage.error.is_some()) {
                println!(
                    " -  {} could not be read: {}",
                    usage.account,
                    usage.error.as_deref().unwrap_or_default()
                );
            }
            let Some((best, _)) = ranked.first() else {
                bail!("no {} account has a readable limit", service.label());
            };
            if service == Service::Antigravity && seated.as_deref() != Some(best.account.as_str()) {
                println!("to switch agy: tokeman services use {}", best.account);
            }
        }
    }
    Ok(())
}

fn seating_line(seating: &agy::Seated) -> String {
    match (&seating.previous, seating.changed) {
        (_, false) => format!("agy is already signed in as {}", seating.email),
        (Some(previous), true) => format!(
            "agy now signs in as {} (was {previous}; `tokeman services use --previous` puts it back)",
            seating.email
        ),
        (None, true) => format!("agy now signs in as {}", seating.email),
    }
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

/// Register `account`, replacing one of the same name; returns whether it replaced.
fn add(account: ServiceAccount) -> Result<bool> {
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
pub async fn read_and_record(accounts: &[ServiceAccount]) -> Vec<Usage> {
    let usages = providers::read_all(
        providers::client(),
        accounts,
        chrono::Utc::now().timestamp(),
    )
    .await;
    if let Ok(store) = Store::open() {
        for usage in &usages {
            let _ = store.insert_usage(usage);
        }
    }
    usages
}
