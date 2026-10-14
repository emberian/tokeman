use anyhow::{Result, bail};
use clap::Parser;

use tokeman::account::Fleet;
use tokeman::cli::{Cli, Command, ResetAction, RotateAction, SettingsAction, UsageAction};
use tokeman::daemon::{self, cycle};
use tokeman::store::{Stats, Store};
use tokeman::ui::text;

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        None if cli.watch => tokeman::ui::tui::run(Fleet::load()?).await,
        None => probe_and_print(cli.json).await,
        Some(command) => dispatch(command).await,
    }
}

/// `tokeman` with no subcommand: probe every account and print the gauges.
async fn probe_and_print(json: bool) -> Result<()> {
    let fleet = Fleet::load()?;
    if fleet.accounts.is_empty() {
        bail!(
            "No accounts configured. Use `tokeman login <name>` or `tokeman add <name> <key>` to add one."
        );
    }
    let mut readings = tokeman::anthropic::probe::probe_all(
        tokeman::anthropic::Client::shared(),
        fleet.accounts.iter(),
        tokeman::anthropic::probe::PROBE_MODEL,
        tokeman::anthropic::probe::PROBE_TIMEOUT,
        chrono::Utc::now().timestamp(),
    )
    .await;
    let _ = tokeman::observe::scan_transcripts(&fleet.accounts);
    tokeman::observe::apply(&mut readings);
    if let Ok(store) = Store::open() {
        for reading in &readings {
            let _ = store.insert(reading);
        }
    }
    if json {
        println!("{}", serde_json::to_string_pretty(&readings)?);
    } else {
        text::print_readings(&readings, tokeman::anthropic::probe::PROBE_MODEL_LABEL);
    }
    Ok(())
}

async fn dispatch(command: Command) -> Result<()> {
    match command {
        Command::Add { name, key } => {
            Fleet::update(|fleet| {
                fleet.add_setup_key(&name, key);
                Ok(())
            })?;
            daemon::reconcile_after_login()?;
            println!("Added account '{name}'");
        }
        Command::Remove { name } => {
            let (_, removed) = Fleet::update(|fleet| Ok(fleet.remove_account(&name)))?;
            if !removed {
                bail!("Account '{name}' not found");
            }
            println!("Removed account '{name}'");
        }
        Command::Login { name, more } => {
            tokeman::ui::interactive::login_accounts(name.as_deref(), more).await?
        }
        Command::Refresh { name } => {
            let refreshed = daemon::refresh::refresh_now(name.as_deref()).await?;
            let now_ms = chrono::Utc::now().timestamp_millis();
            for account in Fleet::load()?
                .accounts
                .iter()
                .filter(|account| name.as_deref().is_none_or(|name| account.name == name))
                .filter(|account| account.is_refreshable())
            {
                println!(
                    "  {} — {}",
                    account.name,
                    text::describe_login(account, now_ms)
                );
            }
            println!("{refreshed} grant(s) refreshed.");
        }
        Command::List => {
            let fleet = Fleet::load()?;
            if fleet.accounts.is_empty() {
                println!(
                    "No accounts configured. Use `tokeman login <name>` or `tokeman add <name> <key>` to add one."
                );
            }
            let now_ms = chrono::Utc::now().timestamp_millis();
            let default = daemon::default_account(&fleet)?;
            for account in &fleet.accounts {
                let marker = if default.as_deref() == Some(account.name.as_str()) {
                    "*"
                } else {
                    " "
                };
                let setup = account
                    .setup_key
                    .as_deref()
                    .map(text::mask_key)
                    .unwrap_or_else(|| "no setup key".into());
                println!(
                    "{marker} {:<24} {setup}  {}",
                    account.name,
                    text::describe_login(account, now_ms)
                );
            }
        }
        Command::Launch { auto, args } => tokeman::launch::run(Fleet::load()?, auto, args).await?,
        Command::Rotate { action } => rotate(action).await?,
        Command::History {
            last,
            account,
            since,
            json,
        } => {
            let store = Store::open()?;
            let snapshots = match since {
                Some(hours) => {
                    let cutoff = chrono::Utc::now()
                        - chrono::Duration::milliseconds((hours * 3_600_000.0) as i64);
                    match account.as_deref() {
                        Some(name) => store.for_account_since(name, cutoff)?,
                        None => store.all_since(cutoff)?,
                    }
                }
                None => store.recent(account.as_deref(), last)?,
            };
            if json {
                println!("{}", serde_json::to_string_pretty(&snapshots)?);
            } else {
                text::print_history(&snapshots);
            }
        }
        Command::Stats => {
            let store = Store::open()?;
            let stats: Vec<Stats> = Fleet::load()?
                .accounts
                .iter()
                .filter_map(|account| {
                    let since = chrono::Utc::now() - chrono::Duration::days(7);
                    let snapshots = store.for_account_since(&account.name, since).ok()?;
                    Some(Stats::compute(&account.name, &snapshots))
                })
                .collect();
            text::print_stats(&stats);
        }
        Command::Chart {
            hours,
            metric,
            output,
            iterm,
            width,
            height,
        } => tokeman::ui::chart::run(&tokeman::ui::chart::Request {
            hours,
            metric,
            output,
            iterm,
            width,
            height,
        })?,
        Command::Resets { action } => match action.unwrap_or(ResetAction::List { name: None }) {
            ResetAction::List { name } => {
                tokeman::ui::interactive::list_resets(name.as_deref()).await?
            }
            ResetAction::Use {
                name,
                grant,
                session,
                yes,
            } => tokeman::ui::interactive::use_reset(&name, grant.as_deref(), session, yes).await?,
        },
        Command::Usage {
            action: UsageAction::Capture { name },
        } => {
            tokeman::ui::interactive::capture_usage(&name).await?;
        }
        Command::Settings { action } => match action.unwrap_or(SettingsAction::Show) {
            SettingsAction::Show => {
                for (key, value) in Fleet::load()?.launch.entries() {
                    println!("{key} = {value}");
                }
            }
            SettingsAction::Set { key, value } => {
                Fleet::update(|fleet| fleet.launch.set(&key, &value))?;
                println!("set {key} = {value}");
            }
        },
        Command::Codex { action } => tokeman::ui::codex_cli::run(action).await?,
        #[cfg(feature = "tray")]
        Command::Tray => tokeman::ui::tray::run(Fleet::load()?)?,
    }
    Ok(())
}

