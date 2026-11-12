//! The `agy` CLI's login slot, and seating an Antigravity account in it.
//!
//! `agy` keeps one Google login in a macOS Keychain generic password
//! (service `gemini`, account `antigravity`) through go-keyring, which drives
//! `/usr/bin/security`; the item's access list trusts that tool, so tokeman
//! reads and writes it the same way. The value is `go-keyring-base64:` and
//! the base64 of `{"token": <oauth2.Token>, "auth_method", "id_token_email"}`.
//!
//! A newly started `agy` signs in as whatever the slot holds. A running one
//! keeps the account it started with, and at its next token refresh (within
//! the hour) writes that account back into the slot, undoing a seating made
//! while it ran. `agy` also deletes its fallback token file whenever it saves
//! to the Keychain. Google does not rotate the refresh token and both
//! sign-ins use one OAuth client, so copying tokeman's grant into the slot
//! leaves tokeman's copy valid.
//!
//! Seating is manual (`tokeman agy use`); the daemon never does it. Other
//! clients share the slot while they run ([`super::clients`]): the switch
//! tells the Antigravity app, asks each `agy acp` server which account it
//! holds, and refuses while an interactive `agy` runs unless forced.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use chrono::{DateTime, Local, SecondsFormat};
use reqwest::Client;
use serde::{Deserialize, Serialize};

use super::clients::{self, ClientKind, Lookout, Running};
use super::grant::{self, Grant};
use crate::fs::{FileLock, read_if_present, write_atomic};
use crate::paths;
use crate::providers::ServiceAccount;
use crate::quota::Service;

const KEYCHAIN_SERVICE: &str = "gemini";
const KEYCHAIN_ACCOUNT: &str = "antigravity";
const KEYRING_PREFIX: &str = "go-keyring-base64:";
const SECURITY_TIMEOUT: Duration = Duration::from_secs(if cfg!(test) { 2 } else { 10 });

/// `/usr/bin/security`, or `$TOKEMAN_SECURITY_BIN` for tests.
fn security_bin() -> PathBuf {
    std::env::var_os("TOKEMAN_SECURITY_BIN")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/usr/bin/security"))
}

/// The file `agy` falls back to when the Keychain does not answer: the same
/// JSON, without the prefix.
pub(super) fn token_file() -> PathBuf {
    paths::expand_tilde("~/.gemini/antigravity-cli/antigravity-oauth-token")
}

/// Where a seating happens; tests point every field at a temporary directory.
#[derive(Debug, Clone)]
struct Place {
    security: PathBuf,
    token_url: String,
    token_file: PathBuf,
    backup: PathBuf,
    lock: PathBuf,
    lookout: Lookout,
}

impl Place {
    fn live() -> Self {
        Self {
            security: security_bin(),
            token_url: grant::TOKEN_URL.into(),
            token_file: token_file(),
            backup: paths::agy_login_backup(),
            lock: paths::agy_seat_lock(),
            lookout: Lookout::live(),
        }
    }
}

/// What the slot holds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Slot {
    pub email: String,
    refresh_token: String,
}

impl Slot {
    fn parse(raw: &str) -> Result<Option<Self>> {
        Ok(grant::parse_cli_grant(raw)?.map(|grant| Self {
            email: grant.name,
            refresh_token: grant.refresh_token.unwrap_or_default(),
        }))
    }

    /// Whether the slot holds `account`: the same grant, or the same email.
    pub fn holds(&self, account: &ServiceAccount) -> bool {
        account.service == Service::Antigravity
            && (account.refresh_token.as_deref() == Some(self.refresh_token.as_str())
                || account.name == self.email)
    }
}

/// The slot's raw value; `None` when `agy` has no login (or off macOS).
pub(super) fn read_slot() -> Result<Option<String>> {
    if !cfg!(target_os = "macos") {
        return Ok(None);
    }
    read_raw(&security_bin())
}

/// The account `agy` is signed in as.
pub fn slot() -> Result<Option<Slot>> {
    match read_slot()? {
        Some(raw) => Slot::parse(&raw),
        None => Ok(None),
    }
}

/// The name of the account in `accounts` that `agy` holds.
pub fn seated(accounts: &[ServiceAccount]) -> Option<String> {
    let slot = slot().ok()??;
    accounts
        .iter()
        .find(|account| slot.holds(account))
        .map(|account| account.name.clone())
}

fn read_raw(security: &Path) -> Result<Option<String>> {
    let args = [
        "find-generic-password",
        "-s",
        KEYCHAIN_SERVICE,
        "-a",
        KEYCHAIN_ACCOUNT,
        "-w",
    ];
    // `security` exits non-zero for a missing item.
    Ok(run(security, &args, None)?.map(|text| text.trim().to_owned()))
}

/// Replace the slot's value the way go-keyring does: the command goes to
/// `security -i` on stdin, so the secret never appears in a process list.
fn write_raw(security: &Path, value: &str) -> Result<()> {
    let payload = value.strip_prefix(KEYRING_PREFIX).unwrap_or(value);
    if !payload
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b"+/=".contains(&b))
    {
        bail!("refusing to write a Keychain value that is not go-keyring base64");
    }
    let command =
        format!("add-generic-password -U -s {KEYCHAIN_SERVICE} -a {KEYCHAIN_ACCOUNT} -w {value}\n");
    run(security, &["-i"], Some(&command))?.context("`security -i` refused the Keychain write")?;
    Ok(())
}

/// Run `security`, waiting at most [`SECURITY_TIMEOUT`]; `None` when it exits
/// non-zero.
fn run(security: &Path, args: &[&str], stdin: Option<&str>) -> Result<Option<String>> {
    use std::io::Read;
    let mut child = Command::new(security)
        .args(args)
        .stdin(if stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .with_context(|| format!("could not run {}", security.display()))?;
    if let Some(input) = stdin {
        // Dropping the handle closes stdin, which ends `security -i`.
        let mut pipe = child.stdin.take().context("no stdin")?;
        pipe.write_all(input.as_bytes())
            .context("could not write to security")?;
    }
    let mut stdout = child.stdout.take().context("no stdout")?;
    let reader = std::thread::spawn(move || {
        let mut text = String::new();
        let _ = stdout.read_to_string(&mut text);
        text
    });
    let deadline = Instant::now() + SECURITY_TIMEOUT;
    let status = loop {
        match child.try_wait()? {
            Some(status) => break status,
            None if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(25)),
            None => {
                let _ = child.kill();
                let _ = child.wait();
                bail!(
                    "{} did not answer within {}s",
                    security.display(),
                    SECURITY_TIMEOUT.as_secs()
                );
            }
        }
    };
    let text = reader.join().unwrap_or_default();
    Ok(status.success().then_some(text))
}

