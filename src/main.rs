mod admission;
mod chart;
mod config;
mod display;
mod launch;
mod probe;
mod rotation;
mod stats;
mod store;
#[cfg(feature = "tray")]
mod terminal;
#[cfg(feature = "tray")]
mod tray;
mod tui;
mod web;

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Duration, Utc};
use clap::{Parser, Subcommand, ValueEnum};

#[derive(Parser)]
#[command(name = "tokeman", about = "Anthropic token usage visualizer")]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,

    /// Launch live TUI dashboard
    #[arg(long)]
    watch: bool,

    /// Output probe results as JSON
    #[arg(long)]
    json: bool,
}

#[derive(Subcommand)]
enum Command {
    /// Add a token
    Add {
        /// Display name for the token
        name: String,
        /// API key (sk-ant-oat01-...)
        key: String,
    },
    /// Remove a token
    Remove {
        /// Name of the token to remove
        name: String,
    },
    /// Show recent snapshots
    History {
        /// Number of snapshots to show
        #[arg(long, default_value = "20")]
        last: usize,
        /// Filter to a specific token name
        #[arg(long)]
        token: Option<String>,
        /// Only show snapshots from the last N hours
        #[arg(long)]
        since: Option<f64>,
        /// Output as JSON
        #[arg(long)]
        json: bool,
    },
    /// Show computed usage rates and statistics
    Stats,
    /// List configured tokens
    List,
    /// Launch claude with the best available token
    Launch {
        /// Monitor the startup default while Claude runs
        #[arg(long)]
        auto: bool,
        /// Arguments to pass to claude
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Manage the OAuth default used by newly started Claude Code processes
    Rotate {
        #[command(subcommand)]
        action: RotateAction,
    },
    /// Browse all history in the browser with D3 charts
    Browse {
        /// Port to serve on
        #[arg(long, default_value = "9847")]
        port: u16,
        /// Don't auto-open the browser
        #[arg(long)]
        no_open: bool,
    },
    /// Render a history chart (inline when run in iTerm2)
    Chart {
        /// How many hours of history to display
        #[arg(long, default_value = "24")]
        hours: f64,
        /// Capacity window to chart
        #[arg(long, value_enum, default_value = "five-hour")]
        metric: chart::ChartMetric,
        /// Also save the PNG at this path
        #[arg(long)]
        output: Option<std::path::PathBuf>,
        /// Force iTerm2's inline-image escape sequence
        #[arg(long)]
        iterm: bool,
        /// Image width in pixels
        #[arg(long, default_value = "1400")]
        width: u32,
        /// Image height in pixels
        #[arg(long, default_value = "680")]
        height: u32,
    },
    /// Manage profile-scoped credentials for model-specific usage buckets
    Usage {
        #[command(subcommand)]
        action: UsageAction,
    },
    /// Run as a system tray application
    #[cfg(feature = "tray")]
    Tray,
}

#[derive(Subcommand)]
enum RotateAction {
    /// Probe and apply the rotation policy once
    Run {
        /// Select the best viable token even if the current default is healthy
        #[arg(long)]
        force: bool,
        /// Report the change without writing Claude settings
        #[arg(long)]
        dry_run: bool,
        /// Respect the adaptive cadence (used by the background service)
        #[arg(long, hide = true)]
        scheduled: bool,
    },
    /// Run the persistent background monitor (used by the service)
    #[command(hide = true)]
    Daemon,
    /// Probe and report the monitor, startup default, sessions, and policy mode
    Status {
        /// Output machine-readable JSON
        #[arg(long)]
        json: bool,
    },
    /// Pause rotation and restore /login credential precedence
    Pause,
    /// Resume rotation and select the best viable token now
    Resume,
    /// Explicitly set the default for newly started Claude processes
    Use {
        /// Configured token name
        name: String,
    },
    /// Install and start the per-user background rotation service
    Install,
    /// Stop and remove the per-user background rotation service
    Uninstall,
    /// Quarantine a model/account pair after an unobserved capacity rejection
    Quarantine {
        /// Configured token name
        name: String,
        /// Model family affected by the rejection
        #[arg(long, value_enum)]
        model: ModelArg,
        /// Concrete model affected (for example claude-opus-4-8)
        #[arg(long)]
        exact_model: Option<String>,
        /// Quota window that rejected the request
        #[arg(long, value_enum)]
        window: WindowArg,
        /// Reset as RFC3339 or a Unix timestamp
        #[arg(long)]
        until: String,
    },
    /// Remove observed/manual admission quarantines for an account
    ClearQuarantine {
        /// Configured token name
        name: String,
        /// Limit removal to one model family
        #[arg(long, value_enum)]
        model: Option<ModelArg>,
    },
    /// Record an exact Claude SessionStart account/transcript binding
    #[command(hide = true)]
    ObserveSession {
        /// Parent Claude process id supplied by the installed hook
        #[arg(long)]
        pid: u32,
    },
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum ModelArg {
    Opus,
    Sonnet,
    General,
}

impl From<ModelArg> for admission::ModelFamily {
    fn from(value: ModelArg) -> Self {
        match value {
            ModelArg::Opus => Self::Opus,
            ModelArg::Sonnet => Self::Sonnet,
            ModelArg::General => Self::General,
        }
    }
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum WindowArg {
    FiveHour,
    Weekly,
}

impl From<WindowArg> for admission::LimitWindow {
    fn from(value: WindowArg) -> Self {
        match value {
            WindowArg::FiveHour => Self::FiveHour,
            WindowArg::Weekly => Self::Weekly,
        }
    }
}

#[derive(Subcommand)]
enum UsageAction {
    /// Capture Claude's current /login credential for one configured account
    Capture {
        /// Configured tokeman account name
        name: String,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Some(Command::Add { name, key }) => {
            let mut cfg = config::Config::load()?;
            cfg.add_token(name.clone(), key);
            cfg.save()?;
            rotation::reconcile_keychain_after_login()?;
            println!("Added token '{name}'");
        }
        Some(Command::Remove { name }) => {
            let mut cfg = config::Config::load()?;
            if cfg.remove_token(&name) {
                cfg.save()?;
                println!("Removed token '{name}'");
            } else {
                bail!("Token '{name}' not found");
            }
        }
        Some(Command::List) => {
            let cfg = config::Config::load()?;
            if cfg.tokens.is_empty() {
                println!("No tokens configured. Use `tokeman add <name> <key>` to add one.");
            } else {
                for t in &cfg.tokens {
                    let masked = mask_key(&t.key);
                    println!(
                        "  {} — {} — model usage: {}",
                        t.name,
                        masked,
                        if t.usage_key.is_some() {
                            "captured"
                        } else {
                            "not captured"
                        }
                    );
                }
            }
        }
        Some(Command::History {
            last,
            token,
            since,
            json,
        }) => {
            let db = store::Store::open()?;
            let snapshots = if let Some(hours) = since {
                let cutoff = Utc::now() - Duration::milliseconds((hours * 3_600_000.0) as i64);
                match token.as_deref() {
                    Some(name) => db.for_token_since(name, cutoff)?,
                    None => db.all_since(cutoff)?,
                }
            } else {
                db.recent(token.as_deref(), last)?
            };
            if json {
                println!("{}", serde_json::to_string(&snapshots)?);
            } else {
                display::print_history(&snapshots);
            }
        }
        Some(Command::Stats) => {
            let cfg = config::Config::load()?;
            let db = store::Store::open()?;
            let since = Utc::now() - Duration::hours(24);

            let mut all_stats = Vec::new();
            for token in &cfg.tokens {
                let snaps = db.for_token_since(&token.name, since)?;
                all_stats.push(stats::compute_stats(&token.name, &snaps));
            }
            display::print_stats(&all_stats);
        }
        Some(Command::Launch { auto, args }) => {
            let cfg = config::Config::load()?;
            launch::run(cfg, auto, args).await?;
        }
        Some(Command::Rotate { action }) => {
            let cfg = config::Config::load()?;
            match action {
                RotateAction::Run {
                    force,
                    dry_run,
                    scheduled,
                } => {
                    let outcome = rotation::rotate(
                        &cfg,
                        rotation::RotateOptions {
                            force,
                            dry_run,
                            scheduled,
                        },
                    )
                    .await?;
                    if !scheduled || outcome.changed || outcome.action == "no-viable-token" {
                        println!("{}", outcome.message);
                    }
                }
                RotateAction::Daemon => {
                    rotation::daemon().await?;
                }
                RotateAction::Status { json } => {
                    let status = rotation::status(&cfg).await?;
                    if json {
                        println!("{}", serde_json::to_string_pretty(&status)?);
                    } else {
                        rotation::print_status(&status);
                    }
                }
                RotateAction::Pause => {
                    rotation::pause().await?;
                    println!("rotation paused; Claude Code will use /login credentials");
                }
                RotateAction::Resume => {
                    let outcome = rotation::resume(&cfg).await?;
                    println!("rotation resumed; {}", outcome.message);
                }
                RotateAction::Use { name } => {
                    rotation::activate_token(&cfg, &name)?;
                    println!(
                        "set startup default to {name}; restart/resume to guarantee existing sessions use it"
                    );
                }
                RotateAction::Install => {
                    let path = rotation::install_service()?;
                    println!("installed and started {}", path.display());
                }
                RotateAction::Uninstall => {
                    rotation::uninstall_service()?;
                    println!("stopped and removed the tokeman rotation service");
                }
                RotateAction::Quarantine {
                    name,
                    model,
                    exact_model,
                    window,
                    until,
                } => {
                    if !cfg.tokens.iter().any(|token| token.name == name) {
                        bail!("Token '{name}' not found");
                    }
                    let reset = admission::parse_reset_spec(&until)?;
                    let family = admission::ModelFamily::from(model);
                    if let Some(model_id) = exact_model.as_deref()
                        && admission::ModelFamily::from_model(model_id) != family
                    {
                        bail!(
                            "exact model '{model_id}' does not belong to the {} family",
                            family.label()
                        );
                    }
                    admission::record_manual_limit(
                        &name,
                        family,
                        exact_model.clone(),
                        window.into(),
                        reset,
                    )?;
                    println!(
                        "quarantined {name} {} {} until {}",
                        exact_model.as_deref().unwrap_or_else(|| family.label()),
                        admission::LimitWindow::from(window).label(),
                        DateTime::from_timestamp(reset, 0)
                            .map(|value| value.to_rfc3339())
                            .unwrap_or_else(|| reset.to_string())
                    );
                    let outcome =
                        rotation::rotate(&cfg, rotation::RotateOptions::default()).await?;
                    println!("{}", outcome.message);
                }
                RotateAction::ClearQuarantine { name, model } => {
                    let removed = admission::clear_limits(&name, model.map(Into::into))?;
                    println!("removed {removed} admission quarantine(s) for {name}");
                }
                RotateAction::ObserveSession { pid } => {
                    use std::io::Read;
                    let mut input = Vec::new();
                    std::io::stdin().read_to_end(&mut input)?;
                    admission::record_session_from_hook(pid, &input)?;
                }
            }
        }
        Some(Command::Browse { port, no_open }) => {
            web::serve(port, !no_open).await?;
        }
        Some(Command::Chart {
            hours,
            metric,
            output,
            iterm,
            width,
            height,
        }) => {
            chart::run(
                &config::Config::load()?,
                chart::ChartOptions {
                    hours,
                    metric,
                    width,
                    height,
                    output,
                    force_iterm: iterm,
                },
            )?;
        }
        Some(Command::Usage { action }) => match action {
            UsageAction::Capture { name } => {
                let mut cfg = config::Config::load()?;
                if !cfg.tokens.iter().any(|token| token.name == name) {
                    bail!("Token '{name}' not found");
                }
                let usage_key = read_claude_usage_key()?;
                let usage = probe::validate_usage_key(&usage_key)
                    .await
                    .map_err(anyhow::Error::msg)?;
                cfg.set_usage_key(&name, usage_key);
                cfg.save()?;
                rotation::reconcile_keychain_after_login()?;
                let buckets = usage.buckets();
                let summary = if buckets.is_empty() {
                    "no model-scoped weekly buckets reported".into()
                } else {
                    buckets
                        .iter()
                        .map(|bucket| {
                            format!(
                                "{} {:.0}% left",
                                bucket.label,
                                (1.0 - bucket.window.utilization) * 100.0
                            )
                        })
                        .collect::<Vec<_>>()
                        .join(", ")
                };
                println!("captured profile usage credential for {name}; {summary}");
            }
        },
        #[cfg(feature = "tray")]
        Some(Command::Tray) => {
            let cfg = config::Config::load()?;
            if cfg.tokens.is_empty() {
                bail!("No tokens configured. Use `tokeman add <name> <key>` to add one.");
            }
            tray::run(cfg)?;
        }
        None => {
            let cfg = config::Config::load()?;
            if cfg.tokens.is_empty() {
                println!("No tokens configured. Use `tokeman add <name> <key>` to add one.");
                return Ok(());
            }

            if cli.watch {
                tui::run(cfg).await?;
            } else {
                // One-shot probe
                let _ = admission::scan_transcripts(&cfg.tokens);
                let mut results = probe::probe_all(&cfg.tokens).await;
                admission::apply_observed_limits(&mut results);

                // Save snapshots
                let db = store::Store::open()?;
                for r in &results {
                    let _ = db.insert(r);
                }

                if cli.json {
                    println!("{}", serde_json::to_string(&results)?);
                } else {
                    display::print_results(&results);
                }
            }
        }
    }

