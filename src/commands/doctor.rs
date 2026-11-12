//! `tokeman doctor`: one fresh read of every account, then what is wrong
//! with tokeman's view of the services and of this machine, each problem
//! with the command that fixes it.
//!
//! [`gather`] does the reading; [`Report::assemble`] is pure and decides
//! what counts as a problem.

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::Serialize;

use super::{now, view};
use crate::account::Fleet;
use crate::anthropic;
use crate::claims::{Journal, Pending};
use crate::claude::settings as claude_settings;
use crate::codex;
use crate::drift;
use crate::paths;
use crate::providers::antigravity::seat;
use crate::quota::{ReadError, Reading, Service};
use crate::ui::overview::Monitor;

/// The tool versions this build was verified with: its parsers, its
/// identity strings and what it assumes about each tool's files were checked
/// against these.
pub const VERIFIED_CLAUDE_CODE: &str = "2.1.322";
pub const VERIFIED_CODEX: &str = "0.167.0";
pub const VERIFIED_AGY: &str = "1.5.6";
pub const VERIFIED_ANTIGRAVITY_HUB: &str = "2.31.2";

/// How long a tool may take to say its version.
const VERSION_TIMEOUT: Duration = Duration::from_secs(5);

/// Whether the service answered the read.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Reach {
    Answered,
    /// The request did not complete, or an edge refused it before the
    /// service saw it.
    NoAnswer,
    /// No request was made: there was nothing to send.
    NotAsked,
}

/// What the read says about the account's credential.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CredentialState {
    Accepted,
    /// The account has none.
    Missing,
    /// It could not be read, or its grant could not be renewed.
    Unusable,
    /// The service refused it (401, 403).
    Refused,
    /// The read did not get far enough to say.
    Unknown,
}

/// One account, as its fresh read left it.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct AccountCheck {
    pub service: Service,
    pub name: String,
    pub reach: Reach,
    pub credential: CredentialState,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<ReadError>,
    /// The account's browser login was refused for good (Claude).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub login_error: Option<String>,
    /// What a part of the read that is not needed for quota said when it
    /// failed (the profile endpoint, the reset programs, the credit list).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub notes: Vec<String>,
    /// What the answers carried that this build does not know.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub unrecognized: Vec<String>,
}

fn classify(error: Option<&ReadError>) -> (Reach, CredentialState) {
    match error {
        None | Some(ReadError::Decode { .. }) => (Reach::Answered, CredentialState::Accepted),
        Some(ReadError::NoCredential) => (Reach::NotAsked, CredentialState::Missing),
        Some(ReadError::Credential { .. }) => (Reach::NotAsked, CredentialState::Unusable),
        Some(ReadError::Network { .. } | ReadError::Edge { .. }) => {
            (Reach::NoAnswer, CredentialState::Unknown)
        }
        Some(ReadError::Http {
            status: 401 | 403, ..
        }) => (Reach::Answered, CredentialState::Refused),
        Some(ReadError::Http { .. } | ReadError::Other { .. }) => {
            (Reach::Answered, CredentialState::Unknown)
        }
    }
}

impl AccountCheck {
    pub fn of(reading: &Reading) -> Self {
        let (reach, credential) = classify(reading.error.as_ref());
        Self {
            service: reading.service,
            name: reading.account.clone(),
            reach,
            credential,
            error: reading.error.clone(),
            login_error: None,
            notes: reading
                .claude()
                .and_then(|claude| claude.profile_error.clone())
                .into_iter()
                .collect(),
            unrecognized: reading.unrecognized.clone(),
        }
    }
}

/// An installed tool's version against the one this build was verified with.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum VersionState {
    Verified,
    Newer,
    Older,
    /// The tool is not installed, or did not say its version in time.
    NotFound,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ToolCheck {
    pub tool: &'static str,
    pub verified: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub installed: Option<String>,
    pub state: VersionState,
}

/// The numbers of a dotted version; `None` when a part is not a number.
fn version_numbers(version: &str) -> Option<Vec<u64>> {
    version.split('.').map(|part| part.parse().ok()).collect()
}

/// The first word of `text` that is a dotted number (`codex-cli 0.167.0`,
/// `2.1.322 (Claude Code)`).
fn version_in(text: &str) -> Option<String> {
    text.split_whitespace()
        .find(|word| word.contains('.') && version_numbers(word).is_some())
        .map(str::to_owned)
}