#[derive(Serialize)]
struct SlotDocument<'a> {
    token: SlotToken<'a>,
    auth_method: &'static str,
    id_token_email: &'a str,
}

/// Go's `oauth2.Token`, in its field order.
#[derive(Serialize)]
struct SlotToken<'a> {
    access_token: &'a str,
    token_type: &'static str,
    refresh_token: &'a str,
    expiry: String,
    expires_in: i64,
}

/// The slot value (and the fallback file's text) for a freshly minted grant.
fn slot_values(
    grant: &Grant,
    refresh_token: &str,
    email: &str,
    now: DateTime<Local>,
) -> Result<(String, String)> {
    let expires_in = grant.expires_in.unwrap_or(3599);
    let json = serde_json::to_string(&SlotDocument {
        token: SlotToken {
            access_token: &grant.access_token,
            token_type: "Bearer",
            refresh_token,
            expiry: (now + chrono::Duration::seconds(expires_in))
                .to_rfc3339_opts(SecondsFormat::Micros, false),
            expires_in,
        },
        auth_method: "consumer",
        id_token_email: email,
    })?;
    Ok((format!("{KEYRING_PREFIX}{}", STANDARD.encode(&json)), json))
}

/// The slot value a seating replaced, so it can be put back.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Backup {
    /// Epoch seconds.
    saved_at: i64,
    /// The account the value signs in, when it could be read.
    account: Option<String>,
    /// The Keychain value, byte for byte.
    value: String,
}

impl Backup {
    fn load(path: &Path) -> Result<Option<Self>> {
        match read_if_present(path)? {
            Some(text) => {
                Ok(Some(serde_json::from_str(&text).with_context(|| {
                    format!("could not parse {}", path.display())
                })?))
            }
            None => Ok(None),
        }
    }

    fn save(&self, path: &Path) -> Result<()> {
        let mut bytes = serde_json::to_vec_pretty(self)?;
        bytes.push(b'\n');
        write_atomic(path, &bytes, 0o600)
    }
}

/// What one write of the slot did.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Seated {
    /// The account `agy` now signs in as.
    email: String,
    /// The account it held before, when it held one.
    previous: Option<String>,
    /// `false` when the slot already held this grant and nothing was written.
    changed: bool,
    /// The slot read empty right after the write and was written again.
    rewritten: bool,
}

fn require_macos() -> Result<()> {
    if cfg!(target_os = "macos") {
        Ok(())
    } else {
        bail!("agy keeps its login in the macOS Keychain; seating an account is macOS-only")
    }
}

/// What to put in the slot.
#[derive(Debug, Clone, Copy)]
pub enum UseTarget<'a> {
    /// This Antigravity account's grant, freshly refreshed.
    Account(&'a ServiceAccount),
    /// The value the last switch replaced, byte for byte.
    Previous,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct UseOptions {
    /// Write even while an interactive `agy` runs (it will undo the switch).
    pub force: bool,
    /// Look and report; write nothing and tell no client.
    pub dry_run: bool,
}

/// What happened to the slot. With [`UseReport::dry_run`] set, what would.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum UseOutcome {
    /// The slot was written.
    Written,
    /// The slot already held this login.
    Unchanged,
    /// An interactive `agy` is running and `--force` was not given.
    Refused,
}

/// What became of one running client.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "result", rename_all = "snake_case")]
pub enum ClientResult {
    /// The hub read the slot again and now answers `account`.
    Told { account: String },
    /// The hub could not be told, or still answers another account.
    NotTold { reason: String },
    /// Dry run: the hub would be asked to read the slot again.
    WouldTell,
    /// It is already on the account the slot holds.
    OnTarget,
    /// An `agy acp` server on another account: its editor has to restart it,
    /// or it writes `account` back at its next token refresh.
    HoldsOld { account: String },
    /// It could not be asked which account it holds.
    Unknown { reason: String },
    /// An interactive `agy`: the reason the switch was refused.
    Blocks,
    /// An interactive `agy` the switch was forced past (dry run: would be):
    /// it writes its own account back at its next token refresh.
    Forced,
    /// A print-mode run; the next one signs in from the slot.
    FollowsSlot,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ClientReport {
    pub kind: ClientKind,
    pub pid: u32,
    pub cwd: Option<PathBuf>,
    /// The account it held before the switch, when it could be asked.
    pub account: Option<String>,
    /// An interactive `agy`: the conversation its history last recorded for
    /// its working directory, which `agy --conversation=<id>` resumes.
    pub conversation: Option<String>,
    #[serde(flatten)]
    pub result: ClientResult,
}

/// What `tokeman agy use` did, or with `dry_run` would do.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct UseReport {
    /// The account the slot holds after a write, or would hold.
    pub email: String,
    /// The account the slot held before.
    pub previous: Option<String>,
    pub outcome: UseOutcome,
    pub dry_run: bool,
    /// The slot read empty right after the write and was written a second time.
    pub rewritten: bool,
    /// Why the running clients could not be listed (only with `--force` or
    /// a dry run; otherwise that is an error).
    pub census_error: Option<String>,
    pub clients: Vec<ClientReport>,
}

impl UseReport {
    /// Nothing was written because an interactive `agy` is running.
    pub fn refused(&self) -> bool {
        self.outcome == UseOutcome::Refused
    }