    Ok(())
}

#[cfg(target_os = "macos")]
fn read_claude_usage_key() -> Result<String> {
    let output = std::process::Command::new("/usr/bin/security")
        .args([
            "find-generic-password",
            "-s",
            "Claude Code-credentials",
            "-w",
        ])
        .output()?;
    if !output.status.success() {
        bail!("could not read Claude's Keychain credential");
    }
    let value: serde_json::Value = serde_json::from_slice(&output.stdout)
        .context("Claude Keychain credential is not valid JSON")?;
    value
        .pointer("/claudeAiOauth/accessToken")
        .and_then(serde_json::Value::as_str)
        .filter(|token| !token.is_empty())
        .map(str::to_owned)
        .context("Claude Keychain credential has no OAuth access token")
}

#[cfg(not(target_os = "macos"))]
fn read_claude_usage_key() -> Result<String> {
    bail!("`tokeman usage capture` currently reads Claude's macOS Keychain")
}

fn mask_key(key: &str) -> String {
    let chars: Vec<_> = key.chars().collect();
    if chars.len() > 20 {
        format!(
            "{}...{}",
            chars[..16].iter().collect::<String>(),
            chars[chars.len() - 4..].iter().collect::<String>()
        )
    } else {
        "****".to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::mask_key;

    #[test]
    fn mask_key_handles_unicode_without_slicing_bytes() {
        let masked = mask_key("abcdefghijklmnop💥qrstuv");
        assert!(masked.starts_with("abcdefghijklmnop..."));
        assert!(masked.ends_with("stuv"));
    }
}