impl ToolCheck {
    pub fn of(tool: &'static str, verified: &'static str, installed: Option<String>) -> Self {
        let state = match installed.as_deref().and_then(version_numbers) {
            None => VersionState::NotFound,
            Some(installed) => {
                match installed.cmp(&version_numbers(verified).unwrap_or_default()) {
                    std::cmp::Ordering::Equal => VersionState::Verified,
                    std::cmp::Ordering::Greater => VersionState::Newer,
                    std::cmp::Ordering::Less => VersionState::Older,
                }
            }
        };
        Self {
            tool,
            verified,
            installed: installed.filter(|_| state != VersionState::NotFound),
            state,
        }
    }
}

/// The SessionStart hook in Claude's settings.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum Hook {
    Installed {
        command: String,
    },
    /// The hook names a binary that is not there.
    Dangling {
        command: String,
        binary: String,
    },
    Missing,
    /// The settings file could not be read.
    Unreadable {
        message: String,
    },
}

/// The binary a hook command runs: its first word, unquoted. `None` for a
/// command whose first word is not a plain or single-quoted path.
fn hook_binary(command: &str) -> Option<String> {
    let command = command.trim_start();
    match command.strip_prefix('\'') {
        Some(rest) => {
            let (binary, after) = rest.split_once('\'')?;
            // `'\''` continues the word; such a path is not judged.
            (after.is_empty() || after.starts_with(' ')).then(|| binary.to_owned())
        }
        None => command
            .split_whitespace()
            .next()
            .filter(|word| !word.contains(['"', '\\', '$']))
            .map(str::to_owned),
    }
}

impl Hook {
    /// `exists` says whether a binary path is there.
    pub fn of(
        settings: Result<&claude_settings::Settings, String>,
        exists: impl Fn(&Path) -> bool,
    ) -> Self {
        let settings = match settings {
            Ok(settings) => settings,
            Err(message) => return Self::Unreadable { message },
        };
        let Some(command) = claude_settings::observer_hook(settings) else {
            return Self::Missing;
        };
        match hook_binary(&command) {
            // A bare name is looked up on PATH when the hook runs.
            Some(binary) if binary.contains('/') && !exists(Path::new(&binary)) => {
                Self::Dangling { command, binary }
            }
            _ => Self::Installed { command },
        }
    }
}

/// Whether a file's mode keeps it to its owner.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ModeState {
    Private,
    /// Bits beyond the expected mode are set.
    TooOpen,
    Missing,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct FileCheck {
    pub path: PathBuf,
    /// The permission bits found, when the file is there.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mode: Option<u32>,
    /// The most the mode may allow.
    pub expected: u32,
    pub state: ModeState,
}

impl FileCheck {
    pub fn of(path: PathBuf, mode: Option<u32>, expected: u32) -> Self {
        let state = match mode {
            None => ModeState::Missing,
            Some(mode) if mode & !expected != 0 => ModeState::TooOpen,
            Some(_) => ModeState::Private,
        };
        Self {
            path,
            mode,
            expected,
            state,
        }
    }
}

#[cfg(unix)]
fn mode_of(path: &Path) -> Option<u32> {
    use std::os::unix::fs::PermissionsExt;
    Some(std::fs::metadata(path).ok()?.permissions().mode() & 0o7777)
}

/// The login slot agy, the Antigravity app and `agy acp` share.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum AgySlot {
    /// Holds a login tokeman can read; `registered` when it is one of
    /// tokeman's accounts.
    Readable {
        account: String,
        registered: bool,
    },
    /// agy is signed out.
    Empty,
    Unreadable {
        message: String,
    },
    /// No Antigravity account is registered, or this is not macOS.
    NotChecked,
}

/// A problem, and the command that fixes it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Problem {
    pub what: String,
    pub fix: String,
}

/// Everything `doctor` found, before judging it.
#[derive(Debug, Clone)]
pub struct Findings {
    pub at: i64,
    pub accounts: Vec<AccountCheck>,
    pub tools: Vec<ToolCheck>,
    pub hook: Hook,
    pub daemon: Monitor,
    pub files: Vec<FileCheck>,
    pub agy_slot: AgySlot,
    /// Reset claims without a known outcome, or why the journal did not read.
    pub claims: Result<Vec<Pending>, String>,
    /// What the daemon has on its drift ledger.
    pub ledger: Vec<drift::Entry>,
}

/// What `tokeman doctor` reports; `tokeman doctor --json` is its
/// serialization.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Report {
    pub at: i64,
    pub healthy: bool,
    pub problems: Vec<Problem>,
    pub accounts: Vec<AccountCheck>,
    pub tools: Vec<ToolCheck>,
    pub hook: Hook,
    pub daemon: Monitor,
    pub files: Vec<FileCheck>,
    pub agy_slot: AgySlot,
    pub pending_claims: Vec<Pending>,
    /// Items on the daemon's ledger that no read made just now carried.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub seen_by_daemon: Vec<drift::Entry>,
}