    /// What to tell the user, one line each.
    pub fn lines(&self) -> Vec<String> {
        let email = &self.email;
        let mut lines = Vec::new();
        let was = |text: &str| match &self.previous {
            Some(previous) => format!(" ({text} {previous})"),
            None => String::new(),
        };
        match (self.outcome, self.dry_run) {
            (UseOutcome::Refused, _) => {
                let blocking = self
                    .clients
                    .iter()
                    .filter(|client| client.result == ClientResult::Blocks)
                    .count();
                lines.push(format!(
                    "agy's login was not changed{}: {blocking} interactive agy session(s) are running. A running agy keeps its account and writes it back over the login at its next token refresh (within the hour), so the switch to {email} would be undone.",
                    if self.dry_run { " (dry run)" } else { "" }
                ));
            }
            (UseOutcome::Unchanged, _) => {
                lines.push(format!("agy is already signed in as {email}"))
            }
            (UseOutcome::Written, true) => lines.push(format!(
                "dry run: agy would sign in as {email}{}; nothing was written",
                was("now")
            )),
            (UseOutcome::Written, false) => lines.push(match &self.previous {
                Some(previous) => format!(
                    "agy now signs in as {email} (was {previous}; `tokeman agy use --previous` puts it back)"
                ),
                None => format!("agy now signs in as {email}"),
            }),
        }
        if self.rewritten {
            lines.push(
                "the login read empty right after the write (a logged-out agy removes it as it exits) and was written again"
                    .into(),
            );
        }
        if let Some(error) = &self.census_error {
            lines.push(format!(
                "warning: the running agy clients could not be listed ({error}); one that is running keeps its account and may write it back"
            ));
        }
        for client in &self.clients {
            let who = match &client.cwd {
                Some(cwd) => format!(
                    "{} (pid {}, in {})",
                    client.kind.label(),
                    client.pid,
                    paths::tilde(cwd)
                ),
                None => format!("{} (pid {})", client.kind.label(), client.pid),
            };
            let held = client.account.as_deref().unwrap_or("another account");
            let resume = match &client.conversation {
                Some(id) => format!("`agy --conversation={id}`"),
                None => "`agy -c`".to_owned(),
            };
            lines.push(match &client.result {
                ClientResult::Told { account } => format!("{who}: now on {account}"),
                ClientResult::NotTold { reason } => format!(
                    "warning: {who} was not switched: {reason}. Restart the app if it stays on {held}."
                ),
                ClientResult::WouldTell => {
                    format!("{who}: on {held}; would be told to read the new login")
                }
                ClientResult::OnTarget => format!("{who}: already on {email}"),
                ClientResult::HoldsOld { account } => format!(
                    "warning: {who} still holds {account}. Restart the agent from its editor; until then it writes {account} back as agy's login at its next token refresh."
                ),
                ClientResult::Unknown { reason } => format!(
                    "{who}: could not be asked which account it holds ({reason}). If it is not on {email}, restart it, or it writes its account back at its next token refresh."
                ),
                ClientResult::Blocks => format!(
                    "  {who}: quit it (Ctrl-C twice), run this command again, then resume with {resume}"
                ),
                ClientResult::Forced => format!(
                    "warning: {who} keeps its account and writes it back at its next token refresh, so the switch {} only until then. Quit it (Ctrl-C twice), run this command again, then resume with {resume}.",
                    if self.dry_run { "would last" } else { "lasts" }
                ),
                ClientResult::FollowsSlot => {
                    format!("{who}: a print-mode run; the next one signs in as {email}")
                }
            });
        }
        if self.refused() {
            lines.push(
                "Or pass --force to switch anyway; the switch then lasts only until one of them refreshes its token."
                    .into(),
            );
        }
        lines
    }
}

/// Make `agy` sign in as `target`, and tell the running clients that can be
/// told. Refuses (`Ok` with [`UseReport::refused`]) while an interactive
/// `agy` runs, unless `options.force`. Nothing is ever logged out.
pub async fn use_account(
    client: &Client,
    target: UseTarget<'_>,
    options: UseOptions,
) -> Result<UseReport> {
    require_macos()?;
    use_at(client, Place::live(), target, options).await
}

/// The slot value to write and the grant it carries.
#[derive(Debug, Clone)]
struct Prepared {
    target: Slot,
    value: String,
    /// The fallback file's text; `None` decodes it from `value`.
    file_text: Option<String>,
}

/// Build the value for `target`. An account's grant is refreshed first, so a
/// dead one is refused before anything is written.
async fn prepare(client: &Client, place: &Place, target: UseTarget<'_>) -> Result<Prepared> {
    let account = match target {
        UseTarget::Account(account) => account,
        UseTarget::Previous => return prepare_previous(place),
    };
    if account.service != Service::Antigravity {
        bail!(
            "'{}' is a {} account; only Antigravity accounts can be seated in agy",
            account.name,
            account.service.label()
        );
    }
    let refresh_token = account
        .refresh_token
        .clone()
        .filter(|token| !token.is_empty())
        .with_context(|| {
            format!(
                "'{}' has no refresh token; run `tokeman agy import` again",
                account.name
            )
        })?;
    let grant = grant::exchange(client, &place.token_url, &refresh_token)
        .await
        .map_err(|error| anyhow::anyhow!("{error}"))?;
    let email = grant
        .email()
        .or_else(|| account.name.contains('@').then(|| account.name.clone()))
        .with_context(|| format!("Google did not say which account '{}' is", account.name))?;
    let (value, file_text) = slot_values(&grant, &refresh_token, &email, Local::now())?;
    Ok(Prepared {
        target: Slot {
            email,
            refresh_token,
        },
        value,
        file_text: Some(file_text),
    })
}

/// The value the last switch replaced. Writing it makes the value it
/// replaces the new backup, so repeating this alternates between the two.
fn prepare_previous(place: &Place) -> Result<Prepared> {
    let backup = Backup::load(&place.backup)?
        .context("no earlier agy login is saved; `tokeman agy use <name>` saves one")?;
    let target = Slot::parse(&backup.value)?.context("the saved agy login holds no account")?;
    Ok(Prepared {
        target,
        value: backup.value,
        file_text: None,
    })
}

