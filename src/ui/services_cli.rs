//! `tokeman services ...`: register, import and read the service accounts.

use anyhow::{Result, bail};

use crate::account::Fleet;
use crate::cli::{ImportSource, ServicesAction};
use crate::paths::tilde;
use crate::providers::{self, ServiceAccount, Usage};
use crate::store::Store;
use crate::ui::text;

pub async fn run(action: Option<ServicesAction>, json: bool) -> Result<()> {
    match action.unwrap_or(ServicesAction::List { json }) {
        ServicesAction::List { json: list_json } => {
            let usages = read_and_record(&Fleet::load()?.services).await;
            if json || list_json {
                println!("{}", serde_json::to_string_pretty(&usages)?);
            } else {
                text::print_usages(&usages);
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
            text::print_usages(&read_and_record(std::slice::from_ref(&account)).await);
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
            text::print_usages(&read_and_record(&found).await);
        }
    }
    Ok(())
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
