use anyhow::{Context, Result, bail};

use crate::config::Config;
use crate::rotation::{self, RotateOptions};

/// Launch Claude Code against the shared startup OAuth setting.
///
/// `CLAUDE_CODE_OAUTH_TOKEN` is intentionally removed from the child process
/// environment so the token is not exposed in process listings. Claude loads
/// `.env.CLAUDE_CODE_OAUTH_TOKEN` from `~/.claude/settings.json` at startup.
/// Existing processes are not assumed to reload auth reliably.
pub async fn run(config: Config, auto: bool, claude_args: Vec<String>) -> Result<()> {
    if config.tokens.is_empty() {
        bail!("No tokens configured. Use `tokeman add <name> <key>` to add one.");
    }

    eprintln!(
        " \x1b[1mtokeman:\x1b[0m probing {} tokens and selecting the startup OAuth default...",
        config.tokens.len()
    );
    let outcome = rotation::rotate(
        &config,
        RotateOptions {
            // Rotation is sticky until the startup default crosses a policy
            // floor, preserving prompt-cache locality across the fleet.
            force: false,
            ..RotateOptions::default()
        },
    )
    .await?;
    eprintln!(" \x1b[1mtokeman:\x1b[0m {}", outcome.message);

    let claude_bin = std::env::var("TOKEMAN_CLAUDE_BIN")
        .ok()
        .or_else(|| config.settings.claude_bin.clone())
        .unwrap_or_else(|| "claude".into());

    let mut args = config.settings.launch_args.clone();
    args.extend(claude_args);
    if config.settings.dangerous_mode
        && !args
            .iter()
            .any(|arg| arg == "--dangerously-skip-permissions")
    {
        args.push("--dangerously-skip-permissions".into());
    }

    eprintln!(
        " \x1b[1mtokeman:\x1b[0m launching {}{}{}",
        claude_bin,
        if args.is_empty() { "" } else { " " },
        args.iter()
            .map(|arg| format!("{arg:?}"))
            .collect::<Vec<_>>()
            .join(" ")
    );
    if auto {
        eprintln!(
            " \x1b[1mtokeman:\x1b[0m default monitor enabled ({}s normal / {}s sip-and-drain); restart/resume to guarantee a changed credential",
            config.rotation.normal_probe_interval_secs, config.rotation.sip_probe_interval_secs
        );
    }
    eprintln!();

    let mut child = tokio::process::Command::new(&claude_bin)
        .args(&args)
        .env_remove("CLAUDE_CODE_OAUTH_TOKEN")
        .stdin(std::process::Stdio::inherit())
        .stdout(std::process::Stdio::inherit())
        .stderr(std::process::Stdio::inherit())
        .spawn()
        .with_context(|| format!("failed to launch {claude_bin}"))?;

    let monitor = if auto {
        let monitor_config = config.clone();
        Some(tokio::spawn(async move {
            let wake_interval = monitor_config.rotation.sip_probe_interval_secs.max(1);
            let mut wake = tokio::time::interval(std::time::Duration::from_secs(wake_interval));
            wake.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            // The launch itself just completed a probe.
            wake.tick().await;
            loop {
                wake.tick().await;
                match rotation::rotate(
                    &monitor_config,
                    RotateOptions {
                        scheduled: true,
                        ..RotateOptions::default()
                    },
                )
                .await
                {
                    Ok(outcome) if outcome.changed => {
                        eprintln!("\n \x1b[1mtokeman:\x1b[32m {}\x1b[0m", outcome.message);
                    }
                    Ok(outcome) if outcome.action == "no-viable-token" => {
                        eprintln!("\n \x1b[1mtokeman:\x1b[33m {}\x1b[0m", outcome.message);
                    }
                    Ok(_) => {}
                    Err(error) => {
                        eprintln!(
                            "\n \x1b[1mtokeman:\x1b[31m rotation probe failed: {error:#}\x1b[0m"
                        );
                    }
                }
            }
        }))
    } else {
        None
    };

    let status = child.wait().await;
    if let Some(handle) = monitor {
        handle.abort();
    }

    let status = status.context("failed while waiting for Claude Code")?;
    if status.success() {
        eprintln!("\n \x1b[1mtokeman:\x1b[0m claude exited cleanly");
    } else {
        eprintln!(
            "\n \x1b[1mtokeman:\x1b[33m claude exited with {}\x1b[0m",
            status
        );
    }
    Ok(())
}