const UPGRADE: &str = "upgrade tokeman (in its checkout: `git pull && ./scripts/install.sh`); if it is current, \
     report the items at https://github.com/emberian/tokeman/issues";

/// The command that adds or renews the credential of an account.
fn credential_fix(service: Service, name: &str) -> String {
    match service {
        Service::Claude => format!("tokeman claude login {name}"),
        Service::Codex => format!("tokeman codex refresh {name}"),
        Service::Antigravity => "sign in with `agy`, then `tokeman agy import`".to_owned(),
        Service::Zai => format!("tokeman glm add {name} --key-file <file with a working key>"),
        Service::Kimi => format!("tokeman kimi add {name} --key-file <file with a working key>"),
    }
}

/// An error in a problem line: an HTTP answer by its status and cause, the
/// rest in their own words, cut short.
fn brief(error: &ReadError) -> String {
    match error {
        ReadError::Http { .. } => error.reason(),
        other => crate::util::one_line(&other.to_string(), 160),
    }
}

fn account_problem(account: &AccountCheck) -> Option<Problem> {
    let who = format!("{} ({})", account.name, account.service.label());
    let error = account.error.as_ref()?;
    let error = &brief(error);
    let unreadable = matches!(account.error, Some(ReadError::Decode { .. }));
    let (what, fix) = match (account.reach, account.credential, unreadable) {
        (_, _, true) => (
            format!("{who}: the answer could not be read: {error}"),
            UPGRADE.to_owned(),
        ),
        (_, CredentialState::Missing, _) => (
            format!("{who} has no usable credential"),
            credential_fix(account.service, &account.name),
        ),
        (_, CredentialState::Unusable, _) => (
            format!("{who}: the credential cannot be used: {error}"),
            credential_fix(account.service, &account.name),
        ),
        (_, CredentialState::Refused, _) => (
            format!("{who}: the service refused the credential: {error}"),
            format!(
                "{}; if the account is gone, `tokeman {} remove {}`",
                credential_fix(account.service, &account.name),
                super::services::command_word(account.service),
                account.name
            ),
        ),
        (Reach::NoAnswer, _, _) => (
            format!("{who} was not reached: {error}"),
            "check the network, then `tokeman doctor` again".to_owned(),
        ),
        _ => (
            format!("{who}: the read failed: {error}"),
            "`tokeman doctor` again in a few minutes; if it stays, see the service's status page"
                .to_owned(),
        ),
    };
    Some(Problem { what, fix })
}

