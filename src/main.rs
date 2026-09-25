// The rotation service (launchd) and Claude login handling (Keychain) are
// macOS-only, so much of that layer is unreachable elsewhere. Dead code is
// still reported on macOS, where the whole crate is live.
#![cfg_attr(not(target_os = "macos"), allow(dead_code))]

mod admission;
mod chart;
mod claude_login;
mod config;
mod credential_history;
mod display;
mod launch;
mod openai;
mod private_fs;
mod probe;
mod resets;
mod rotation;
mod stats;
mod store;
#[cfg(feature = "tray")]
mod terminal;
mod text;
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
        /// Window to chart: five-hour, seven-day, overage, or a model's
        /// weekly bucket by name (fable, opus-5, "Opus 4.8", ...)
        #[arg(long, default_value = "five-hour")]
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
    /// Manage Codex (OpenAI) accounts and migrate running Codex sessions
    Codex {
        #[command(subcommand)]
        action: CodexAction,
    },
    /// Grant a configured account the full OAuth scope set via the browser
    ///
    /// A `claude setup-token` credential is inference-only, so tokeman has to
    /// spend quota to measure quota. A login credential carries `user:profile`
    /// and can read `/api/oauth/usage` for free.
    Login {
        /// Account to log in (defaults to the first one still missing scope)
        name: Option<String>,
        /// Walk every account that still needs it, in configured order
        #[arg(long)]
        more: bool,
    },
    /// Show or use usage-limit resets (needs a `tokeman login` account)
    Resets {
        #[command(subcommand)]
        action: Option<ResetAction>,
    },
    /// Renew login grants now instead of waiting for the daemon
    Refresh {
        /// Account to refresh (default: every account with a login)
        name: Option<String>,
    },
    /// Run as a system tray application
    #[cfg(feature = "tray")]
    Tray,
}

#[derive(Subcommand)]
enum ResetAction {
    /// List each logged-in account's reset grants and session reset
    List {
        /// Limit to one account
        name: Option<String>,
    },
    /// Use a reset on one account (asks before claiming)
    Use {
        /// Configured account name
        name: String,
        /// Grant to use (default: the one the server offers next)
        #[arg(long)]
        grant: Option<String>,
        /// Use the session (5-hour) reset instead of a grant
        #[arg(long, conflicts_with = "grant")]
        session: bool,
        /// Do not ask for confirmation
        #[arg(long)]
        yes: bool,
    },
}

