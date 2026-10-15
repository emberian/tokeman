//! The command line, as clap sees it. No logic lives here.

use clap::{Parser, Subcommand, ValueEnum};
use std::path::PathBuf;

use crate::observe::{Family, LimitWindow};

#[derive(Parser)]
#[command(
    name = "tokeman",
    about = "Keeps Claude Code on an Anthropic account with room left"
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<Command>,
    /// Launch the live TUI dashboard
    #[arg(long)]
    pub watch: bool,
    /// Output the probe as JSON
    #[arg(long)]
    pub json: bool,
}

#[derive(Subcommand)]
pub enum Command {
    /// Add an account with a `claude setup-token` key
    Add {
        /// Account name
        name: String,
        /// The key (sk-ant-oat01-...)
        key: String,
    },
    /// Remove an account
    Remove { name: String },
    /// Grant an account the full OAuth scope set through the browser
    ///
    /// A setup token is inference-only; a login can read profile usage and
    /// refreshes itself.
    Login {
        /// Account to log in (default: the first one still without a login)
        name: Option<String>,
        /// Walk every account that still needs it, in configured order
        #[arg(long)]
        more: bool,
    },
    /// Renew login grants now instead of waiting for the daemon
    Refresh {
        /// Account to refresh (default: every account with a login)
        name: Option<String>,
    },
    /// List configured accounts and their credentials
    List,
    /// Launch claude on the best account
    Launch {
        /// Keep rotating the default while Claude runs
        #[arg(long)]
        auto: bool,
        /// Arguments passed through to claude
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Manage the default account newly started Claude Code processes use
    Rotate {
        #[command(subcommand)]
        action: RotateAction,
    },
    /// Show recent snapshots
    History {
        #[arg(long, default_value = "20")]
        last: usize,
        /// Only this account
        #[arg(long)]
        account: Option<String>,
        /// Only the last N hours
        #[arg(long)]
        since: Option<f64>,
        #[arg(long)]
        json: bool,
    },
    /// Burn rates and time to depletion
    Stats,
    /// Render a history chart (inline in iTerm2)
    Chart {
        /// Hours of history to show
        #[arg(long, default_value = "24")]
        hours: f64,
        /// five-hour, seven-day, overage, or a model's weekly bucket by name
        /// (fable, opus-5, "Opus 4.8")
        #[arg(long, default_value = "five-hour")]
        metric: String,
        /// Also save the PNG here
        #[arg(long)]
        output: Option<PathBuf>,
        /// Force iTerm2's inline-image escape sequence
        #[arg(long)]
        iterm: bool,
        #[arg(long, default_value = "1400")]
        width: u32,
        #[arg(long, default_value = "680")]
        height: u32,
    },
    /// Show or use usage-limit resets (needs a logged-in account)
    Resets {
        #[command(subcommand)]
        action: Option<ResetAction>,
    },
    /// Show or change the launch settings in tokens.toml
    Settings {
        #[command(subcommand)]
        action: Option<SettingsAction>,
    },
    /// Manage Codex (OpenAI) accounts and migrate running Codex sessions
    Codex {
        #[command(subcommand)]
        action: CodexAction,
    },
    /// Run the tray application
    #[cfg(feature = "tray")]
    Tray,
}

#[derive(Subcommand)]
pub enum RotateAction {
    /// Probe and apply the policy once
    Run {
        /// Take the best account even over a healthy default
        #[arg(long)]
        force: bool,
        /// Decide without writing anything
        #[arg(long)]
        dry_run: bool,
        /// Honour the adaptive cadence (the service passes this)
        #[arg(long, hide = true)]
        scheduled: bool,
    },
    /// The persistent monitor (the service runs this)
    #[command(hide = true)]
    Daemon,
    /// Probe and report the monitor, the default, live sessions and the mode
    Status {
        #[arg(long)]
        json: bool,
    },
    /// Stop managing the credential; /login gets its precedence back
    Pause,
    /// Resume managing and run one cycle now
    Resume,
    /// Make this account the default now
    Use { name: String },
    /// Install and start the per-user background service
    Install,
    /// Stop and remove the background service
    Uninstall,
    /// Record a capacity rejection tokeman did not observe itself
    Quarantine {
        name: String,
        /// Model family the rejection named
        #[arg(long, value_enum)]
        model: ModelArg,
        /// Concrete model, e.g. claude-opus-4-8
        #[arg(long)]
        exact_model: Option<String>,
        /// Window that rejected the request
        #[arg(long, value_enum)]
        window: WindowArg,
        /// Reset as RFC 3339 or a Unix timestamp
        #[arg(long)]
        until: String,
    },
    /// Remove observed and manual quarantines for an account
    ClearQuarantine {
        name: String,
        /// Only this model family
        #[arg(long, value_enum)]
        model: Option<ModelArg>,
    },
    /// The SessionStart hook: record which account a new Claude process spends
    #[command(hide = true)]
    ObserveSession {
        /// The Claude process id, supplied by the hook as $PPID
        #[arg(long)]
        pid: u32,
    },
}

#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum ModelArg {
    Opus,
    Sonnet,
    Fable,
    General,
}