impl Report {
    /// Judge the findings. A problem is something a command can fix or
    /// that means tokeman reads a service wrongly: an account that did not
    /// read, a login refused, an item no parser knows, a hook or service
    /// that should run and does not, a file others can read, an agy login
    /// that does not read, a claim without an answer. A tool at another
    /// version than the verified one, a paused service and a machine
    /// without the service are stated, not counted.
    pub fn assemble(findings: Findings) -> Self {
        let mut problems = Vec::new();
        for account in &findings.accounts {
            problems.extend(account_problem(account));
            if let Some(error) = &account.login_error {
                problems.push(Problem {
                    what: format!("{}: the browser login was refused ({error})", account.name),
                    fix: format!("tokeman claude login {}", account.name),
                });
            }
        }

        let fresh: Vec<(Service, &str)> = findings
            .accounts
            .iter()
            .flat_map(|account| {
                account
                    .unrecognized
                    .iter()
                    .map(|item| (account.service, item.as_str()))
            })
            .collect();
        let seen_by_daemon: Vec<drift::Entry> = findings
            .ledger
            .into_iter()
            .filter(|entry| !fresh.contains(&(entry.service, entry.item.as_str())))
            .collect();
        for service in Service::ALL {
            let mut items: Vec<&str> = fresh
                .iter()
                .filter(|(of, _)| *of == service)
                .map(|(_, item)| *item)
                .chain(
                    seen_by_daemon
                        .iter()
                        .filter(|entry| entry.service == service)
                        .map(|entry| entry.item.as_str()),
                )
                .collect();
            items.sort_unstable();
            items.dedup();
            if !items.is_empty() {
                problems.push(Problem {
                    what: format!(
                        "{} sent {} this build does not know: {}",
                        service.label(),
                        if items.len() == 1 {
                            "something".to_owned()
                        } else {
                            format!("{} things", items.len())
                        },
                        items.join(", ")
                    ),
                    fix: UPGRADE.to_owned(),
                });
            }
        }

        let service_expected = matches!(
            findings.daemon,
            Monitor::Running | Monitor::Paused | Monitor::Stopped
        );
        match &findings.hook {
            Hook::Installed { .. } => {}
            Hook::Dangling { binary, .. } => problems.push(Problem {
                what: format!("the SessionStart hook runs {binary}, which is not there"),
                fix: "tokeman rotate install".to_owned(),
            }),
            Hook::Missing if service_expected => problems.push(Problem {
                what: "the SessionStart hook is not in Claude's settings, so sessions are not \
                       bound to accounts"
                    .to_owned(),
                fix: "tokeman rotate install".to_owned(),
            }),
            Hook::Missing => {}
            Hook::Unreadable { message } => problems.push(Problem {
                what: format!("Claude's settings could not be read: {message}"),
                fix: format!(
                    "repair the JSON in {}",
                    paths::tilde(&paths::claude_settings())
                ),
            }),
        }
        if findings.daemon == Monitor::Stopped {
            problems.push(Problem {
                what: "the background service is installed and no daemon is running".to_owned(),
                fix: "tokeman rotate install".to_owned(),
            });
        }
        for file in &findings.files {
            if file.state == ModeState::TooOpen {
                problems.push(Problem {
                    what: format!(
                        "{} has mode {:o}; others on this machine can read it",
                        paths::tilde(&file.path),
                        file.mode.unwrap_or_default()
                    ),
                    fix: format!(
                        "chmod {:o} {}",
                        file.expected,
                        crate::util::shell_quote(&file.path.to_string_lossy())
                    ),
                });
            }
        }
        if let AgySlot::Unreadable { message } = &findings.agy_slot {
            problems.push(Problem {
                what: format!("agy's Keychain login could not be read: {message}"),
                fix: "sign in with `agy`, then `tokeman agy import`".to_owned(),
            });
        }
        let pending_claims = match findings.claims {
            Ok(pending) => pending,
            Err(message) => {
                problems.push(Problem {
                    what: format!("the claim journal could not be read: {message}"),
                    fix: format!(
                        "look at {}; each entry is a reset claim that may have been spent",
                        paths::tilde(&paths::claims_journal())
                    ),
                });
                Vec::new()
            }
        };
        for pending in &pending_claims {
            let word = super::services::command_word(pending.service);
            let what = format!(
                "a reset claim on {} ({}) sent at {} has no known outcome",
                pending.account,
                pending.service.label(),
                chrono::DateTime::from_timestamp(pending.sent_at, 0).unwrap_or_default()
            );
            let resend = match (&pending.item, pending.service) {
                (Some(item), Service::Claude) => {
                    format!("tokeman claude reset {} --grant {item}", pending.account)
                }
                (Some(item), _) => {
                    format!("tokeman {word} reset {} --credit {item}", pending.account)
                }
                (None, _) => format!("tokeman {word} reset {}", pending.account),
            };
            problems.push(Problem {
                what,
                fix: format!(
                    "{resend} resends it; `tokeman {word} reset {} --clear` forgets it",
                    pending.account
                ),
            });
        }

        Self {
            at: findings.at,
            healthy: problems.is_empty(),
            problems,
            accounts: findings.accounts,
            tools: findings.tools,
            hook: findings.hook,
            daemon: findings.daemon,
            files: findings.files,
            agy_slot: findings.agy_slot,
            pending_claims,
            seen_by_daemon,
        }
    }
}

/// What `binary <arg>` prints, waiting at most [`VERSION_TIMEOUT`].
async fn tool_version(binary: &str, arg: &str) -> Option<String> {
    let run = tokio::process::Command::new(binary)
        .arg(arg)
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true)
        .output();
    let output = tokio::time::timeout(VERSION_TIMEOUT, run)
        .await
        .ok()?
        .ok()?;
    output
        .status
        .success()
        .then(|| version_in(&String::from_utf8_lossy(&output.stdout)))?
}

async fn tools() -> Vec<ToolCheck> {
    let claude = std::env::var("TOKEMAN_CLAUDE_BIN").unwrap_or_else(|_| "claude".into());
    let (claude, codex, agy) = tokio::join!(
        tool_version(&claude, "--version"),
        tool_version("codex", "--version"),
        tool_version("agy", "--version"),
    );
    vec![
        ToolCheck::of("Claude Code", VERIFIED_CLAUDE_CODE, claude),
        ToolCheck::of("codex", VERIFIED_CODEX, codex),
        ToolCheck::of("agy", VERIFIED_AGY, agy),
        ToolCheck::of(
            "Antigravity hub",
            VERIFIED_ANTIGRAVITY_HUB,
            crate::providers::antigravity::installed_app_version(),
        ),
    ]
}

