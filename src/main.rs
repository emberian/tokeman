use anyhow::{Result, bail};
use clap::Parser;

use tokeman::account::Fleet;
use tokeman::cli::{
    self, AgyAction, ClaudeAction, Cli, CodexAction, Command, KeyAction, RotateAction,
    SettingsAction,
};
use tokeman::commands::{claude, codex, rotate, services, view};
use tokeman::daemon::cycle;
use tokeman::quota::Service;
use tokeman::ui::{prompt, text};

#[tokio::main]
async fn main() -> Result<()> {
    tokeman::notice::set_sink(text::notice);
    let args: Vec<String> = std::env::args().skip(1).collect();
    if let Some((old, new)) = cli::removed(&args) {
        bail!("`{old}` was removed in 0.4. Use: {new}");
    }
    let cli = Cli::parse();
    match cli.command {
        None if cli.watch => tokeman::ui::tui::run(Fleet::load()?, cli.fresh).await,
        None => {
            let fleet = Fleet::load()?;
            if fleet.account_keys().is_empty() {
                bail!("{}", text::NO_ACCOUNTS);
            }
            let overview = view::overview(&fleet, cli.fresh).await;
            if cli.json {
                text::print_json(&overview)
            } else {
                text::print_overview(&overview);
                Ok(())
            }
        }
        Some(command) => dispatch(command).await,
    }
}

async fn dispatch(command: Command) -> Result<()> {
    match command {
        Command::List { json } => {
            let overview = view::stored(&Fleet::load()?).await;
            if json {
                text::print_json(&overview)?;
            } else {
                text::print_list(&overview);
            }
        }
        Command::Claude { action } => claude_command(action).await?,
        Command::Codex { action } => codex_command(action).await?,
        Command::Agy { action } => agy_command(action).await?,
        Command::Glm { action } => key_command(Service::Zai, action).await?,
        Command::Kimi { action } => key_command(Service::Kimi, action).await?,
        Command::Launch { auto, args } => tokeman::launch::run(Fleet::load()?, auto, args).await?,
        Command::Rotate { action } => rotate_command(action).await?,
        Command::History {
            service,
            last,
            account,
            since,
            json,
        } => {
            let readings = view::history(service.into(), account.as_deref(), last, since)?;
            if json {
                text::print_json(&readings)?;
            } else {
                text::print_history(&readings);
            }
        }
        Command::Stats { service } => {
            let fleet = Fleet::load()?;
            let service = service.into();
            text::print_stats(
                &view::stats(&fleet, service),
                view::forecast(&fleet, service).as_ref(),
            )
        }
        Command::Notify { test: true } => {
            let delivery = view::notify_test().await;
            text::print_notify_test(&delivery);
            if let tokeman::daemon::notify::Delivery::Failed { reason, .. } = delivery {
                bail!("the notification was not delivered: {reason}");
            }
        }
        Command::Notify { test: false } => {
            let fleet = Fleet::load()?;
            text::print_attention_report(&view::stored(&fleet).await, fleet.launch.notifications)
        }
        Command::Chart {
            service,
            hours,
            metric,
            output,
            iterm,
            width,
            height,
        } => text::print_chart(&tokeman::ui::chart::run(&tokeman::ui::chart::Request {
            service: service.into(),
            hours,
            metric,
            output,
            iterm,
            width,
            height,
        })?),
        Command::Settings { action } => match action.unwrap_or(SettingsAction::Show) {
            SettingsAction::Show => text::print_settings(&view::settings()?),
            SettingsAction::Set { key, value } => {
                view::set_setting(&key, &value)?;
                text::print_setting_set(&key, &value);
            }
        },
        #[cfg(feature = "tray")]
        Command::Tray => tokeman::ui::tray::run(Fleet::load()?)?,
    }
    Ok(())
}