#[derive(Subcommand)]
enum CodexAction {
    /// Register a Codex profile directory (a CODEX_HOME)
    Add {
        /// Display name for the account
        name: String,
        /// Path used as CODEX_HOME, e.g. ~/.codex-homes/espark
        codex_home: String,
        /// Optional human label
        #[arg(long)]
        label: Option<String>,
    },
    /// Unregister a Codex profile
    Remove {
        /// Configured account name
        name: String,
    },
    /// Find Codex profiles on disk and optionally register them
    Discover {
        /// Register everything discovered
        #[arg(long)]
        adopt: bool,
    },
    /// Seed profiles from a file of `<name> <access-token>` lines
    Import {
        /// File with one `<name> <access-token-jwt>` per line
        from: std::path::PathBuf,
        /// Directory to create profiles under (default: ~/.codex-homes)
        #[arg(long)]
        homes_root: Option<String>,
        /// Do not register the imported accounts in tokens.toml
        #[arg(long)]
        no_register: bool,
    },
    /// Probe every configured account's quota
    List {
        /// Output machine-readable JSON
        #[arg(long)]
        json: bool,
    },
    /// Refresh OAuth tokens before they rot into a 401
    Refresh {
        /// Limit to one account (default: all)
        name: Option<String>,
        /// Refresh even if the stored bundle is still fresh
        #[arg(long)]
        force: bool,
        /// Refresh when last_refresh is older than this many days
        #[arg(long, default_value_t = openai::refresh::DEFAULT_MAX_AGE_DAYS)]
        max_age_days: i64,
    },
    /// Copy an account's credentials into another profile directory
    Seat {
        /// Configured account to seat
        account: String,
        /// Target CODEX_HOME to write into
        #[arg(long)]
        into: String,
    },
    /// Migrate a running Codex onto another account without restarting it
    Reseat {
        /// CODEX_HOME whose app-server should be migrated (default: ~/.codex)
        #[arg(long)]
        codex_home: Option<String>,
        /// Account to move to (default: best viable)
        #[arg(long)]
        account: Option<String>,
        /// Report the decision without contacting the app-server
        #[arg(long)]
        dry_run: bool,
        /// Migrate onto the named account even if it is rate limited
        #[arg(long)]
        force: bool,
    },
    /// Follow a running Codex and migrate it before its account runs out
    Watch {
        /// CODEX_HOME to watch (default: ~/.codex)
        #[arg(long)]
        codex_home: Option<String>,
        /// Migrate once remaining headroom drops below this fraction
        #[arg(long, default_value = "0.20")]
        min_remaining: f64,
        /// Evaluate once and exit instead of following notifications
        #[arg(long)]
        once: bool,
    },
    /// Print the CODEX_HOME of the account with the most headroom
    Best {
        /// Rank by a per-model bucket instead (e.g. "spark"), which is metered
        /// separately from the account-wide window
        #[arg(long)]
        limit: Option<String>,
    },
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
            config::Config::update(|cfg| {
                cfg.add_token(name.clone(), key);
                Ok(())
            })?;
            rotation::reconcile_keychain_after_login()?;
            println!("Added token '{name}'");
        }
        Some(Command::Remove { name }) => {
            let (_, removed) = config::Config::update(|cfg| Ok(cfg.remove_token(&name)))?;
            if !removed {
                bail!("Token '{name}' not found");
            }
            println!("Removed token '{name}'");
        }
        Some(Command::List) => {
            let cfg = config::Config::load()?;
            if cfg.tokens.is_empty() {
                println!("No tokens configured. Use `tokeman add <name> <key>` to add one.");
            } else {
                let now_ms = Utc::now().timestamp_millis();
                for t in &cfg.tokens {
                    let setup = t
                        .setup_key()
                        .map(mask_key)
                        .unwrap_or_else(|| "no setup token".into());
                    println!("  {} — {} — {}", t.name, setup, describe_login(t, now_ms));
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
        Some(Command::Login { name, more }) => {
            login_accounts(name, more).await?;
        }
        Some(Command::Resets { action }) => {
            match action.unwrap_or(ResetAction::List { name: None }) {
                ResetAction::List { name } => list_resets(name.as_deref()).await?,
                ResetAction::Use {
                    name,
                    grant,
                    session,
                    yes,
                } => use_reset(&name, grant, session, yes).await?,
            }
        }
        Some(Command::Refresh { name }) => {
            let refreshed = rotation::force_refresh(name.as_deref()).await?;
            let now_ms = Utc::now().timestamp_millis();
            for token in config::Config::load()?
                .tokens
                .iter()
                .filter(|token| name.as_ref().is_none_or(|name| &token.name == name))
                .filter(|token| token.refresh_token.is_some())
            {
                println!("  {} — {}", token.name, describe_login(token, now_ms));
            }
            println!("{refreshed} grant(s) refreshed.");
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
                if !config::Config::load()?
                    .tokens
                    .iter()
                    .any(|token| token.name == name)
                {
                    bail!("Token '{name}' not found");
                }
                let usage_key = read_claude_usage_key()?;
                let usage = probe::validate_usage_key(&usage_key)
                    .await
                    .map_err(anyhow::Error::msg)?;
                config::Config::update(|cfg| Ok(cfg.set_usage_key(&name, usage_key)))?;
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
        Some(Command::Codex { action }) => match action {
            CodexAction::Add {
                name,
                codex_home,
                label,
            } => openai::cli::add(name, codex_home, label)?,
            CodexAction::Remove { name } => openai::cli::remove(&name)?,
            CodexAction::Discover { adopt } => openai::cli::discover(adopt)?,
            CodexAction::Import {
                from,
                homes_root,
                no_register,
            } => openai::cli::import(from, homes_root, !no_register).await?,
            CodexAction::List { json } => openai::cli::list(json || cli.json).await?,
            CodexAction::Refresh {
                name,
                force,
                max_age_days,
            } => openai::cli::refresh_accounts(name, force, max_age_days).await?,
            CodexAction::Seat { account, into } => openai::cli::seat(&account, &into).await?,
            CodexAction::Reseat {
                codex_home,
                account,
                dry_run,
                force,
            } => openai::cli::reseat(codex_home, account, dry_run, force).await?,
            CodexAction::Watch {
                codex_home,
                min_remaining,
                once,
            } => openai::cli::watch(codex_home, min_remaining, once).await?,
            CodexAction::Best { limit } => openai::cli::best_home(limit).await?,
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

/// One-line summary of an account's login grant, without secrets.
fn describe_login(token: &config::Token, now_ms: i64) -> String {
    if let Some(error) = &token.login_error {
        return format!(
            "login refused ({error}); run `tokeman login {}`",
            token.name
        );
    }
    if !token.is_refreshable() {
        return match token.access_token {
            Some(_) => "captured usage token (not refreshable)".into(),
            None => "no login (inference-only)".into(),
        };
    }
    match token.expires_at {
        Some(expires_at) if expires_at > now_ms => format!(
            "login, full scope, access token valid {}m",
            (expires_at - now_ms) / 60_000
        ),
        Some(_) => "login, access token expired (refresh pending)".into(),
        None => "login, expiry unknown".into(),
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

/// Drive the browser login flow for one account, or for every account that
/// still lacks a refreshable login.
///
/// `--more` exists so the accounts come from the config in order rather than
/// being typed out one email at a time. Each round is: open a URL, paste what
/// the page shows, move on. `skip` passes on an account, `q` stops. Naming an
/// account that is not configured yet enrolls it without a setup token.
async fn login_accounts(name: Option<String>, more: bool) -> anyhow::Result<()> {
    use std::io::{BufRead, Write};

    let cfg = config::Config::load()?;
    let needs_login = |token: &&config::Token| !token.is_refreshable();
    let targets: Vec<String> = match (&name, more) {
        (Some(name), _) => vec![name.clone()],
        (None, true) => cfg
            .tokens
            .iter()
            .filter(needs_login)
            .map(|token| token.name.clone())
            .collect(),
        (None, false) => cfg
            .tokens
            .iter()
            .find(needs_login)
            .map(|token| vec![token.name.clone()])
            .unwrap_or_default(),
    };
    if targets.is_empty() {
        println!("every configured account already has a refreshable login.");
        return Ok(());
    }

    let total = targets.len();
    let stdin = std::io::stdin();
    let mut lines = stdin.lock().lines();
    let mut done = 0usize;
    for (index, account) in targets.iter().enumerate() {
        // Account names are usually the account's email; hint only then.
        let pkce = claude_login::begin(account.contains('@').then_some(account.as_str()))?;
        println!();
        println!("[{}/{}] {account}", index + 1, total);
        if !cfg.tokens.iter().any(|token| &token.name == account) {
            println!("  (not configured yet: it will be added as a login-only account)");
        }
        println!("  sign in as this account, then paste what the page shows:");
        println!();
        println!("  {}", pkce.url);
        println!();
        print!("  code (or `skip` / `q`): ");
        std::io::stdout().flush().ok();
        let Some(line) = lines.next() else { break };
        let line = line?;
        let answer = line.trim();
        if answer.eq_ignore_ascii_case("q") || answer.eq_ignore_ascii_case("quit") {
            break;
        }
        if answer.is_empty() || answer.eq_ignore_ascii_case("skip") {
            println!("  skipped {account}");
            continue;
        }
        match claude_login::exchange(answer, &pkce).await {
            Ok(bundle) => {
                if bundle.refresh_token.is_none() {
                    eprintln!(
                        "  {account}: the grant came back without a refresh token; not stored"
                    );
                    continue;
                }
                if !bundle.has_profile_scope() {
                    eprintln!(
                        "  {account}: granted scopes are {:?} -- no user:profile, so /feedback and usage reads will still be refused",
                        bundle.scopes
                    );
                }
                let scopes = bundle.scopes.join(" ");
                // Save after each account: a failure on number five must not
                // discard the four logins already completed.
                config::Config::update(|cfg| {
                    cfg.set_login_bundle(account, bundle, Utc::now().timestamp_millis());
                    Ok(())
                })?;
                done += 1;
                println!("  {account}: stored ({scopes})");
            }
            Err(error) => eprintln!("  {account}: {error:#}"),
        }
    }
    println!();
    println!("{done}/{total} account(s) updated.");
    if done > 0 {
        println!(
            "the rotation service installs these on its next cycle; `tokeman rotate status` shows which credential each account uses."
        );
    }
    Ok(())
}

/// Every logged-in account's reset offers. Accounts without a profile
/// credential are listed as such rather than skipped silently.
async fn list_resets(only: Option<&str>) -> anyhow::Result<()> {
    let cfg = config::Config::load()?;
    let now_ms = Utc::now().timestamp_millis();
    for token in cfg
        .tokens
        .iter()
        .filter(|token| only.is_none_or(|name| token.name == name))
    {
        if token.usage_credential(now_ms).is_none() {
            if only.is_some() {
                println!(
                    "{}: no login; run `tokeman login {}`",
                    token.name, token.name
                );
            }
            continue;
        }
        println!("{}", token.name);
        match resets::status(token).await {
            Ok(status) => print_reset_status(&status),
            Err(error) => println!("  unavailable: {error:#}"),
        }
    }
    Ok(())
}

fn print_reset_status(status: &resets::ResetStatus) {
    match &status.cedar_ember {
        Some(program) if program.eligible => {
            let grants = program.grants();
            if grants.is_empty() {
                println!("  limit resets: none on offer");
            }
            for grant in grants {
                let next = program.next_grant_id.as_deref() == Some(grant.id.as_str());
                println!(
                    "  {} {}",
                    if next { "*" } else { " " },
                    resets::describe_grant(&grant)
                );
            }
            if let Some(until) = &program.cooldown_until {
                println!("    cooling down until {until}");
            }
        }
        Some(program) => println!(
            "  limit resets: not eligible ({})",
            program
                .ineligible_reason
                .as_deref()
                .unwrap_or("no reason given")
        ),
        None => println!("  limit resets: not offered"),
    }
    match &status.juniper_tide {
        Some(session) if session.available => println!(
            "  session reset: available ({} per week)",
            session.resets_per_week.unwrap_or(1)
        ),
        Some(session) => println!(
            "  session reset: not available ({}{})",
            session
                .ineligible_reason
                .as_deref()
                .unwrap_or("not offered"),
            session
                .next_available_at
                .as_deref()
                .map(|at| format!("; next {at}"))
                .unwrap_or_default()
        ),
        None => println!("  session reset: not offered"),
    }
}

/// Claim a reset after showing what it would clear. Resets are scarce and
/// cannot be taken back, so this asks unless `--yes` is given.
async fn use_reset(
    name: &str,
    grant: Option<String>,
    session: bool,
    yes: bool,
) -> anyhow::Result<()> {
    use std::io::{BufRead, Write};

    let cfg = config::Config::load()?;
    let token = cfg
        .tokens
        .iter()
        .find(|token| token.name == name)
        .with_context(|| format!("no configured account named {name}"))?;
    let status = resets::status(token).await?;
    let grant_id = if session {
        let session = status
            .juniper_tide
            .as_ref()
            .context("the session reset is not offered to this account")?;
        if !session.available {
            bail!(
                "the session reset is not available ({})",
                session
                    .ineligible_reason
                    .as_deref()
                    .unwrap_or("not offered")
            );
        }
        println!("{name}: session reset (refills the 5-hour limit, paid from the weekly limit)");
        None
    } else {
        let program = status
            .cedar_ember
            .as_ref()
            .context("limit resets are not offered to this account")?;
        if !program.eligible {
            bail!(
                "not eligible for limit resets ({})",
                program
                    .ineligible_reason
                    .as_deref()
                    .unwrap_or("no reason given")
            );
        }
        let wanted = grant.or_else(|| program.next_grant_id.clone());
        let chosen = program
            .grants()
            .into_iter()
            .find(|candidate| Some(&candidate.id) == wanted.as_ref())
            .context("no such grant on offer; see `tokeman resets list`")?;
        if !chosen.usable_now {
            bail!(
                "that grant is not usable right now{}",
                if chosen.use_requires_limit {
                    " (it only works at a usage limit)"
                } else {
                    ""
                }
            );
        }
        println!("{name}: {}", resets::describe_grant(&chosen));
        Some(chosen.id)
    };

    if !yes {
        print!("use it now? [y/N] ");
        std::io::stdout().flush().ok();
        let mut answer = String::new();
        std::io::stdin().lock().read_line(&mut answer)?;
        if !answer.trim().eq_ignore_ascii_case("y") {
            println!("kept.");
            return Ok(());
        }
    }
    let outcome = resets::claim(token, grant_id.as_deref()).await?;
    let cleared = outcome
        .cleared
        .iter()
        .map(|window| resets::window_label(window))
        .collect::<Vec<_>>()
        .join(" ");
    match outcome.result.as_str() {
        "reset" => println!(
            "reset: cleared {}{}",
            if cleared.is_empty() { "-" } else { &cleared },
            outcome
                .resets_left
                .map(|left| format!("; {left} left"))
                .unwrap_or_default()
        ),
        other => println!(
            "not reset: {other}{}",
            outcome
                .reason
                .as_deref()
                .map(|reason| format!(" ({reason})"))
                .unwrap_or_default()
        ),
    }
    if let Some(weekly) = &outcome.weekly_resets_at {
        println!("your weekly reset day stays {weekly}");
    }
    if let Some(until) = &outcome.cooldown_until {
        println!("cooling down until {until}");
    }
    Ok(())
}