/// The reset programs of each Claude account with a profile credential and
/// the credit list of each Codex account: what their answers carried that is
/// not known, and why a read failed.
async fn reset_reads(fleet: &Fleet, accounts: &mut [AccountCheck], at: i64) {
    let cache = anthropic::profile::Cache::default();
    let claude = futures::future::join_all(
        fleet
            .accounts
            .iter()
            .filter(|account| account.profile_token(at * 1000).is_some())
            .map(|account| {
                let cache = &cache;
                async move {
                    let client = anthropic::Client::shared();
                    let status = anthropic::resets::status(client, cache, account, false, at).await;
                    (Service::Claude, account.name.clone(), status)
                }
            }),
    );
    let codex_client = codex::Client::new().ok();
    let codex = futures::future::join_all(fleet.codex.iter().filter_map(|account| {
        let client = codex_client.as_ref()?;
        Some(async move {
            (
                account.name.clone(),
                codex::resets::list(client, account).await,
            )
        })
    }));
    let (claude, codex) = tokio::join!(claude, codex);
    let mut found = |service: Service, name: &str| {
        accounts
            .iter_mut()
            .position(|account| account.service == service && account.name == name)
    };
    let mut merges: Vec<(usize, Vec<String>, Vec<String>)> = Vec::new();
    for (service, name, status) in claude {
        let Some(index) = found(service, &name) else {
            continue;
        };
        match status {
            Ok(status) => merges.push((
                index,
                status.unrecognized,
                status
                    .errors
                    .iter()
                    .map(|error| format!("reset programs: {error}"))
                    .collect(),
            )),
            Err(error) => merges.push((
                index,
                Vec::new(),
                vec![format!("reset programs: {error:#}")],
            )),
        }
    }
    for (name, list) in codex {
        let Some(index) = found(Service::Codex, &name) else {
            continue;
        };
        match list {
            Ok(list) => merges.push((index, list.unrecognized, Vec::new())),
            Err(error) => merges.push((
                index,
                Vec::new(),
                vec![format!("reset credit list: {error}")],
            )),
        }
    }
    for (index, unrecognized, notes) in merges {
        let account = &mut accounts[index];
        // An account whose usage read failed already says why.
        if account.error.is_none() {
            account.notes.extend(notes);
        }
        drift::merge(&mut account.unrecognized, unrecognized);
    }
}

async fn agy_slot(fleet: &Fleet) -> AgySlot {
    let accounts: Vec<_> = fleet
        .services
        .iter()
        .filter(|account| account.service == Service::Antigravity)
        .cloned()
        .collect();
    if accounts.is_empty() || !cfg!(target_os = "macos") {
        return AgySlot::NotChecked;
    }
    let read = tokio::task::spawn_blocking(move || {
        seat::slot().map(|slot| {
            slot.map(|slot| {
                let registered = accounts.iter().any(|account| slot.holds(account));
                (slot.email, registered)
            })
        })
    })
    .await;
    match read {
        Ok(Ok(Some((account, registered)))) => AgySlot::Readable {
            account,
            registered,
        },
        Ok(Ok(None)) => AgySlot::Empty,
        Ok(Err(error)) => AgySlot::Unreadable {
            message: format!("{error:#}"),
        },
        Err(error) => AgySlot::Unreadable {
            message: error.to_string(),
        },
    }
}

fn files() -> Vec<FileCheck> {
    #[cfg(unix)]
    {
        let (dir, tokens) = (paths::config_dir(), paths::accounts_file());
        vec![
            FileCheck::of(dir.clone(), mode_of(&dir), 0o700),
            FileCheck::of(tokens.clone(), mode_of(&tokens), 0o600),
        ]
    }
    #[cfg(not(unix))]
    Vec::new()
}

fn pending_claims(fleet: &Fleet) -> Result<Vec<Pending>, String> {
    let journal = Journal::default();
    let mut pending = Vec::new();
    for (service, account) in fleet.account_keys() {
        if !matches!(service, Service::Claude | Service::Codex) {
            continue;
        }
        match journal.pending(service, account) {
            Ok(found) => pending.extend(found),
            Err(error) => return Err(format!("{error:#}")),
        }
    }
    Ok(pending)
}

