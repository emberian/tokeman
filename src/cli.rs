//! The command line, as clap sees it: definitions only. The one thing here
//! that is not a definition is [`REMOVED`], the table of spellings 0.3 had.

use clap::{Parser, Subcommand, ValueEnum};
use std::path::PathBuf;

use crate::observe::{Family, LimitWindow};
use crate::quota::Service;

#[derive(Parser)]
#[command(
    name = "tokeman",
    about = "Keeps Claude Code, Codex and agy on a subscription account with room left, and shows the rest"
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<Command>,
    /// Launch the live TUI dashboard
    #[arg(long)]
    pub watch: bool,
    /// Read every account now. Without it, while the background service
    /// runs, the stored view is shown and no request is sent
    #[arg(long)]
    pub fresh: bool,
    /// Output the fleet view as JSON (the shape of `tokeman list --json`)
    #[arg(long)]
    pub json: bool,
}

#[derive(Subcommand)]
pub enum Command {
    /// Everything stored about every account of every service
    ///
    /// Makes no request and writes nothing.
    List {
        /// The fleet view as JSON (what the trays read)
        #[arg(long)]
        json: bool,
    },
    /// Anthropic accounts: the ones Claude Code is rotated across
    Claude {
        #[command(subcommand)]
        action: ClaudeAction,
    },
    /// Codex (OpenAI) profiles, and moving a running Codex between them
    Codex {
        #[command(subcommand)]
        action: CodexAction,
    },
    /// Google Antigravity accounts, and the one the agy CLI signs in as
    #[command(alias = "antigravity")]
    Agy {
        #[command(subcommand)]
        action: AgyAction,
    },
    /// z.ai GLM Coding Plan accounts (read-only quota)
    Glm {
        #[command(subcommand)]
        action: KeyAction,
    },
    /// Kimi Code accounts (read-only quota)
    Kimi {
        #[command(subcommand)]
        action: KeyAction,
    },
    /// Launch claude on the best account
    Launch {
        /// Keep rotating the default while Claude runs
        #[arg(long)]
        auto: bool,
        /// Arguments passed through to claude
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// The rotation of Claude Code's default account, and its background service
    Rotate {
        #[command(subcommand)]
        action: RotateAction,
    },
    /// Show recent stored reads of one service
    History {
        /// Which service's reads
        #[arg(long, value_enum, default_value = "claude")]
        service: ServiceArg,
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
    /// Burn rates and time to depletion, per limit
    Stats {
        /// Which service's accounts
        #[arg(long, value_enum, default_value = "claude")]
        service: ServiceArg,
    },
    /// Render a history chart of one limit (inline in iTerm2)
    Chart {
        /// Which service's accounts
        #[arg(long, value_enum, default_value = "claude")]
        service: ServiceArg,
        /// Hours of history to show
        #[arg(long, default_value = "24")]
        hours: f64,
        /// The limit to draw: a window by length (5h, weekly, monthly,
        /// daily), `overage`, or a limit by its model or group (fable,
        /// "Opus 4.8", gemini, "gemini 5h", spark). Default: the service's
        /// first account-wide window
        #[arg(long)]
        metric: Option<String>,
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
    /// Show or change the launch settings in tokens.toml
    Settings {
        #[command(subcommand)]
        action: Option<SettingsAction>,
    },
    /// Run the tray application
    #[cfg(feature = "tray")]
    Tray,
}

#[derive(Subcommand)]
pub enum ClaudeAction {
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
    /// Probe every account now and print the gauges
    List {
        #[arg(long)]
        json: bool,
    },
    /// Make this account the default now
    Use { name: String },
    /// Probe every account and rank them as the rotation would land
    Best,
    /// Usage-limit resets: list what is on offer, or use one on an account
    /// (asks first; needs a logged-in account)
    Reset {
        /// The account to reset (default: list every account's offers)
        name: Option<String>,
        /// Grant to use (default: the one the server offers next)
        #[arg(long, requires = "name")]
        grant: Option<String>,
        /// Use the session reset instead of a grant
        #[arg(long, requires = "name", conflicts_with = "grant")]
        session: bool,
        /// Do not ask
        #[arg(long, requires = "name")]
        yes: bool,
        /// Forget a grant claim whose outcome is unknown, once Settings >
        /// Usage on claude.ai shows what happened
        #[arg(long, requires = "name", conflicts_with_all = ["grant", "session", "yes"])]
        clear: bool,
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
    /// Probe every account's quota
    List {
        #[arg(long)]
        json: bool,
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
    /// Move a running Codex onto an account without restarting it, or (with
    /// --into) copy the account's credentials into another profile directory
    Use {
        /// Account to move to (default: the best viable)
        account: Option<String>,
        /// Copy the credentials into this CODEX_HOME instead of moving a
        /// running Codex
        #[arg(long, requires = "account", conflicts_with_all = ["codex_home", "dry_run", "force"])]
        into: Option<String>,
        /// CODEX_HOME whose app-server to migrate (default: ~/.codex)
        #[arg(long)]
        codex_home: Option<String>,
        /// Report the decision without contacting the app-server
        #[arg(long)]
        dry_run: bool,
        /// Even if the named account is rate limited
        #[arg(long)]
        force: bool,
    },
    /// Print the CODEX_HOME of the account with the most headroom
    Best {
        /// Rank by a per-model limit (e.g. "spark") instead of the account windows
        #[arg(long)]
        limit: Option<String>,
    },
    /// Spend one rate-limit reset credit on an account at its limit (asks first)
    Reset {
        account: String,
        /// Credit to spend (default: the backend's choice); see `tokeman codex list`
        #[arg(long)]
        credit: Option<String>,
        /// Do not ask
        #[arg(long)]
        yes: bool,
        /// Forget a credit claim whose outcome is unknown, once the account's
        /// usage page shows what happened
        #[arg(long, conflicts_with_all = ["credit", "yes"])]
        clear: bool,
    },
}

#[derive(Subcommand)]
pub enum AgyAction {
    /// Register the accounts agy and the Antigravity app are signed in to
    /// on this machine
    Import,
    /// Unregister an account
    Remove { name: String },
    /// Read every account now and print the gauges
    List {
        #[arg(long)]
        json: bool,
    },
    /// Sign the agy CLI in as an account (macOS; manual, the daemon never
    /// switches it)
    ///
    /// An open Antigravity app is told to follow. An interactive agy cannot
    /// be told: it keeps its account and writes it back when it refreshes
    /// its token, so the switch is refused while one runs unless --force.
    Use {
        /// The account to seat
        #[arg(required_unless_present = "previous")]
        name: Option<String>,
        /// Put back the login the last `use` replaced
        #[arg(long, conflicts_with = "name")]
        previous: bool,
        /// Switch even while an interactive agy runs; it undoes the switch
        /// at its next token refresh
        #[arg(long)]
        force: bool,
        /// Say what is running and what would happen; write nothing
        #[arg(long)]
        dry_run: bool,
    },
    /// Read every account now and rank them by room left
    Best {
        /// Count only the limits whose group contains this text, e.g. "claude"
        #[arg(long)]
        group: Option<String>,
    },
}

/// The verbs of a service whose accounts are an API key.
#[derive(Subcommand)]
pub enum KeyAction {
    /// Register an account
    Add {
        /// Unique across services
        name: String,
        /// File holding the API key, e.g. ~/.zai-key; read on every poll
        #[arg(long)]
        key_file: PathBuf,
        #[arg(long)]
        label: Option<String>,
    },
    /// Unregister an account
    Remove { name: String },
    /// Read every account now and print the gauges
    List {
        #[arg(long)]
        json: bool,
    },
    /// Read every account now and rank them by room left
    Best {
        /// Count only the limits whose group contains this text
        #[arg(long)]
        group: Option<String>,
    },
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

/// A service, as `--service` names it: the words that select it as a command.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum ServiceArg {
    Claude,
    Codex,
    #[value(alias = "antigravity")]
    Agy,
    #[value(alias = "zai")]
    Glm,
    Kimi,
}

impl From<ServiceArg> for Service {
    fn from(value: ServiceArg) -> Self {
        match value {
            ServiceArg::Claude => Self::Claude,
            ServiceArg::Codex => Self::Codex,
            ServiceArg::Agy => Self::Antigravity,
            ServiceArg::Glm => Self::Zai,
            ServiceArg::Kimi => Self::Kimi,
        }
    }
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

/// A spelling 0.3 had and 0.4 does not: the words after `tokeman`, and what
/// to run instead. Nothing here runs anything; `main` refuses the invocation
/// with the replacement. Longer spellings come first.
pub const REMOVED: &[(&[&str], &str)] = &[
    (&["rotate", "use"], "tokeman claude use <account>"),
    (&["resets", "list"], "tokeman claude reset"),
    (
        &["resets", "use"],
        "tokeman claude reset <account> [--grant <id> | --session] [--yes]",
    ),
    (
        &["resets", "clear"],
        "tokeman claude reset <account> --clear",
    ),
    (&["resets"], "tokeman claude reset [<account>]"),
    (
        &["codex", "seat"],
        "tokeman codex use <account> --into <codex-home>",
    ),
    (
        &["codex", "reseat"],
        "tokeman codex use [<account>] [--codex-home <dir>] [--dry-run] [--force]",
    ),
    (
        &["services", "list"],
        "tokeman agy list, tokeman glm list or tokeman kimi list (tokeman list shows every service from stored reads)",
    ),
    (
        &["services", "add"],
        "tokeman glm add <name> --key-file <path> or tokeman kimi add <name> --key-file <path>",
    ),
    (
        &["services", "remove"],
        "tokeman agy remove <name>, tokeman glm remove <name> or tokeman kimi remove <name>",
    ),
    (&["services", "import"], "tokeman agy import"),
    (
        &["services", "use"],
        "tokeman agy use <account> [--previous]",
    ),
    (
        &["services", "best"],
        "tokeman agy best [--group <text>], tokeman glm best or tokeman kimi best",
    ),
    (
        &["services"],
        "tokeman agy list, tokeman glm list or tokeman kimi list (tokeman list shows every service from stored reads)",
    ),
    (&["login"], "tokeman claude login [<account>] [--more]"),
    (&["add"], "tokeman claude add <name> <key>"),
    (&["remove"], "tokeman claude remove <name>"),
    (&["refresh"], "tokeman claude refresh [<account>]"),
];

/// The replacement for a removed spelling, when `args` (the words after the
/// program name) start with one. Leading flags are skipped, so
/// `tokeman --json services` is caught too.
pub fn removed(args: &[String]) -> Option<(String, &'static str)> {
    let words: Vec<&str> = args
        .iter()
        .map(String::as_str)
        .skip_while(|arg| arg.starts_with('-'))
        .collect();
    REMOVED
        .iter()
        .find(|(old, _)| words.len() >= old.len() && words[..old.len()] == **old)
        .map(|(old, new)| (format!("tokeman {}", old.join(" ")), *new))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(line: &str) -> Result<Cli, clap::Error> {
        Cli::try_parse_from(line.split_whitespace())
    }

    #[test]
    fn the_service_first_tree_parses() {
        for line in [
            "tokeman",
            "tokeman --watch",
            "tokeman --json",
            "tokeman list --json",
            "tokeman claude add work sk-ant-oat01-x",
            "tokeman claude remove work",
            "tokeman claude login",
            "tokeman claude login work --more",
            "tokeman claude refresh work",
            "tokeman claude list --json",
            "tokeman claude use work",
            "tokeman claude best",
            "tokeman claude reset",
            "tokeman claude reset work --grant g1 --yes",
            "tokeman claude reset work --session",
            "tokeman claude reset work --clear",
            "tokeman codex add work ~/.codex-homes/work --label x",
            "tokeman codex remove work",
            "tokeman codex discover --adopt",
            "tokeman codex import tokens.txt --no-register",
            "tokeman codex refresh work --force",
            "tokeman codex list --json",
            "tokeman codex watch --once",
            "tokeman codex use",
            "tokeman codex use work --dry-run --force",
            "tokeman codex use work --into ~/.codex-homes/other",
            "tokeman codex best --limit spark",
            "tokeman codex reset work --credit c1 --yes",
            "tokeman agy import",
            "tokeman antigravity import",
            "tokeman agy remove a@example.com",
            "tokeman agy list --json",
            "tokeman agy use a@example.com --dry-run",
            "tokeman agy use --previous --force",
            "tokeman agy best --group claude",
            "tokeman glm add glm --key-file ~/.zai-key --label x",
            "tokeman glm remove glm",
            "tokeman glm list",
            "tokeman kimi add kimi --key-file ~/.kimi-key",
            "tokeman kimi list --json",
            "tokeman kimi best",
            "tokeman launch --auto -- --model fable",
            "tokeman rotate run --dry-run",
            "tokeman rotate status --json",
            "tokeman rotate pause",
            "tokeman rotate resume",
            "tokeman rotate install",
            "tokeman rotate uninstall",
            "tokeman rotate daemon",
            "tokeman rotate quarantine work --model fable --window weekly --until 1900000000",
            "tokeman rotate clear-quarantine work --model fable",
            "tokeman history --last 5 --json",
            "tokeman stats",
            "tokeman chart --metric fable --hours 12",
            "tokeman settings",
            "tokeman settings show",
            "tokeman settings set launch_args --model",
        ] {
            assert!(
                parse(line).is_ok(),
                "{line}: {:?}",
                parse(line).err().map(|e| e.to_string())
            );
        }
    }

    #[test]
    fn what_an_installed_0_3_service_and_hook_invoke_still_parses() {
        // The launcher script 0.3 wrote ends in `exec "$binary" rotate daemon`.
        assert!(matches!(
            parse("tokeman rotate daemon").unwrap().command,
            Some(Command::Rotate {
                action: RotateAction::Daemon
            })
        ));
        // The scheduled form older launchers passed.
        assert!(parse("tokeman rotate run --scheduled").is_ok());
        // The SessionStart hook 0.3 installed, as the shell hands it over.
        let hook = Cli::try_parse_from([
            "/usr/local/bin/tokeman",
            "rotate",
            "observe-session",
            "--pid",
            "4242",
        ])
        .unwrap();
        assert!(matches!(
            hook.command,
            Some(Command::Rotate {
                action: RotateAction::ObserveSession { pid: 4242 }
            })
        ));
    }

    #[test]
    fn a_removed_spelling_does_not_parse_and_names_its_replacement() {
        let old = [
            ("login work", "tokeman claude login"),
            ("add work key", "tokeman claude add"),
            ("remove work", "tokeman claude remove"),
            ("refresh", "tokeman claude refresh"),
            ("resets", "tokeman claude reset"),
            ("resets list", "tokeman claude reset"),
            (
                "resets use work --grant g",
                "tokeman claude reset <account>",
            ),
            (
                "resets clear work",
                "tokeman claude reset <account> --clear",
            ),
            ("rotate use work", "tokeman claude use"),
            (
                "codex seat work --into x",
                "tokeman codex use <account> --into",
            ),
            ("codex reseat --dry-run", "tokeman codex use"),
            ("services", "tokeman agy list"),
            ("--json services", "tokeman agy list"),
            ("services list", "tokeman agy list"),
            ("services add zai glm --key-file k", "tokeman glm add"),
            ("services remove glm", "tokeman agy remove"),
            ("services import antigravity", "tokeman agy import"),
            ("services use a@example.com", "tokeman agy use"),
            ("services best antigravity", "tokeman agy best"),
        ];
        for (line, replacement) in old {
            let args: Vec<String> = line.split_whitespace().map(str::to_owned).collect();
            let (spelling, new) =
                removed(&args).unwrap_or_else(|| panic!("{line} is not in the table"));
            assert!(new.starts_with(replacement), "{line}: {new}");
            assert!(
                line.trim_start_matches("--json ")
                    .starts_with(spelling.trim_start_matches("tokeman ")),
                "{line}: {spelling}"
            );
            // None of them is an alias: clap refuses every one.
            assert!(
                parse(&format!("tokeman {line}")).is_err(),
                "{line} still parses"
            );
        }
        // The new tree is never mistaken for an old spelling.
        for line in [
            "claude login",
            "codex use work",
            "rotate status",
            "list",
            "agy use a",
        ] {
            let args: Vec<String> = line.split_whitespace().map(str::to_owned).collect();
            assert_eq!(removed(&args), None, "{line}");
        }
    }
}
