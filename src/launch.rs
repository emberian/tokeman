//! `tokeman launch`: pick the default, then start claude in this terminal.

use anyhow::{Context, Result, bail};

use crate::account::Fleet;
use crate::daemon::cycle::{self, Options};
use crate::notice::notice;

const TAG: &str = " \x1b[1mtokeman:\x1b[0m";

/// Run one cycle, then exec claude with the configured arguments. With
/// `auto`, keep cycling on the sip cadence while claude runs.
pub async fn run(fleet: Fleet, auto: bool, claude_args: Vec<String>) -> Result<()> {
    if fleet.accounts.is_empty() {
        bail!(
            "No accounts configured. Use `tokeman claude login <name>` or `tokeman claude add <name> <key>` to add one."
        );
    }
    notice(&format!(
        "{TAG} probing {} accounts and selecting the default...",
        fleet.accounts.len()
    ));
    let outcome = cycle::run(&fleet, Options::default()).await?;
    notice(&format!("{TAG} {}", outcome.message));
    let (binary, args) = fleet.launch.command(claude_args);
    let shown: Vec<String> = args.iter().map(|arg| format!("{arg:?}")).collect();
    notice(&format!("{TAG} launching {binary} {}", shown.join(" ")));
    if auto {
        notice(&format!(
            "{TAG} default monitor enabled ({}s normal / {}s sip-and-drain)",
            fleet.policy.normal_probe_interval_secs, fleet.policy.sip_probe_interval_secs
        ));
    }
    notice("");
    // The child reads its credential from settings.json like every other
    // session; an inherited token would pin it to the launch-time account.
    let mut child = tokio::process::Command::new(&binary)
        .args(&args)
        .env_remove("CLAUDE_CODE_OAUTH_TOKEN")
        .spawn()
        .with_context(|| format!("failed to launch {binary}"))?;
    let monitor = auto.then(|| {
        let fleet = fleet.clone();
        tokio::spawn(async move {
            let mut wake = tokio::time::interval(std::time::Duration::from_secs(
                fleet.policy.sip_probe_interval_secs.max(1),
            ));
            wake.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            wake.tick().await; // the launch itself just ran a cycle
            loop {
                wake.tick().await;
                let scheduled = Options {
                    scheduled: true,
                    ..Options::default()
                };
                match cycle::run(&fleet, scheduled).await {
                    Ok(outcome) if outcome.changed() => {
                        notice(&format!("\n{TAG}\x1b[32m {}\x1b[0m", outcome.message))
                    }
                    Ok(outcome) if outcome.action == cycle::Action::NoneViable => {
                        notice(&format!("\n{TAG}\x1b[33m {}\x1b[0m", outcome.message))
                    }
                    Ok(_) => {}
                    Err(error) => notice(&format!(
                        "\n{TAG}\x1b[31m rotation probe failed: {error:#}\x1b[0m"
                    )),
                }
            }
        })
    });
    let status = child.wait().await?;
    if let Some(monitor) = monitor {
        monitor.abort();
    }
    if !status.success() {
        std::process::exit(status.code().unwrap_or(1));
    }
    Ok(())
}
