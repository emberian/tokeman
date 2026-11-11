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
//! Seating is manual (`tokeman services use`); the daemon never does it.

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

use super::antigravity::{self, Grant};
use super::{Service, ServiceAccount};
use crate::fs::{FileLock, read_if_present, write_atomic};
use crate::paths;

const KEYCHAIN_SERVICE: &str = "gemini";
const KEYCHAIN_ACCOUNT: &str = "antigravity";
const KEYRING_PREFIX: &str = "go-keyring-base64:";
const SECURITY_TIMEOUT: Duration = Duration::from_secs(if cfg!(test) { 2 } else { 10 });
const PS_TIMEOUT: Duration = Duration::from_secs(5);

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
}

impl Place {
    fn live() -> Self {
        Self {
            security: security_bin(),
            token_url: antigravity::TOKEN_URL.into(),
            token_file: token_file(),
            backup: paths::agy_login_backup(),
            lock: paths::agy_seat_lock(),
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
        Ok(antigravity::parse_cli_grant(raw)?.map(|grant| Self {
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

/// The outcome of a seating.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Seated {
    /// The account `agy` now signs in as.
    pub email: String,
    /// The account it held before, when it held one.
    pub previous: Option<String>,
    /// `false` when the slot already held this grant and nothing was written.
    pub changed: bool,
}

fn require_macos() -> Result<()> {
    if cfg!(target_os = "macos") {
        Ok(())
    } else {
        bail!("agy keeps its login in the macOS Keychain; seating an account is macOS-only")
    }
}

/// Make `account` the one `agy` signs in as. The grant is refreshed first,
/// so a dead one is refused before anything is written.
pub async fn seat(client: &Client, account: &ServiceAccount) -> Result<Seated> {
    require_macos()?;
    seat_at(client, Place::live(), account).await
}

async fn seat_at(client: &Client, place: Place, account: &ServiceAccount) -> Result<Seated> {
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
        .with_context(|| format!("'{}' has no refresh token; import it again", account.name))?;
    let grant = antigravity::exchange(client, &place.token_url, &refresh_token).await?;
    let email = grant
        .email()
        .or_else(|| account.name.contains('@').then(|| account.name.clone()))
        .with_context(|| format!("Google did not say which account '{}' is", account.name))?;
    let (value, file_text) = slot_values(&grant, &refresh_token, &email, Local::now())?;
    tokio::task::spawn_blocking(move || {
        let target = Slot {
            email,
            refresh_token,
        };
        install(&place, Some(&target), &value, Some(&file_text))
    })
    .await?
}

/// Put back the value the last seating replaced. The value it replaces
/// becomes the new backup, so repeating this alternates between the two.
pub async fn seat_previous() -> Result<Seated> {
    require_macos()?;
    let place = Place::live();
    tokio::task::spawn_blocking(move || restore(&place)).await?
}

fn restore(place: &Place) -> Result<Seated> {
    let backup = Backup::load(&place.backup)?
        .context("no earlier agy login is saved; `tokeman services use <name>` saves one")?;
    install(place, None, &backup.value, None)
}

/// Back up the slot, write `value`, and confirm it by reading it back.
/// `target` is the grant `value` carries when tokeman built it.
fn install(
    place: &Place,
    target: Option<&Slot>,
    value: &str,
    file_text: Option<&str>,
) -> Result<Seated> {
    let _lock = FileLock::acquire_announced(&place.lock, "another tokeman seating an agy login")?;
    let current = read_raw(&place.security)?;
    let previous = current
        .as_deref()
        .and_then(|raw| Slot::parse(raw).ok().flatten());
    let target = match target {
        Some(target) => target.clone(),
        None => Slot::parse(value)?.context("the saved agy login holds no account")?,
    };
    let previous_email = previous.as_ref().map(|slot| slot.email.clone());
    if previous.as_ref() == Some(&target) {
        return Ok(Seated {
            email: target.email,
            previous: previous_email,
            changed: false,
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
    if read_raw(&place.security)?.as_deref() != Some(value) {
        // Leave agy with the login it had rather than an unknown one.
        if let Some(raw) = &current {
            let _ = write_raw(&place.security, raw);
        }
        bail!("the Keychain did not keep the new agy login; the earlier one was put back");
    }
    // agy reads this file when the Keychain is slow to answer; one that names
    // another account would sign it back in as that account.
    if place.token_file.exists() {
        let text = match file_text {
            Some(text) => text.to_owned(),
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
        email: target.email,
        previous: previous_email,
        changed: true,
    })
}

/// How many `agy` processes are running. Each keeps the account it started
/// with and writes it back into the slot when it next refreshes its token.
pub async fn running() -> usize {
    crate::claude::sessions::ps(&["-axo", "comm="], PS_TIMEOUT)
        .await
        .map(|census| count_agy(&census))
        .unwrap_or(0)
}

fn count_agy(census: &str) -> usize {
    census
        .lines()
        .filter(|line| {
            let command = line.trim();
            command == "agy" || command.ends_with("/agy")
        })
        .count()
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
                changed: true
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

    #[test]
    fn only_agy_itself_counts_as_a_running_agy() {
        let census = "/Users/x/.local/bin/agy\nagy\n/Applications/Antigravity.app/Contents/MacOS/Antigravity\n/usr/bin/magy\n/opt/agy/bin/node\n";
        assert_eq!(count_agy(census), 2);
    }
}