async fn claude_command(action: ClaudeAction) -> Result<()> {
    match action {
        ClaudeAction::Add { name, key } => {
            claude::add(&name, key)?;
            text::print_account_change("Added", &name);
        }
        ClaudeAction::Remove { name } => {
            claude::remove(&name)?;
            text::print_account_change("Removed", &name);
        }
        ClaudeAction::Login { name, more } => prompt::login(name.as_deref(), more).await?,
        ClaudeAction::Refresh { name } => {
            text::print_claude_refresh(&claude::refresh(name.as_deref()).await?)
        }
        ClaudeAction::List { json } => service_list(Service::Claude, json).await?,
        ClaudeAction::Use { name } => {
            claude::use_account(&Fleet::load()?, &name)?;
            text::print_claude_use(&name);
        }
        ClaudeAction::Best => best(Service::Claude, None).await?,
        ClaudeAction::Reset { name: None, .. } => {
            text::print_reset_offers(&claude::reset_offers(&Fleet::load()?).await)
        }
        ClaudeAction::Reset {
            name: Some(name),
            clear: true,
            ..
        } => text::print_reset_cleared(&name, claude::reset_clear(&name)?.as_ref()),
        ClaudeAction::Reset {
            name: Some(name),
            grant,
            session,
            yes,
            ..
        } => {
            let plan =
                claude::reset_plan(&Fleet::load()?, &name, grant.as_deref(), session).await?;
            text::print_reset_plan(&plan);
            if !yes && !prompt::confirm("use it now?")? {
                text::print_kept();
                return Ok(());
            }
            text::print_reset_claimed(&claude::reset_claim(&plan).await?);
        }
    }
    Ok(())
}

async fn codex_command(action: CodexAction) -> Result<()> {
    match action {
        CodexAction::Add {
            name,
            codex_home,
            label,
        } => text::print_codex_added(&codex::add(&name, &codex_home, label)?),
        CodexAction::Remove { name } => {
            codex::remove(&name)?;
            text::print_removed(&name);
        }
        CodexAction::Discover { adopt } => {
            text::print_codex_discovery(&codex::discover(adopt)?, adopt)
        }
        CodexAction::Import {
            from,
            homes_root,
            no_register,
        } => {
            text::print_codex_imported(&codex::import(&from, homes_root.as_deref(), !no_register)?)
        }
        CodexAction::List { json } => {
            let fleet = Fleet::load()?;
            if fleet.codex.is_empty() {
                text::print_no_accounts(Service::Codex);
                return Ok(());
            }
            let listed = codex::list(&fleet).await?;
            if json {
                text::print_json(&listed)?;
            } else {
                let probes = listed.iter().map(|entry| entry.probe.clone()).collect();
                let overview = view::shown(&fleet, probes, Some(Service::Codex)).await;
                text::print_codex_listed(&overview, &listed);
            }
        }
        CodexAction::Refresh {
            name,
            force,
            max_age_days,
        } => {
            let reports =
                codex::refresh(&Fleet::load()?, name.as_deref(), force, max_age_days).await?;
            text::print_codex_refresh(&reports);
            let failed = reports
                .iter()
                .filter(|report| report.outcome.is_err())
                .count();
            if failed > 0 {
                bail!("{failed} account(s) failed to refresh");
            }
        }
        CodexAction::Use {
            account,
            into,
            codex_home,
            dry_run,
            force,
        } => {
            let options = codex::UseOptions {
                into,
                codex_home,
                dry_run,
                force,
            };
            let used = codex::use_account(&Fleet::load()?, account.as_deref(), &options).await?;
            text::print_codex_used(&used);
        }
        CodexAction::Watch {
            codex_home,
            min_remaining,
            once,
        } => {
            codex::watch_running(
                &Fleet::load()?,
                codex_home.as_deref(),
                min_remaining,
                once,
                |event| text::print_codex_watch(&event),
            )
            .await?
        }
        CodexAction::Reset {
            account,
            clear: true,
            ..
        } => text::print_reset_cleared(&account, codex::reset_clear(&account)?.as_ref()),
        CodexAction::Reset {
            account,
            credit,
            yes,
            ..
        } => {
            let plan = codex::reset_plan(&Fleet::load()?, &account, credit.as_deref()).await?;
            let which = text::print_codex_reset_plan(&plan);
            if !yes && !prompt::confirm(&format!("spend {which} on {account} now?"))? {
                text::print_kept();
                return Ok(());
            }
            let claim = codex::reset_claim(&plan).await?;
            text::print_codex_reset_claimed(&account, &claim);
            if let Some(uncertain) = text::codex_reset_uncertain(&account, &claim) {
                bail!("{uncertain}");
            }
        }
        CodexAction::Best { limit } => best(Service::Codex, limit.as_deref()).await?,
    }
    Ok(())
}