impl ModelArg {
    pub fn family(self) -> Family {
        match self {
            Self::Opus => Family::Opus,
            Self::Sonnet => Family::Sonnet,
            Self::Fable => Family::Fable,
            Self::General => Family::General,
        }
    }
}

#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum WindowArg {
    FiveHour,
    Weekly,
}

impl From<WindowArg> for LimitWindow {
    fn from(value: WindowArg) -> Self {
        match value {
            WindowArg::FiveHour => Self::Session,
            WindowArg::Weekly => Self::Weekly,
        }
    }
}

#[derive(Subcommand)]
pub enum ResetAction {
    /// List each logged-in account's grants and session reset
    List { name: Option<String> },
    /// Use a reset on one account (asks first)
    Use {
        name: String,
        /// Grant to use (default: the one the server offers next)
        #[arg(long)]
        grant: Option<String>,
        /// Use the session reset instead of a grant
        #[arg(long, conflicts_with = "grant")]
        session: bool,
        /// Do not ask
        #[arg(long)]
        yes: bool,
    },
}

#[derive(Subcommand)]
pub enum SettingsAction {
    /// Print every launch setting
    Show,
    /// Set one (launch_args, dangerous_mode, terminal, claude_bin,
    /// probe_interval_secs); an empty value clears it
    Set {
        key: String,
        #[arg(default_value = "", allow_hyphen_values = true)]
        value: String,
    },
}

#[derive(Subcommand)]
pub enum CodexAction {
    /// Register a Codex profile directory (a CODEX_HOME)
    Add {
        name: String,
        /// Used as CODEX_HOME, e.g. ~/.codex-homes/work
        codex_home: String,
        #[arg(long)]
        label: Option<String>,
    },
    /// Unregister a profile
    Remove { name: String },
    /// Find profiles on disk and optionally register them
    Discover {
        /// Register everything found
        #[arg(long)]
        adopt: bool,
    },
    /// Seed profiles from a file of `<name> <access-token>` lines
    Import {
        from: PathBuf,
        /// Directory to create profiles under (default: ~/.codex-homes)
        #[arg(long)]
        homes_root: Option<String>,
        /// Do not register the imported accounts
        #[arg(long)]
        no_register: bool,
    },
    /// Probe every account's quota
    List {
        #[arg(long)]
        json: bool,
    },
    /// Refresh OAuth tokens before they rot into a 401
    Refresh {
        /// Only this account
        name: Option<String>,
        /// Even if the stored bundle is fresh
        #[arg(long)]
        force: bool,
        /// Refresh when last_refresh is older than this many days
        #[arg(long, default_value_t = crate::codex::refresh::DEFAULT_MAX_AGE_DAYS)]
        max_age_days: i64,
    },
    /// Copy an account's credentials into another profile directory
    Seat {
        account: String,
        /// Target CODEX_HOME
        #[arg(long)]
        into: String,
    },
    /// Move a running Codex onto another account without restarting it
    Reseat {
        /// CODEX_HOME whose app-server to migrate (default: ~/.codex)
        #[arg(long)]
        codex_home: Option<String>,
        /// Account to move to (default: the best viable)
        #[arg(long)]
        account: Option<String>,
        /// Report the decision without contacting the app-server
        #[arg(long)]
        dry_run: bool,
        /// Even if the named account is rate limited
        #[arg(long)]
        force: bool,
    },
    /// Follow a running Codex and move it before its account runs out
    Watch {
        /// CODEX_HOME to watch (default: ~/.codex)
        #[arg(long)]
        codex_home: Option<String>,
        /// Move once remaining headroom drops below this fraction
        #[arg(long, default_value = "0.20")]
        min_remaining: f64,
        /// Evaluate once and exit
        #[arg(long)]
        once: bool,
    },
    /// Print the CODEX_HOME of the account with the most headroom
    Best {
        /// Rank by a per-model bucket (e.g. "spark") instead of the account window
        #[arg(long)]
        limit: Option<String>,
    },
}
