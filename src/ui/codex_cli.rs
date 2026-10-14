//! `tokeman codex ...`: printing for the Codex account commands.

use anyhow::{Result, bail};

use crate::account::Fleet;
use crate::cli::CodexAction;
use crate::codex::Client;
use crate::codex::commands::{self, WatchEvent};
use crate::paths::tilde;

pub async fn run(action: CodexAction) -> Result<()> {
    let now = chrono::Utc::now().timestamp();
    match action {
        CodexAction::Add {
            name,
            codex_home,
            label,
        } => {
            let account = commands::add(&name, &codex_home, label)?;
            println!("registered {} at {}", account.name, account.codex_home);
        }
        CodexAction::Remove { name } => {
            commands::remove(&name)?;
            println!("removed {name}");
        }
        CodexAction::Discover { adopt } => {
            let discovery = commands::discover(adopt)?;
            if discovery.profiles.is_empty() {
                println!("no Codex profiles found");
                return Ok(());
            }
            for profile in &discovery.profiles {
                println!(
                    "{} {:<16} {}",
                    if profile.registered { "*" } else { " " },
                    profile.name,
                    tilde(&profile.path)
                );
            }
            if adopt {
                println!("registered {} new profile(s)", discovery.adopted);
            } else if discovery.profiles.iter().any(|profile| !profile.registered) {
                println!("re-run with --adopt to register these");
            }
        }
        CodexAction::Import {
            from,
            homes_root,
            no_register,
        } => {
            let imported = commands::import(&from, homes_root.as_deref(), !no_register)?;
            for item in &imported {
                println!(
                    "{:<16} {}  {}{}{}",
                    item.name,
                    tilde(&item.home),
                    item.plan.as_deref().unwrap_or("plan unknown"),
                    item.expires_at
                        .map(|at| format!(", expires {}", crate::ui::text::format_reset(at)))
                        .unwrap_or_default(),
                    if item.registered {
                        ""
                    } else {
                        "  (not registered)"
                    }
                );
            }
            println!(
                "{} profile(s) seeded from access tokens; they cannot refresh and die when the token expires",
                imported.len()
            );
        }
        CodexAction::List { json } => {
            let fleet = Fleet::load()?;
            if fleet.codex.is_empty() {
                println!("no codex accounts configured — try `tokeman codex discover --adopt`");
                return Ok(());
            }
            let probes = commands::list(&Client::new()?, &fleet).await;
            if json {
                println!("{}", serde_json::to_string_pretty(&probes)?);
                return Ok(());
            }
            println!("{}", commands::ROW_HEADER);
            for probe in &probes {
                for row in commands::rows(probe, now) {
                    println!("{row}");
                }
            }
        }
        CodexAction::Refresh {
            name,
            force,
            max_age_days,
        } => {
            let fleet = Fleet::load()?;
            let reports = commands::refresh_accounts(
                &Client::new()?,
                &fleet,
                name.as_deref(),
                force,
                max_age_days,
            )
            .await?;
            let failed = reports
                .iter()
                .filter(|report| report.outcome.is_err())
                .count();
            for report in &reports {
                println!("{report}");
            }
            if failed > 0 {
                bail!("{failed} account(s) failed to refresh");
            }
        }
        CodexAction::Seat { account, into } => {
            let seated = commands::seat(&Client::new()?, &Fleet::load()?, &account, &into).await?;
            for warning in &seated.warnings {
                eprintln!("warning: {warning}");
            }
            println!("seated {} into {}", seated.account, tilde(&seated.into));
        }
        CodexAction::Reseat {
            codex_home,
            account,
            dry_run,
            force,
        } => {
            let reseat = commands::reseat(
                &Client::new()?,
                &Fleet::load()?,
                codex_home.as_deref(),
                account.as_deref(),
                dry_run,
                force,
            )
            .await?;
            for warning in &reseat.warnings {
                eprintln!("warning: {warning}");
            }
            match &reseat.moved {
                None => println!(
                    "would move {} (socket {}) onto {}",
                    tilde(&reseat.target),
                    tilde(&reseat.socket),
                    reseat.account
                ),
                Some(moved) => println!(
                    "moved {} onto {}: {} -> {}",
                    tilde(&reseat.target),
                    reseat.account,
                    moved
                        .before
                        .as_ref()
                        .map_or("unknown".to_string(), ToString::to_string),
                    moved
                        .after
                        .as_ref()
                        .map_or("unknown".to_string(), ToString::to_string)
                ),
            }
        }
        CodexAction::Watch {
            codex_home,
            min_remaining,
            once,
        } => {
            commands::watch(
                &Client::new()?,
                &Fleet::load()?,
                codex_home.as_deref(),
                min_remaining,
                once,
                |event| match event {
                    WatchEvent::Watching {
                        target,
                        min_remaining,
                    } => {
                        println!(
                            "watching {} (migrate below {:.0}% left)",
                            tilde(&target),
                            min_remaining * 100.0
                        )
                    }
                    WatchEvent::Headroom { remaining } => {
                        println!("{:.0}% left", remaining * 100.0)
                    }
                    WatchEvent::Migrating => println!("below the threshold; migrating"),
                    WatchEvent::Migrated(reseat) => println!("moved onto {}", reseat.account),
                },
            )
            .await?;
        }
        CodexAction::Best { limit } => {
            let probe = commands::best(&Client::new()?, &Fleet::load()?, limit.as_deref()).await?;
            println!("{}", probe.codex_home.display());
        }
    }
    Ok(())
}