async fn agy_command(action: AgyAction) -> Result<()> {
    match action {
        AgyAction::Import => text::print_agy_imported(&services::import_antigravity().await?),
        AgyAction::Remove { name } => {
            services::remove(Service::Antigravity, &name)?;
            text::print_removed(&name);
        }
        AgyAction::List { json } => {
            service_list(Service::Antigravity, json).await?;
            if !json {
                text::print_agy_sessions(&services::agy_sessions(&Fleet::load()?).await);
            }
        }
        AgyAction::Use {
            name,
            previous,
            force,
            dry_run,
        } => {
            let name = name.filter(|_| !previous);
            let options = tokeman::providers::antigravity::seat::UseOptions { force, dry_run };
            let report = services::agy_use(&Fleet::load()?, name.as_deref(), options).await?;
            text::print_agy_use(&report);
            if report.refused() {
                bail!("agy's login was not changed");
            }
        }
        AgyAction::Best { group } => best(Service::Antigravity, group.as_deref()).await?,
        AgyAction::Run { name, args } => {
            let launch = services::agy_run(&Fleet::load()?, &name, args).await?;
            text::print_agy_run(&launch);
            return Err(launch.exec());
        }
    }
    Ok(())
}

async fn key_command(service: Service, action: KeyAction) -> Result<()> {
    match action {
        KeyAction::Add {
            name,
            key_file,
            label,
        } => {
            let (registered, reading) = services::add(service, &name, &key_file, label).await?;
            text::print_service_added(&registered, &reading);
        }
        KeyAction::Remove { name } => {
            services::remove(service, &name)?;
            text::print_removed(&name);
        }
        KeyAction::List { json } => service_list(service, json).await?,
        KeyAction::Best { group } => best(service, group.as_deref()).await?,
    }
    Ok(())
}

/// `tokeman <service> list`: read the service's accounts now and show them.
async fn service_list(service: Service, json: bool) -> Result<()> {
    let overview = view::fresh(&Fleet::load()?, Some(service)).await;
    if json {
        text::print_json(&overview.readings())
    } else if overview.accounts.is_empty() {
        text::print_no_accounts(service);
        Ok(())
    } else {
        text::print_overview(&overview);
        Ok(())
    }
}

/// `tokeman <service> best`: read, rank, print; an error when nothing ranks.
async fn best(service: Service, scope: Option<&str>) -> Result<()> {
    let best = view::best(&Fleet::load()?, service, scope).await?;
    text::print_best(&best);
    if best.winner().is_none() {
        bail!("{}", text::nothing_ranked(&best));
    }
    Ok(())
}

async fn rotate_command(action: RotateAction) -> Result<()> {
    match action {
        RotateAction::Run {
            force,
            dry_run,
            scheduled,
        } => {
            let options = cycle::Options {
                force,
                dry_run,
                scheduled,
            };
            text::print_outcome(&rotate::run(&Fleet::load()?, options).await?);
        }
        RotateAction::Daemon => rotate::daemon().await?,
        RotateAction::Status { json } => {
            let status = rotate::status(&Fleet::load()?).await?;
            if json {
                text::print_json(&status)?;
            } else {
                text::print_status(&status);
            }
        }
        RotateAction::Pause => {
            rotate::pause()?;
            text::print_paused();
        }
        RotateAction::Resume => text::print_resumed(&rotate::resume(&Fleet::load()?).await?),
        RotateAction::Install => text::print_installed(&rotate::install()?),
        RotateAction::Uninstall => {
            rotate::uninstall()?;
            text::print_uninstalled();
        }
        RotateAction::Quarantine {
            name,
            model,
            exact_model,
            window,
            until,
        } => {
            let quarantine = rotate::Quarantine {
                family: model.family(),
                exact_model,
                window: window.into(),
                until,
            };
            let outcome = rotate::quarantine(&Fleet::load()?, &name, quarantine).await?;
            text::print_quarantined(&name, &outcome);
        }
        RotateAction::ClearQuarantine { name, model } => text::print_quarantines_cleared(
            &name,
            rotate::clear_quarantine(&name, model.map(|model| model.family()))?,
        ),
        RotateAction::ObserveSession { pid } => {
            rotate::observe_session(pid, &prompt::stdin_bytes()?).await?
        }
    }
    Ok(())
}