/// Read every account now (recording the reads, like any explicit read) and
/// look at this machine.
pub async fn gather(fleet: &Fleet) -> Findings {
    let at = now();
    let (readings, tools, agy_slot) =
        tokio::join!(view::probe(fleet, None), tools(), agy_slot(fleet));
    let mut accounts: Vec<AccountCheck> = readings.iter().map(AccountCheck::of).collect();
    for account in &mut accounts {
        if account.service == Service::Claude {
            account.login_error = fleet
                .account(&account.name)
                .and_then(|account| account.login.as_ref())
                .and_then(|login| login.error.clone());
        }
    }
    reset_reads(fleet, &mut accounts, at).await;
    let settings = claude_settings::load().map_err(|error| format!("{error:#}"));
    Findings {
        at,
        accounts,
        tools,
        hook: Hook::of(settings.as_ref().map_err(Clone::clone), Path::exists),
        daemon: Monitor::now(),
        files: files(),
        agy_slot,
        claims: pending_claims(fleet),
        ledger: drift::Ledger::default().entries(),
    }
}

/// `tokeman doctor`.
pub async fn run(fleet: &Fleet) -> Report {
    Report::assemble(gather(fleet).await)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn healthy() -> Findings {
        Findings {
            at: 1_794_000_000,
            accounts: vec![AccountCheck::of(&Reading::empty(
                Service::Claude,
                "first@example.com",
                1_794_000_000,
            ))],
            tools: vec![ToolCheck::of(
                "codex",
                VERIFIED_CODEX,
                Some(VERIFIED_CODEX.to_owned()),
            )],
            hook: Hook::Installed {
                command: "'/bin/tokeman' rotate observe-session --pid \"$PPID\"".into(),
            },
            daemon: Monitor::Running,
            files: vec![FileCheck::of("/c/tokens.toml".into(), Some(0o600), 0o600)],
            agy_slot: AgySlot::NotChecked,
            claims: Ok(Vec::new()),
            ledger: Vec::new(),
        }
    }

    fn failed(service: Service, error: ReadError) -> AccountCheck {
        AccountCheck::of(&Reading::failed(service, "second@example.com", 5, error))
    }

    #[test]
    fn nothing_wrong_is_healthy_and_states_no_problem() {
        let report = Report::assemble(healthy());
        assert!(report.healthy);
        assert!(report.problems.is_empty());
        assert_eq!(report.accounts[0].reach, Reach::Answered);
        assert_eq!(report.accounts[0].credential, CredentialState::Accepted);
    }

    #[test]
    fn a_read_error_says_whether_the_service_answered_and_what_became_of_the_credential() {
        let http = |status| ReadError::Http {
            status,
            body: "x".into(),
        };
        let network = ReadError::Network {
            message: "timed out".into(),
        };
        let unreadable = ReadError::Decode {
            message: "odd".into(),
        };
        let unusable = ReadError::Credential {
            message: "no key file".into(),
        };
        for (error, reach, credential) in [
            (http(401), Reach::Answered, CredentialState::Refused),
            (http(403), Reach::Answered, CredentialState::Refused),
            (http(500), Reach::Answered, CredentialState::Unknown),
            (network, Reach::NoAnswer, CredentialState::Unknown),
            (
                ReadError::Edge {
                    status: 403,
                    attempts: 4,
                },
                Reach::NoAnswer,
                CredentialState::Unknown,
            ),
            (
                ReadError::NoCredential,
                Reach::NotAsked,
                CredentialState::Missing,
            ),
            (unusable, Reach::NotAsked, CredentialState::Unusable),
            (unreadable, Reach::Answered, CredentialState::Accepted),
        ] {
            assert_eq!(classify(Some(&error)), (reach, credential), "{error:?}");
        }
    }

    #[test]
    fn each_failing_account_is_a_problem_with_the_command_for_its_service() {
        let mut findings = healthy();
        findings.accounts = vec![
            failed(Service::Claude, ReadError::NoCredential),
            failed(
                Service::Codex,
                ReadError::Http {
                    status: 401,
                    body: "expired".into(),
                },
            ),
            failed(
                Service::Kimi,
                ReadError::Credential {
                    message: "the key file is empty".into(),
                },
            ),
            failed(
                Service::Zai,
                ReadError::Network {
                    message: "timed out".into(),
                },
            ),
            failed(
                Service::Antigravity,
                ReadError::Decode {
                    message: "quota summary carried no groups".into(),
                },
            ),
        ];
        let report = Report::assemble(findings);
        assert!(!report.healthy);
        let fixes: Vec<&str> = report.problems.iter().map(|p| p.fix.as_str()).collect();
        assert_eq!(fixes.len(), 5);
        assert_eq!(fixes[0], "tokeman claude login second@example.com");
        assert!(fixes[1].starts_with("tokeman codex refresh second@example.com"));
        assert!(fixes[1].contains("tokeman codex remove second@example.com"));
        assert!(fixes[2].starts_with("tokeman kimi add second@example.com --key-file"));
        assert!(fixes[3].contains("tokeman doctor"));
        assert_eq!(fixes[4], UPGRADE);
    }

    #[test]
    fn a_refused_login_is_a_problem_even_when_the_setup_key_reads() {
        let mut findings = healthy();
        findings.accounts[0].login_error = Some("invalid_grant".into());
        let report = Report::assemble(findings);
        assert_eq!(report.problems.len(), 1);
        assert_eq!(
            report.problems[0].fix,
            "tokeman claude login first@example.com"
        );
    }

    #[test]
    fn unrecognised_items_are_one_problem_per_service_with_what_the_daemon_saw() {
        let entry = |service, item: &str| drift::Entry {
            service,
            item: item.to_owned(),
            seen: drift::Seen {
                first_seen: 9,
                account: "first@example.com".into(),
            },
        };
        let mut findings = healthy();
        findings.accounts[0].unrecognized = vec!["usage.zinc_parrot".into()];
        findings.ledger = vec![
            // Seen again just now: said once.
            entry(Service::Claude, "usage.zinc_parrot"),
            // Only the daemon saw it (a header at a wall).
            entry(
                Service::Claude,
                "header anthropic-ratelimit-unified-8h-reset",
            ),
            entry(Service::Kimi, "usages.usages.limit_day"),
        ];
        let report = Report::assemble(findings);
        assert_eq!(report.seen_by_daemon.len(), 2);
        assert_eq!(report.problems.len(), 2);
        assert_eq!(
            report.problems[0].what,
            "Claude sent 2 things this build does not know: header \
             anthropic-ratelimit-unified-8h-reset, usage.zinc_parrot"
        );
        assert_eq!(
            report.problems[1].what,
            "Kimi Code sent something this build does not know: usages.usages.limit_day"
        );
        assert!(report.problems.iter().all(|p| p.fix == UPGRADE));
    }

    #[test]
    fn a_tool_is_compared_with_its_verified_version_and_never_counts_as_a_problem() {
        let state = |installed: Option<&str>| {
            ToolCheck::of("codex", "0.167.0", installed.map(str::to_owned)).state
        };
        assert_eq!(state(Some("0.167.0")), VersionState::Verified);
        assert_eq!(state(Some("0.170.1")), VersionState::Newer);
        assert_eq!(state(Some("0.99.12")), VersionState::Older);
        assert_eq!(state(Some("1.0")), VersionState::Newer);
        assert_eq!(state(Some("nightly")), VersionState::NotFound);
        assert_eq!(state(None), VersionState::NotFound);
        assert_eq!(
            version_in("codex-cli 0.167.0\n").as_deref(),
            Some("0.167.0")
        );
        assert_eq!(
            version_in("2.1.322 (Claude Code)").as_deref(),
            Some("2.1.322")
        );
        assert_eq!(version_in("no version here"), None);

        let mut findings = healthy();
        findings.tools = vec![
            ToolCheck::of("codex", "0.167.0", Some("0.200.0".into())),
            ToolCheck::of("agy", "1.5.6", None),
        ];
        assert!(Report::assemble(findings).healthy);
    }

    #[test]
    fn the_hook_is_found_by_its_command_and_a_missing_binary_is_dangling() {
        let settings = |command: &str| -> claude_settings::Settings {
            serde_json::from_value(serde_json::json!({
                "hooks": {"SessionStart": [
                    {"hooks": [{"type": "command", "command": "echo hi"}]},
                    {"hooks": [{"type": "command", "command": command}]}
                ]}
            }))
            .unwrap()
        };
        let here = |path: &Path| path == Path::new("/opt/my tools/tokeman");
        let quoted = "'/opt/my tools/tokeman' rotate observe-session --pid \"$PPID\"";
        assert_eq!(
            Hook::of(Ok(&settings(quoted)), here),
            Hook::Installed {
                command: quoted.into()
            }
        );
        let gone = "'/old/tokeman' rotate observe-session --pid \"$PPID\"";
        assert_eq!(
            Hook::of(Ok(&settings(gone)), here),
            Hook::Dangling {
                command: gone.into(),
                binary: "/old/tokeman".into()
            }
        );
        // A bare name is resolved on PATH when the hook runs; it is not judged.
        let bare = "tokeman rotate observe-session --pid \"$PPID\"";
        assert!(matches!(
            Hook::of(Ok(&settings(bare)), here),
            Hook::Installed { .. }
        ));
        assert_eq!(
            Hook::of(Ok(&claude_settings::Settings::new()), here),
            Hook::Missing
        );
        assert_eq!(
            Hook::of(Err("bad json".into()), here),
            Hook::Unreadable {
                message: "bad json".into()
            }
        );
    }

    #[test]
    fn a_missing_hook_is_a_problem_only_where_the_service_is_installed() {
        let mut findings = healthy();
        findings.hook = Hook::Missing;
        findings.daemon = Monitor::NotInstalled;
        assert!(Report::assemble(findings.clone()).healthy);
        findings.daemon = Monitor::Paused;
        let report = Report::assemble(findings);
        assert_eq!(report.problems.len(), 1);
        assert_eq!(report.problems[0].fix, "tokeman rotate install");
    }

    #[test]
    fn a_stopped_service_is_a_problem_and_a_paused_one_is_not() {
        let mut findings = healthy();
        findings.daemon = Monitor::Paused;
        assert!(Report::assemble(findings.clone()).healthy);
        findings.daemon = Monitor::Stopped;
        let report = Report::assemble(findings);
        assert_eq!(report.problems.len(), 1);
        assert!(report.problems[0].what.contains("no daemon is running"));
    }

    #[test]
    fn a_file_others_can_read_is_a_problem_with_its_chmod() {
        assert_eq!(
            FileCheck::of("/c".into(), Some(0o700), 0o700).state,
            ModeState::Private
        );
        assert_eq!(
            FileCheck::of("/c/t".into(), Some(0o400), 0o600).state,
            ModeState::Private
        );
        assert_eq!(
            FileCheck::of("/c/t".into(), None, 0o600).state,
            ModeState::Missing
        );
        let mut findings = healthy();
        findings.files = vec![
            FileCheck::of("/c/it's".into(), Some(0o755), 0o700),
            FileCheck::of("/c/tokens.toml".into(), Some(0o644), 0o600),
        ];
        let report = Report::assemble(findings);
        let fixes: Vec<&str> = report.problems.iter().map(|p| p.fix.as_str()).collect();
        assert_eq!(
            fixes,
            ["chmod 700 '/c/it'\\''s'", "chmod 600 '/c/tokens.toml'"]
        );
        assert!(report.problems[1].what.contains("mode 644"));
    }

    #[test]
    fn an_unreadable_agy_login_and_an_unanswered_claim_are_problems() {
        let mut findings = healthy();
        findings.agy_slot = AgySlot::Unreadable {
            message: "security timed out".into(),
        };
        findings.claims = Ok(vec![
            Pending {
                service: Service::Claude,
                account: "first@example.com".into(),
                item: Some("fable-launch".into()),
                request_id: "r1".into(),
                sent_at: 1_794_000_000,
            },
            Pending {
                service: Service::Codex,
                account: "work".into(),
                item: None,
                request_id: "r2".into(),
                sent_at: 1_794_000_000,
            },
        ]);
        let report = Report::assemble(findings);
        assert_eq!(report.problems.len(), 3);
        assert!(report.problems[0].fix.contains("tokeman agy import"));
        assert!(
            report.problems[1]
                .fix
                .starts_with("tokeman claude reset first@example.com --grant fable-launch resends")
        );
        assert!(
            report.problems[1]
                .fix
                .contains("`tokeman claude reset first@example.com --clear`")
        );
        assert!(
            report.problems[2]
                .fix
                .starts_with("tokeman codex reset work resends")
        );
        assert_eq!(report.pending_claims.len(), 2);

        let mut findings = healthy();
        findings.claims = Err("claims.json is not JSON".into());
        let report = Report::assemble(findings);
        assert_eq!(report.problems.len(), 1);
        assert!(report.problems[0].what.contains("claims.json is not JSON"));
    }

    #[test]
    fn the_report_serializes_with_fixed_words_for_every_state() {
        let mut findings = healthy();
        findings.accounts.push(failed(
            Service::Zai,
            ReadError::Network {
                message: "timed out".into(),
            },
        ));
        findings.agy_slot = AgySlot::Readable {
            account: "first@example.com".into(),
            registered: true,
        };
        let json = serde_json::to_value(Report::assemble(findings)).unwrap();
        assert_eq!(json["healthy"], false);
        assert_eq!(json["accounts"][0]["reach"], "answered");
        assert_eq!(json["accounts"][0]["credential"], "accepted");
        assert!(json["accounts"][0].get("unrecognized").is_none());
        assert_eq!(json["accounts"][1]["reach"], "no_answer");
        assert_eq!(json["accounts"][1]["error"]["kind"], "network");
        assert_eq!(json["tools"][0]["state"], "verified");
        assert_eq!(json["hook"]["state"], "installed");
        assert_eq!(json["daemon"], "running");
        assert_eq!(json["files"][0]["state"], "private");
        assert_eq!(json["agy_slot"]["state"], "readable");
        assert_eq!(
            json["problems"][0]["fix"],
            "check the network, then `tokeman doctor` again"
        );
    }
}