async fn use_at(
    client: &Client,
    place: Place,
    target: UseTarget<'_>,
    options: UseOptions,
) -> Result<UseReport> {
    let prepared = prepare(client, &place, target).await?;
    let email = prepared.target.email.clone();
    let (running, census_error) = match clients::census(&place.lookout).await {
        Ok(running) => (running, None),
        Err(error) if options.force || options.dry_run => (Vec::new(), Some(format!("{error:#}"))),
        Err(error) => {
            return Err(error.context(
                "could not list the running agy clients, and one that is running would undo the switch; pass --force to write anyway",
            ));
        }
    };
    let history = place.lookout.history.clone();
    let security = place.security.clone();
    let current = tokio::task::spawn_blocking(move || read_raw(&security))
        .await??
        .and_then(|raw| Slot::parse(&raw).ok().flatten());
    let previous = current.as_ref().map(|slot| slot.email.clone());
    let already = current.as_ref() == Some(&prepared.target);

    // Which account each client that can be asked holds now.
    let mut held = Vec::with_capacity(running.len());
    for client in &running {
        held.push(match client.kind {
            ClientKind::Hub | ClientKind::Acp => Some(clients::account(client).await),
            ClientKind::Interactive | ClientKind::Print => None,
        });
    }
    let entry =
        |client: &Running, held: &Option<Result<String>>, result: ClientResult| ClientReport {
            kind: client.kind,
            pid: client.pid,
            cwd: client.cwd.clone(),
            account: held.as_ref().and_then(|held| held.as_ref().ok().cloned()),
            conversation: match (client.kind, &client.cwd) {
                (ClientKind::Interactive, Some(cwd)) => clients::latest_conversation(&history, cwd),
                _ => None,
            },
            result,
        };
    let blocked = !already
        && !options.force
        && running
            .iter()
            .any(|client| client.kind == ClientKind::Interactive);
    // What stands for a client without touching it.
    let untouched = |client: &Running, held: &Option<Result<String>>| match (client.kind, held) {
        (ClientKind::Print, _) => ClientResult::FollowsSlot,
        (ClientKind::Interactive, _) if blocked => ClientResult::Blocks,
        (ClientKind::Interactive, _) if already => ClientResult::Unknown {
            reason: "an interactive agy has no way to be asked".into(),
        },
        (ClientKind::Interactive, _) => ClientResult::Forced,
        (_, Some(Ok(account))) if *account == email => ClientResult::OnTarget,
        (ClientKind::Hub, _) => ClientResult::WouldTell,
        (ClientKind::Acp, Some(Ok(account))) => ClientResult::HoldsOld {
            account: account.clone(),
        },
        (ClientKind::Acp, Some(Err(error))) => ClientResult::Unknown {
            reason: format!("{error:#}"),
        },
        (ClientKind::Acp, None) => ClientResult::Unknown {
            reason: "not asked".into(),
        },
    };
    let mut report = UseReport {
        email: email.clone(),
        previous,
        outcome: match (blocked, already) {
            (true, _) => UseOutcome::Refused,
            (false, true) => UseOutcome::Unchanged,
            (false, false) => UseOutcome::Written,
        },
        dry_run: options.dry_run,
        rewritten: false,
        census_error,
        clients: Vec::new(),
    };
    if blocked || options.dry_run {
        report.clients = running
            .iter()
            .zip(&held)
            .map(|(client, held)| entry(client, held, untouched(client, held)))
            .collect();
        return Ok(report);
    }

    let seated = tokio::task::spawn_blocking(move || install(&place, &prepared)).await??;
    report.previous = seated.previous;
    report.rewritten = seated.rewritten;
    report.outcome = if seated.changed {
        UseOutcome::Written
    } else {
        UseOutcome::Unchanged
    };
    for (client, held) in running.iter().zip(&held) {
        let result = match untouched(client, held) {
            // The hub follows the slot once it is asked for its auth status.
            ClientResult::WouldTell => match clients::reread(client).await {
                Err(error) => ClientResult::NotTold {
                    reason: format!("{error:#}"),
                },
                Ok(()) => match clients::account(client).await {
                    Ok(account) if account == email => ClientResult::Told { account },
                    Ok(account) => ClientResult::NotTold {
                        reason: format!("it read the login again and still answers {account}"),
                    },
                    Err(error) => ClientResult::NotTold {
                        reason: format!("it read the login again, then {error:#}"),
                    },
                },
            },
            other => other,
        };
        report.clients.push(entry(client, held, result));
    }
    Ok(report)
}

/// Back up the slot, write the prepared value, and confirm it by reading it
/// back. A slot that reads empty right after the write (a logged-out `agy`
/// deletes it as it exits) is written once more.
fn install(place: &Place, prepared: &Prepared) -> Result<Seated> {
    let _lock = FileLock::acquire_announced(&place.lock, "another tokeman seating an agy login")?;
    let value = prepared.value.as_str();
    let target = &prepared.target;
    let current = read_raw(&place.security)?;
    let previous = current
        .as_deref()
        .and_then(|raw| Slot::parse(raw).ok().flatten());
    let previous_email = previous.as_ref().map(|slot| slot.email.clone());
    if previous.as_ref() == Some(target) {
        return Ok(Seated {
            email: target.email.clone(),
            previous: previous_email,
            changed: false,
            rewritten: false,
        });
    }
    if let Some(raw) = &current {
        Backup {
            saved_at: chrono::Utc::now().timestamp(),
            account: previous_email.clone(),
            value: raw.clone(),
        }
        .save(&place.backup)?;
    }
    write_raw(&place.security, value)?;
    let mut kept = read_raw(&place.security)?;
    let rewritten = kept.is_none();
    if rewritten {
        write_raw(&place.security, value)?;
        kept = read_raw(&place.security)?;
    }
    if kept.as_deref() != Some(value) {
        // Leave agy with the login it had rather than an unknown one.
        if let Some(raw) = &current {
            let _ = write_raw(&place.security, raw);
        }
        bail!("the Keychain did not keep the new agy login; the earlier one was put back");
    }
    // agy reads this file when the Keychain is slow to answer; one that names
    // another account would sign it back in as that account.
    if place.token_file.exists() {
        let text = match &prepared.file_text {
            Some(text) => text.clone(),
            None => String::from_utf8(
                STANDARD
                    .decode(value.strip_prefix(KEYRING_PREFIX).unwrap_or(value))
                    .context("the saved agy login is not go-keyring base64")?,
            )?,
        };
        write_atomic(&place.token_file, text.as_bytes(), 0o600)
            .context("the Keychain was updated, but agy's fallback token file was not")?;
    }
    Ok(Seated {
        email: target.email.clone(),
        previous: previous_email,
        changed: true,
        rewritten,
    })
}

/// The email inside a slot value, for messages; `None` when unreadable.
#[cfg(test)]
fn email_of(raw: &str) -> Option<String> {
    let json = STANDARD.decode(raw.strip_prefix(KEYRING_PREFIX)?).ok()?;
    serde_json::from_slice::<serde_json::Value>(&json)
        .ok()?
        .get("id_token_email")?
        .as_str()
        .map(str::to_owned)
}

#[cfg(all(test, unix))]
mod tests {
    use super::super::clients::tests::{CSRF, Proc, Server, hub_file, lookout, proc};
    use super::*;
    use axum::routing::post;
    use axum::{Form, Json, Router};
    use serde_json::Value;
    use std::os::unix::fs::PermissionsExt;

    /// A stand-in for `/usr/bin/security` that keeps the item in a file
    /// beside it: `find-generic-password … -w` prints it (exit 44 when
    /// absent), `-i` takes `add-generic-password … -w <value>` on stdin and
    /// records every invocation's arguments.
    fn fake_security(dir: &Path, behaviour: &str) -> PathBuf {
        let path = dir.join("security");
        let script = format!(
            r#"#!/bin/sh
dir="{dir}"
echo "$*" >> "$dir/calls"
case "$1" in
  find-generic-password)
    [ -f "$dir/item" ] || exit 44
    cat "$dir/item"; echo ;;
  -i)
    read -r line
    echo "${{line%% -w *}}" >> "$dir/stdin"
    {behaviour}
    printf '%s' "${{line##* -w }}" > "$dir/item" ;;
  *) exit 2 ;;
