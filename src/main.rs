mod config;
mod display;
mod launch;
mod probe;
mod stats;
mod store;
#[cfg(feature = "tray")]
mod terminal;
#[cfg(feature = "tray")]
mod tray;
mod tui;

use anyhow::{bail, Result};
use chrono::{Duration, Utc};
use clap::{Parser, Subcommand};

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
        /// Auto-relaunch on token exhaustion (no prompt)
        #[arg(long)]
        auto: bool,
        /// Arguments to pass to claude
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Run as a system tray application
    #[cfg(feature = "tray")]
    Tray,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Some(Command::Add { name, key }) => {
            let mut cfg = config::Config::load()?;
            cfg.add_token(name.clone(), key);
            cfg.save()?;
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
                    println!("  {} — {}", t.name, masked);
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
                let cutoff =
                    Utc::now() - Duration::milliseconds((hours * 3_600_000.0) as i64);
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
                let results = probe::probe_all(&cfg.tokens).await;

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

fn mask_key(key: &str) -> String {
    if key.len() > 20 {
        format!("{}...{}", &key[..16], &key[key.len() - 4..])
    } else {
        "****".to_string()
    }
}
