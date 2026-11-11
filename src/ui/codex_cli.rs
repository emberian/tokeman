//! `tokeman codex ...`: printing for the Codex account commands.

use anyhow::{Context, Result, bail};
use std::io::{BufRead, Write};

use crate::account::Fleet;
use crate::cli::CodexAction;
use crate::codex::Client;
use crate::codex::commands::{self, WatchEvent};
use crate::codex::resets;
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
            let listed = commands::list_detailed(&Client::new()?, &fleet).await;
            let probes: Vec<_> = listed.iter().map(|entry| entry.probe.clone()).collect();
            commands::record(&probes);
            if json {
                println!("{}", serde_json::to_string_pretty(&listed)?);
                return Ok(());
            }
            println!("{}", commands::ROW_HEADER);
            for entry in &listed {
                for row in commands::listed_rows(entry, now) {
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
                    WatchEvent::Headroom { remaining, span } => {
                        println!("{:.0}% left in the {span} window", remaining * 100.0)
                    }
                    WatchEvent::Migrating => println!("below the threshold; migrating"),
                    WatchEvent::Migrated(reseat) => println!("moved onto {}", reseat.account),
                },
            )
            .await?;
        }
        CodexAction::Reset {
            account,
            credit,
            yes,
        } => reset(&account, credit.as_deref(), yes).await?,
        CodexAction::Best { limit } => {
            let probe = commands::best(&Client::new()?, &Fleet::load()?, limit.as_deref()).await?;
            println!("{}", probe.codex_home.display());
        }
    }
    Ok(())
}

/// Spend a reset credit after showing what it would clear. Credits are
/// scarce and cannot be taken back, so this asks unless `yes`.
async fn reset(name: &str, credit: Option<&str>, yes: bool) -> Result<()> {
    let fleet = Fleet::load()?;
    let account = fleet
        .codex_account(name)
        .with_context(|| format!("no codex account named {name}"))?;
    let client = Client::new()?;
    let journal = resets::Journal::default();
    let plan = resets::plan(&client, account, credit, &journal).await?;
    let now = chrono::Utc::now().timestamp();
    println!("{}", commands::ROW_HEADER);
    for row in commands::rows(&plan.probe, now) {
        println!("{row}");
    }
    match &plan.credits {
        Ok(list) => {
            for credit in &list.credits {
                println!("  └ reset credit {}", resets::describe(credit));
            }
        }
        Err(error) => println!("  └ reset credit details unavailable ({error})"),
    }
    if let Some(pending) = &plan.pending {
        println!(
            "an earlier claim (request {}, started {}) got no definite answer; this retries it with the same request id",
            pending.redeem_request_id,
            chrono::DateTime::from_timestamp(pending.started_at, 0)
                .map(|at| at
                    .with_timezone(&chrono::Local)
                    .format("%Y-%m-%d %H:%M")
                    .to_string())
                .unwrap_or_else(|| pending.started_at.to_string())
        );
    }
    let which = match (plan.chosen(), &plan.credit_id) {
        (Some(chosen), _) => resets::describe(chosen),
        (None, Some(id)) => format!("credit {id}"),
        (None, None) => "one reset credit (the backend's choice)".to_owned(),
    };
    if !yes {
        print!("spend {which} on {name} now? [y/N] ");
        std::io::stdout().flush().ok();
        let mut answer = String::new();
        std::io::stdin().lock().read_line(&mut answer)?;
        if !answer.trim().eq_ignore_ascii_case("y") {
            println!("kept.");
            return Ok(());
        }
    }
    match resets::claim(&client, &plan, &journal, now).await? {
        resets::Claim::Answered {
            consumed, retried, ..
        } => {
            use resets::ConsumeCode::*;
            let text = match consumed.code {
                Reset => format!("reset: {} window(s) cleared", consumed.windows_reset),
                AlreadyRedeemed if retried => {
                    "the earlier claim went through; nothing more was spent".to_owned()
                }
                AlreadyRedeemed => "already redeemed; nothing more was spent".to_owned(),
                NothingToReset => "nothing to reset; no credit was spent".to_owned(),
                NoCredit => "no credit to spend".to_owned(),
            };
            println!("{name}: {text}");
        }
        resets::Claim::Uncertain {
            redeem_request_id,
            error,
        } => bail!(
            "no definite answer ({error}); the claim may or may not have gone through. \
             Run `tokeman codex reset {name}` again: it resends request {redeem_request_id}, \
             which the backend redeems at most once"
        ),
    }
    Ok(())
}