async fn rotate(action: RotateAction) -> Result<()> {
    match action {
        RotateAction::Run {
            force,
            dry_run,
            scheduled,
        } => {
            let fleet = Fleet::load()?;
            let outcome = cycle::run(
                &fleet,
                cycle::Options {
                    force,
                    dry_run,
                    scheduled,
                },
            )
            .await?;
            println!("{}", outcome.message);
        }
        RotateAction::Daemon => daemon::run().await?,
        RotateAction::Status { json } => {
            let status = daemon::status::report(&Fleet::load()?).await?;
            if json {
                println!("{}", serde_json::to_string_pretty(&status)?);
            } else {
                text::print_status(&status);
            }
        }
        RotateAction::Pause => {
            daemon::pause()?;
            println!("rotation paused; Claude Code will use /login credentials");
        }
        RotateAction::Resume => {
            let outcome = daemon::resume(&Fleet::load()?).await?;
            println!("rotation resumed; {}", outcome.message);
        }
        RotateAction::Use { name } => {
            daemon::activate(&Fleet::load()?, &name)?;
            println!(
                "set startup default to {name}; live sessions follow it from their next request"
            );
        }
        RotateAction::Install => {
            let plist = daemon::install()?;
            println!("installed and started {}", plist.display());
        }
        RotateAction::Uninstall => {
            daemon::uninstall()?;
            println!("uninstalled the rotation service");
        }
        RotateAction::Quarantine {
            name,
            model,
            exact_model,
            window,
            until,
        } => {
            let fleet = Fleet::load()?;
            if fleet.account(&name).is_none() {
                bail!("account '{name}' is not configured");
            }
            tokeman::observe::quarantine(
                &name,
                model.family(),
                exact_model.as_deref().map(tokeman::quota::ModelKey::new),
                window.into(),
                tokeman::observe::parse_reset_spec(&until)?,
            )?;
            println!("quarantined {name}");
            let outcome = cycle::run(&fleet, cycle::Options::default()).await?;
            println!("{}", outcome.message);
        }
        RotateAction::ClearQuarantine { name, model } => {
            let removed = tokeman::observe::clear_quarantine(&name, model.map(|m| m.family()))?;
            println!("removed {removed} quarantine(s) for {name}");
        }
        RotateAction::ObserveSession { pid } => {
            use std::io::Read;
            let mut input = Vec::new();
            std::io::stdin().read_to_end(&mut input)?;
            tokeman::observe::record_session_from_hook(pid, &input).await?;
        }
    }
    Ok(())
}