esac
"#,
            dir = dir.display()
        );
        std::fs::write(&path, script).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    fn id_token(email: &str) -> String {
        let claims = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(serde_json::json!({ "email": email }).to_string());
        format!("header.{claims}.signature")
    }

    /// A token endpoint that answers each refresh token with its account.
    async fn token_server() -> String {
        async fn token(
            Form(form): Form<Vec<(String, String)>>,
        ) -> (reqwest::StatusCode, Json<Value>) {
            let refresh = form
                .iter()
                .find(|(key, _)| key == "refresh_token")
                .map(|(_, value)| value.as_str())
                .unwrap_or_default();
            match refresh {
                "1//a" | "1//b" => (
                    reqwest::StatusCode::OK,
                    Json(serde_json::json!({
                        "access_token": format!("ya29.{}", &refresh[3..]),
                        "expires_in": 3599,
                        "id_token": id_token(&format!("{}@example.com", &refresh[3..])),
                    })),
                ),
                _ => (
                    reqwest::StatusCode::BAD_REQUEST,
                    Json(serde_json::json!({"error": "invalid_grant"})),
                ),
            }
        }
        let base = crate::testing::serve(Router::new().route("/token", post(token))).await;
        format!("{base}/token")
    }

    async fn place(behaviour: &str) -> (Place, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let place = Place {
            security: fake_security(dir.path(), behaviour),
            token_url: token_server().await,
            token_file: dir.path().join("antigravity-oauth-token"),
            backup: dir.path().join("agy-login-backup.json"),
            lock: dir.path().join("agy-seat.lock"),
            lookout: lookout(dir.path(), &[]),
        };
        (place, dir)
    }

    fn account(name: &str, refresh: &str) -> ServiceAccount {
        ServiceAccount {
            name: name.into(),
            service: Service::Antigravity,
            key: None,
            key_file: None,
            refresh_token: Some(refresh.into()),
            access_token: None,
            expires_at: None,
            label: None,
        }
    }

    /// One write with no running client in the way.
    async fn seat_at(client: &Client, place: Place, account: &ServiceAccount) -> Result<Seated> {
        let prepared = prepare(client, &place, UseTarget::Account(account)).await?;
        install(&place, &prepared)
    }

    fn restore(place: &Place) -> Result<Seated> {
        install(place, &prepare_previous(place)?)
    }

    fn item(dir: &Path) -> Option<String> {
        std::fs::read_to_string(dir.join("item")).ok()
    }

    fn calls(dir: &Path) -> String {
        std::fs::read_to_string(dir.join("calls")).unwrap_or_default()
    }

    #[tokio::test]
    async fn seating_writes_agys_own_shape_and_backs_up_what_it_replaced() {
        let (place, dir) = place("").await;
        let client = Client::new();
        let first = seat_at(&client, place.clone(), &account("a@example.com", "1//a"))
            .await
            .unwrap();
        assert_eq!(
            first,
            Seated {
                email: "a@example.com".into(),
                previous: None,
                changed: true,
                rewritten: false,
            }
        );
        // Nothing to back up the first time.
        assert!(!place.backup.exists());
        let raw = item(dir.path()).unwrap();
        let json = String::from_utf8(
            STANDARD
                .decode(raw.strip_prefix(KEYRING_PREFIX).unwrap())
                .unwrap(),
        )
        .unwrap();
        // agy's field order, compact.
        assert!(json.starts_with(r#"{"token":{"access_token":"ya29.a","token_type":"Bearer","refresh_token":"1//a","expiry":""#), "{json}");
        assert!(
            json.ends_with(
                r#""expires_in":3599},"auth_method":"consumer","id_token_email":"a@example.com"}"#
            ),
            "{json}"
        );
        let expiry = serde_json::from_str::<Value>(&json).unwrap()["token"]["expiry"]
            .as_str()
            .and_then(|expiry| DateTime::parse_from_rfc3339(expiry).ok())
            .unwrap();
        let ahead = expiry.timestamp() - chrono::Utc::now().timestamp();
        assert!((3500..=3600).contains(&ahead), "{ahead}");

        let second = seat_at(&client, place.clone(), &account("renamed", "1//b"))
            .await
            .unwrap();
        // The email comes from Google, not from the account's name.
        assert_eq!(second.email, "b@example.com");
        assert_eq!(second.previous.as_deref(), Some("a@example.com"));
        let backup = Backup::load(&place.backup).unwrap().unwrap();
        assert_eq!(backup.value, raw);
        assert_eq!(backup.account.as_deref(), Some("a@example.com"));
        let mode = std::fs::metadata(&place.backup)
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
        assert_eq!(
            email_of(&item(dir.path()).unwrap()).as_deref(),
            Some("b@example.com")
        );
        // The secret went to `security -i` on stdin, never as an argument.
        let calls = calls(dir.path());
        assert!(calls.lines().any(|line| line == "-i"));
        assert!(
            !calls.contains("ya29") && !calls.contains("go-keyring"),
            "{calls}"
        );
    }

    #[tokio::test]
    async fn seating_the_account_agy_already_holds_writes_nothing() {
        let (place, dir) = place("").await;
        let client = Client::new();
        let a = account("a@example.com", "1//a");
        seat_at(&client, place.clone(), &a).await.unwrap();
        let before = item(dir.path());
        let writes = |dir: &Path| calls(dir).lines().filter(|line| *line == "-i").count();
        assert_eq!(writes(dir.path()), 1);
        let again = seat_at(&client, place.clone(), &a).await.unwrap();
        assert!(!again.changed);
        assert_eq!(writes(dir.path()), 1);
        assert_eq!(item(dir.path()), before);
        assert!(!place.backup.exists());
    }

    #[tokio::test]
    async fn the_previous_login_comes_back_byte_for_byte_and_alternates() {
        let (place, dir) = place("").await;
        let client = Client::new();
        seat_at(&client, place.clone(), &account("a@example.com", "1//a"))
            .await
            .unwrap();
        let a_value = item(dir.path()).unwrap();
        seat_at(&client, place.clone(), &account("b@example.com", "1//b"))
            .await
            .unwrap();
        let b_value = item(dir.path()).unwrap();

        let back = restore(&place).unwrap();
        assert_eq!(back.email, "a@example.com");
        assert_eq!(back.previous.as_deref(), Some("b@example.com"));
        assert_eq!(item(dir.path()).unwrap(), a_value);
        let forth = restore(&place).unwrap();
        assert_eq!(forth.email, "b@example.com");
        assert_eq!(item(dir.path()).unwrap(), b_value);
    }

    #[tokio::test]
    async fn restoring_without_a_backup_says_so() {
        let (place, _dir) = place("").await;
        let error = restore(&place).unwrap_err().to_string();
        assert!(error.contains("no earlier agy login"), "{error}");
    }

    #[tokio::test]
    async fn a_dead_grant_or_another_service_is_refused_before_any_keychain_call() {
        let (place, dir) = place("").await;
        let client = Client::new();
        let dead = seat_at(&client, place.clone(), &account("x@example.com", "1//dead")).await;
        assert!(dead.unwrap_err().to_string().contains("sign in again"));
        let mut glm = account("glm", "1//a");
        glm.service = Service::Zai;
        let other = seat_at(&client, place.clone(), &glm)
            .await
            .unwrap_err()
            .to_string();
        assert!(other.contains("only Antigravity accounts"), "{other}");
        let mut keyless = account("a@example.com", "1//a");
        keyless.refresh_token = None;
        assert!(seat_at(&client, place.clone(), &keyless).await.is_err());
        assert_eq!(calls(dir.path()), "");
    }

    #[tokio::test]
    async fn a_write_the_keychain_does_not_keep_puts_the_old_login_back() {
        // This `security` drops every second write.
        let (place, dir) = place(
            r#"n=$(cat "$dir/n" 2>/dev/null || echo 0); n=$((n+1)); echo $n > "$dir/n"; [ $n -eq 2 ] && exit 0"#,
        )
        .await;
        let client = Client::new();
        seat_at(&client, place.clone(), &account("a@example.com", "1//a"))
            .await
            .unwrap();
        let a_value = item(dir.path()).unwrap();
        let error = seat_at(&client, place.clone(), &account("b@example.com", "1//b"))
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("did not keep"), "{error}");
        assert_eq!(item(dir.path()).unwrap(), a_value);
        // The attempt still saved what it was about to replace.
        assert_eq!(Backup::load(&place.backup).unwrap().unwrap().value, a_value);
    }

    #[tokio::test]
    async fn an_existing_fallback_file_follows_the_slot_and_a_missing_one_stays_missing() {
        let (place, dir) = place("").await;
        let client = Client::new();
        seat_at(&client, place.clone(), &account("a@example.com", "1//a"))
            .await
            .unwrap();
        assert!(!place.token_file.exists());

        std::fs::write(
            &place.token_file,
            r#"{"token":{},"id_token_email":"old@example.com"}"#,
        )
        .unwrap();
        seat_at(&client, place.clone(), &account("b@example.com", "1//b"))
            .await
            .unwrap();
        let file = std::fs::read_to_string(&place.token_file).unwrap();
        let slot_json = STANDARD
            .decode(
                item(dir.path())
                    .unwrap()
                    .strip_prefix(KEYRING_PREFIX)
                    .unwrap(),
            )
            .unwrap();
        assert_eq!(file.as_bytes(), slot_json);
        let mode = std::fs::metadata(&place.token_file)
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);

        restore(&place).unwrap();
        let file: Value =
            serde_json::from_str(&std::fs::read_to_string(&place.token_file).unwrap()).unwrap();
        assert_eq!(file["id_token_email"], "a@example.com");
    }

    #[tokio::test]
    async fn the_slot_matches_an_account_by_grant_or_by_email() {
        let (place, _dir) = place("").await;
        assert_eq!(read_raw(&place.security).unwrap(), None);
        seat_at(
            &Client::new(),
            place.clone(),
            &account("a@example.com", "1//a"),
        )
        .await
        .unwrap();
        let slot = Slot::parse(&read_raw(&place.security).unwrap().unwrap())
            .unwrap()
            .unwrap();
        assert_eq!(slot.email, "a@example.com");
        assert!(slot.holds(&account("renamed", "1//a")));
        // The same account signed in again elsewhere has another refresh token.
        assert!(slot.holds(&account("a@example.com", "1//newer")));
        assert!(!slot.holds(&account("b@example.com", "1//b")));
        let mut glm = account("a@example.com", "1//a");
        glm.service = Service::Kimi;
        assert!(!slot.holds(&glm));
    }

    #[test]
    fn a_value_that_could_break_out_of_the_security_command_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let security = fake_security(dir.path(), "");
        for value in [
            "go-keyring-base64:abc def",
            "abc\"; delete-keychain",
            "a\nb",
        ] {
            assert!(write_raw(&security, value).is_err());
        }
        assert_eq!(calls(dir.path()), "");
    }

    #[test]
    fn a_security_that_hangs_is_killed_at_the_bound() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("security");
        std::fs::write(&path, "#!/bin/sh\nexec sleep 60\n").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        let started = Instant::now();
        let error = read_raw(&path).unwrap_err().to_string();
        assert!(error.contains("did not answer"), "{error}");
        assert!(started.elapsed() < SECURITY_TIMEOUT + Duration::from_secs(5));
    }

    #[tokio::test]
    async fn the_write_goes_to_security_on_stdin_and_never_opens_the_item_to_every_app() {
        let (place, dir) = place("").await;
        let client = Client::new();
        for (name, refresh) in [("a@example.com", "1//a"), ("b@example.com", "1//b")] {
            seat_at(&client, place.clone(), &account(name, refresh))
                .await
                .unwrap();
        }
        restore(&place).unwrap();
        // Every argv the child was started with: reads by name, writes as a bare `-i`.
        let argv = calls(dir.path());
        assert!(argv.lines().count() >= 9, "{argv}");
        for line in argv.lines() {
            assert!(
                line == "-i" || line == "find-generic-password -s gemini -a antigravity -w",
                "{line}"
            );
        }
        for secret in ["ya29", "1//", "go-keyring"] {
            assert!(!argv.contains(secret), "{argv}");
        }
        // The command on stdin updates the one item and never passes -A
        // (which would let any application read it without asking).
        let commands = std::fs::read_to_string(dir.path().join("stdin")).unwrap();
        assert_eq!(commands.lines().count(), 3);
        for line in commands.lines() {
            assert_eq!(line, "add-generic-password -U -s gemini -a antigravity");
        }
        assert!(
            !argv
                .split_whitespace()
                .chain(commands.split_whitespace())
                .any(|word| word == "-A")
        );
    }

    /// A place whose slot holds `a@example.com`, with `procs` running.
    async fn place_on_a(behaviour: &str, procs: &[Proc]) -> (Place, tempfile::TempDir) {
        let (mut place, dir) = place(behaviour).await;
        seat_at(
            &Client::new(),
            place.clone(),
            &account("a@example.com", "1//a"),
        )
        .await
        .unwrap();
        place.lookout = lookout(dir.path(), procs);
        (place, dir)
    }

    fn writes(dir: &Path) -> usize {
        calls(dir).lines().filter(|line| *line == "-i").count()
    }

    async fn use_b(place: &Place, options: UseOptions) -> UseReport {
        let b = account("b@example.com", "1//b");
        use_at(
            &Client::new(),
            place.clone(),
            UseTarget::Account(&b),
            options,
        )
        .await
        .unwrap()
    }

    const FORCE: UseOptions = UseOptions {
        force: true,
        dry_run: false,
    };
    const DRY_RUN: UseOptions = UseOptions {
        force: false,
        dry_run: true,
    };

    #[tokio::test]
    async fn an_interactive_agy_refuses_the_switch_and_says_how_to_resume_each() {
        let (place, dir) = place_on_a(
            "",
            &[
                Proc {
                    cwd: Some("/work/repo"),
                    ..proc(20, "agy")
                },
                Proc {
                    cwd: Some("/work/fresh"),
                    ..proc(21, "/Users/x/.local/bin/agy -c")
                },
                proc(22, "agy -p /usage"),
            ],
        )
        .await;
        std::fs::write(
            &place.lookout.history,
            r#"{"display":"hi","timestamp":1794442000000,"workspace":"/work/repo","conversationId":"0a1b2c3d-aaaa-4bbb-8ccc-000000000001"}
{"display":"/usage","timestamp":1794442300000,"workspace":"/work/repo","type":"slash_command"}
"#,
        )
        .unwrap();
        let before = item(dir.path());
        let report = use_b(&place, UseOptions::default()).await;
        assert!(report.refused());
        assert_eq!(report.outcome, UseOutcome::Refused);
        assert_eq!(report.email, "b@example.com");
        assert_eq!(report.previous.as_deref(), Some("a@example.com"));
        // Nothing was written or saved.
        assert_eq!(item(dir.path()), before);
        assert_eq!(writes(dir.path()), 1);
        assert!(!place.backup.exists());
        let results: Vec<_> = report
            .clients
            .iter()
            .map(|client| {
                (
                    client.pid,
                    client.result.clone(),
                    client.conversation.as_deref(),
                )
            })
            .collect();
        assert_eq!(
            results,
            vec![
                (
                    20,
                    ClientResult::Blocks,
                    Some("0a1b2c3d-aaaa-4bbb-8ccc-000000000001")
                ),
                (21, ClientResult::Blocks, None),
                (22, ClientResult::FollowsSlot, None),
            ]
        );
        let lines = report.lines();
        assert!(
            lines[0].contains("was not changed: 2 interactive agy"),
            "{lines:?}"
        );
        assert_eq!(
            lines[1],
            "  interactive agy (pid 20, in /work/repo): quit it (Ctrl-C twice), run this command again, then resume with `agy --conversation=0a1b2c3d-aaaa-4bbb-8ccc-000000000001`"
        );
        // No conversation recorded for that directory: the generic resume.
        assert_eq!(
            lines[2],
            "  interactive agy (pid 21, in /work/fresh): quit it (Ctrl-C twice), run this command again, then resume with `agy -c`"
        );
        assert!(lines.last().unwrap().contains("--force"), "{lines:?}");

        // A dry run says the same and also writes nothing.
        let rehearsal = use_b(&place, DRY_RUN).await;
        assert!(rehearsal.refused() && rehearsal.dry_run);
        assert_eq!(writes(dir.path()), 1);

        // Forced: written, with the warning that it will not last.
        let forced = use_b(&place, FORCE).await;
        assert_eq!(forced.outcome, UseOutcome::Written);
        assert_eq!(
            email_of(&item(dir.path()).unwrap()).as_deref(),
            Some("b@example.com")
        );
        assert_eq!(forced.clients[0].result, ClientResult::Forced);
        let lines = forced.lines();
        assert!(
            lines[0].starts_with(
                "agy now signs in as b@example.com (was a@example.com; `tokeman agy use --previous`"
            ),
            "{lines:?}"
        );
        assert!(
            lines[1]
                .starts_with("warning: interactive agy (pid 20, in /work/repo) keeps its account")
                && lines[1].contains("lasts only until then")
                && lines[1].ends_with(
                    "resume with `agy --conversation=0a1b2c3d-aaaa-4bbb-8ccc-000000000001`."
                ),
            "{lines:?}"
        );

        // The slot already holds the account: nothing to refuse.
        let again = use_b(&place, UseOptions::default()).await;
        assert_eq!(again.outcome, UseOutcome::Unchanged);
        assert!(matches!(
            again.clients[0].result,
            ClientResult::Unknown { .. }
        ));
    }

    #[tokio::test]
    async fn the_hub_is_told_and_confirmed_and_an_acp_server_on_the_old_account_is_reported() {
        let hub = Server::hub("a@example.com", Some("b@example.com"));
        let acp = Server::acp("a@example.com");
        let moved = Server::acp("b@example.com");
        let (hub_port, acp_port, moved_port) =
            (hub.serve().await, acp.serve().await, moved.serve().await);
        let (place, dir) = place_on_a(
            "",
            &[
                proc(
                    10,
                    "/Applications/Antigravity.app/language_server --subclient_type hub",
                ),
                Proc {
                    cwd: Some("/work/editor"),
                    ports: vec![acp_port],
                    ..proc(30, "agy acp")
                },
                Proc {
                    ports: vec![moved_port],
                    ..proc(31, "agy acp")
                },
                proc(32, "agy acp"),
            ],
        )
        .await;
        hub_file(&place.lookout, 10, hub_port);

        // The rehearsal asks who holds what and changes nothing anywhere.
        let rehearsal = use_b(&place, DRY_RUN).await;
        assert_eq!(rehearsal.outcome, UseOutcome::Written);
        assert_eq!(rehearsal.clients[0].result, ClientResult::WouldTell);
        assert_eq!(
            rehearsal.clients[0].account.as_deref(),
            Some("a@example.com")
        );
        assert_eq!(writes(dir.path()), 1);
        assert!(!place.backup.exists());
        assert_eq!(
            email_of(&item(dir.path()).unwrap()).as_deref(),
            Some("a@example.com")
        );
        assert_eq!(hub.methods(), ["GetUserStatus"]);
        assert!(
            rehearsal.lines()[0]
                .starts_with("dry run: agy would sign in as b@example.com (now a@example.com)")
        );

        let report = use_b(&place, UseOptions::default()).await;
        assert_eq!(report.outcome, UseOutcome::Written);
        assert!(!report.refused() && !report.rewritten);
        let results: Vec<_> = report
            .clients
            .iter()
            .map(|client| (client.kind, client.pid, client.result.clone()))
            .collect();
        assert_eq!(
            results[..3],
            [
                (
                    ClientKind::Hub,
                    10,
                    ClientResult::Told {
                        account: "b@example.com".into()
                    }
                ),
                (
                    ClientKind::Acp,
                    30,
                    ClientResult::HoldsOld {
                        account: "a@example.com".into()
                    }
                ),
                (ClientKind::Acp, 31, ClientResult::OnTarget),
            ]
        );
        // An acp server with no port found could not be asked.
        assert!(matches!(results[3].2, ClientResult::Unknown { .. }));
        // Asked, told, asked again; the token went with each call.
        assert_eq!(
            hub.methods(),
            [
                "GetUserStatus",
                "GetUserStatus",
                "GetAuthStatus",
                "GetUserStatus"
            ]
        );
        // An acp server is only ever asked, and nobody is logged out.
        assert_eq!(acp.methods(), ["GetUserStatus", "GetUserStatus"]);
        for server in [&hub, &acp, &moved] {
            assert!(
                server
                    .methods()
                    .iter()
                    .all(|method| method == "GetUserStatus" || method == "GetAuthStatus"),
                "{:?}",
                server.methods()
            );
        }
        let lines = report.lines();
        assert_eq!(lines[1], "Antigravity app (pid 10): now on b@example.com");
        assert!(
            lines[2].starts_with("warning: agy acp (pid 30, in /work/editor) still holds a@example.com. Restart the agent from its editor"),
            "{lines:?}"
        );
        assert_eq!(lines[3], "agy acp (pid 31): already on b@example.com");
        // Neither the token nor a secret is in what is shown or serialized.
        let shown = format!("{lines:?}{}", serde_json::to_string(&report).unwrap());
        assert!(!shown.contains(CSRF) && !shown.contains("ya29") && !shown.contains("1//"));
        assert_eq!(
            serde_json::to_value(&report.clients[0]).unwrap(),
            serde_json::json!({
                "kind": "hub", "pid": 10, "cwd": null, "account": "a@example.com",
                "conversation": null, "result": "told", "account": "b@example.com",
            })
        );
    }

    #[tokio::test]
    async fn a_hub_that_cannot_be_told_is_a_warning_and_the_seating_stands() {
        let failing = Server::hub("a@example.com", Some("b@example.com")).failing_auth();
        let stubborn = Server::hub("a@example.com", None);
        let (failing_port, stubborn_port) = (failing.serve().await, stubborn.serve().await);
        let (place, dir) = place_on_a(
            "",
            &[
                proc(10, "language_server"),
                proc(11, "language_server"),
                proc(12, "language_server"),
            ],
        )
        .await;
        hub_file(&place.lookout, 10, failing_port);
        hub_file(&place.lookout, 11, stubborn_port);
        // A hub whose port no longer answers.
        hub_file(&place.lookout, 12, 2);
        let report = use_b(&place, UseOptions::default()).await;
        assert_eq!(report.outcome, UseOutcome::Written);
        assert_eq!(
            email_of(&item(dir.path()).unwrap()).as_deref(),
            Some("b@example.com")
        );
        let reasons: Vec<String> = report
            .clients
            .iter()
            .map(|client| match &client.result {
                ClientResult::NotTold { reason } => reason.clone(),
                other => panic!("{other:?}"),
            })
            .collect();
        assert!(
            reasons[0].contains("GetAuthStatus") && reasons[0].contains("HTTP 500"),
            "{reasons:?}"
        );
        assert_eq!(
            reasons[1],
            "it read the login again and still answers a@example.com"
        );
        assert!(reasons[2].contains("got no answer"), "{reasons:?}");
        assert!(
            report.lines()[1].starts_with("warning: Antigravity app (pid 10) was not switched")
        );
    }

    #[tokio::test]
    async fn a_slot_that_reads_empty_right_after_the_write_is_written_once_more() {
        // This `security` loses the item on its second write (the first
        // seats a@), the way a logged-out agy deletes it as it exits.
        let (place, dir) = place_on_a(
            r#"n=$(cat "$dir/n" 2>/dev/null || echo 0); n=$((n+1)); echo $n > "$dir/n"; [ $n -eq 2 ] && { rm -f "$dir/item"; exit 0; }"#,
            &[],
        )
        .await;
        let report = use_b(&place, UseOptions::default()).await;
        assert_eq!(report.outcome, UseOutcome::Written);
        assert!(report.rewritten);
        assert_eq!(writes(dir.path()), 3);
        assert_eq!(
            email_of(&item(dir.path()).unwrap()).as_deref(),
            Some("b@example.com")
        );
        assert!(report.lines()[1].contains("was written again"));
    }

    #[tokio::test]
    async fn an_untakeable_census_stops_the_switch_unless_forced() {
        let (mut place, dir) = place_on_a("", &[]).await;
        place.lookout.ps = dir.path().join("missing-ps");
        let b = account("b@example.com", "1//b");
        let error = use_at(
            &Client::new(),
            place.clone(),
            UseTarget::Account(&b),
            UseOptions::default(),
        )
        .await
        .unwrap_err();
        assert!(format!("{error:#}").contains("pass --force"), "{error:#}");
        assert_eq!(writes(dir.path()), 1);
        let forced = use_b(&place, FORCE).await;
        assert_eq!(forced.outcome, UseOutcome::Written);
        assert!(forced.census_error.is_some());
        assert!(
            forced.lines()[1].starts_with("warning: the running agy clients could not be listed")
        );
    }

    #[tokio::test]
    async fn the_previous_login_goes_through_the_same_checks() {
        let (place, dir) = place_on_a("", &[]).await;
        use_b(&place, UseOptions::default()).await;
        let mut busy = place.clone();
        busy.lookout = lookout(dir.path(), &[proc(20, "agy")]);
        let refused = use_at(
            &Client::new(),
            busy,
            UseTarget::Previous,
            UseOptions::default(),
        )
        .await
        .unwrap();
        assert!(refused.refused());
        assert_eq!(refused.email, "a@example.com");
        // The interactive agy has quit.
        lookout(dir.path(), &[]);
        let back = use_at(
            &Client::new(),
            place,
            UseTarget::Previous,
            UseOptions::default(),
        )
        .await
        .unwrap();
        assert_eq!(back.outcome, UseOutcome::Written);
        assert_eq!(back.previous.as_deref(), Some("b@example.com"));
        assert_eq!(
            email_of(&item(dir.path()).unwrap()).as_deref(),
            Some("a@example.com")
        );
    }
}
