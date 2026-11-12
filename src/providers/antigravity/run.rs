//! `tokeman agy run`: an `agy` signed in to one account, side by side with the
//! normal login.
//!
//! `agy` keeps its login in the shared Keychain slot ([`super::seat`]) unless
//! its token storage decides on a file: it does when `SSH_TTY`, `SSH_CLIENT`
//! or `SSH_CONNECTION` is set (agy 1.5.6), and then every load, save,
//! refresh and logout goes to `$HOME/.gemini/antigravity-cli/antigravity-oauth-token`
//! and none to the Keychain. That decision runs under a 500 ms timer and
//! falls back to the Keychain when the timer wins, so the session also gets a
//! `HOME` without `Library`: there `/usr/bin/security`, which agy drives,
//! sees no login keychain, and a fallback cannot reach the slot.
//!
//! The per-account home ([`paths::agy_homes`]) holds that account's token
//! file, conversations and logs, a link to every entry of the real home but
//! `.gemini` and `Library` (so the commands agy runs find the user's git, ssh
//! and toolchain settings), links to what agy only reads from the real
//! `~/.gemini` ([`SHARED`]), and its own copy of the files agy writes
//! ([`SEEDED`]), taken from the real ones the first time. agy saves settings
//! by replacing the file, so a link there would follow the real settings
//! until the session's first save and then silently stop; a copy makes the
//! session's model and trust choices its own from the start and never
//! touches the normal login's. The home is refreshed on every run; only
//! links tokeman made are ever removed.
//!
//! A running session refreshes its access token into its own file. Google
//! does not rotate refresh tokens, so tokeman's stored grant stays valid.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use chrono::Local;
use reqwest::Client;

use super::clients::{self, ClientKind, Lookout};
use super::grant::{self, parse_cli_grant};
use super::seat::slot_values;
use crate::fs::{FileLock, read_if_present, set_mode, write_atomic};
use crate::paths;
use crate::providers::ServiceAccount;
use crate::quota::Service;

/// Entries of the real home a per-account home does not link: agy's own data
/// (the account's login lives there) and `Library` (the login keychain).
const NOT_LINKED: &[&str] = &[".gemini", "Library"];
/// What agy only reads under `~/.gemini`, linked into each per-account home
/// when it exists.
pub const SHARED: &[&str] = &["GEMINI.md", "skills", "config/mcp_config.json"];
/// What agy writes under `~/.gemini` (the model, trusted folders), copied
/// into a per-account home once, when the home has none.
pub const SEEDED: &[&str] = &["antigravity-cli/settings.json"];
const TOKEN_FILE: &str = ".gemini/antigravity-cli/antigravity-oauth-token";

/// The directory name of `account`'s home under [`paths::agy_homes`].
pub fn home_name(account: &str) -> String {
    account
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || "@.+-_".contains(c) {
                c
            } else {
                '_'
            }
        })
        .collect::<String>()
        .trim_start_matches('.')
        .to_owned()
}

/// How the session signs in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Login {
    /// The account is the one the slot holds: plain `agy`.
    Normal,
    /// Its own token file in this per-account home.
    Own { home: PathBuf },
}

/// An `agy` ready to start.
#[derive(Debug)]
pub struct Launch {
    pub account: String,
    pub login: Login,
    program: OsString,
    args: Vec<String>,
    env: Vec<(&'static str, OsString)>,
}

impl Launch {
    pub fn program(&self) -> &std::ffi::OsStr {
        &self.program
    }

    pub fn args(&self) -> &[String] {
        &self.args
    }

    fn command(&self) -> std::process::Command {
        let mut command = std::process::Command::new(&self.program);
        command.args(&self.args);
        for (key, value) in &self.env {
            command.env(key, value);
        }
        command
    }

    /// Replace this process with `agy`, which takes over the terminal and
    /// its signals. Returns only when `agy` could not be started.
    #[cfg(unix)]
    pub fn exec(self) -> anyhow::Error {
        use std::os::unix::process::CommandExt;
        let error = self.command().exec();
        anyhow::Error::new(error)
            .context(format!("could not run {}", self.program.to_string_lossy()))
    }

    /// Run `agy` in this terminal and exit with its status.
    #[cfg(not(unix))]
    pub fn exec(self) -> anyhow::Error {
        match self.command().status() {
            Ok(status) => std::process::exit(status.code().unwrap_or(1)),
            Err(error) => anyhow::Error::new(error)
                .context(format!("could not run {}", self.program.to_string_lossy())),
        }
    }
}

/// Where a run is prepared; tests point every field at a temporary directory.
#[derive(Debug, Clone)]
struct Place {
    agy: OsString,
    token_url: String,
    homes: PathBuf,
    real_home: PathBuf,
    /// An `SSH_TTY` already in the environment, kept as it is.
    ssh_tty: Option<OsString>,
}

impl Place {
    fn live() -> Result<Self> {
        let real_home = paths::home();
        if !real_home.is_absolute() {
            bail!("no home directory");
        }
        let homes = paths::agy_homes();
        if real_home.starts_with(&homes) {
            bail!(
                "this shell already runs on a per-account agy home ({}); run `tokeman agy run` from a shell on your own home",
                real_home.display()
            );
        }
        Ok(Self {
            agy: std::env::var_os("TOKEMAN_AGY_BIN").unwrap_or_else(|| "agy".into()),
            token_url: grant::TOKEN_URL.into(),
            homes,
            real_home,
            ssh_tty: std::env::var_os("SSH_TTY").filter(|tty| !tty.is_empty()),
        })
    }
}

/// Get `agy` ready to run as `account` with `args`. `seated` says the slot
/// already holds it, which needs nothing but plain `agy`.
pub async fn prepare(
    client: &Client,
    account: &ServiceAccount,
    seated: bool,
    args: Vec<String>,
) -> Result<Launch> {
    if !cfg!(target_os = "macos") {
        bail!(
            "`tokeman agy run` is macOS-only: elsewhere a HOME of its own does not keep agy away from the shared login"
        );
    }
    prepare_at(client, &Place::live()?, account, seated, args).await
}

async fn prepare_at(
    client: &Client,
    place: &Place,
    account: &ServiceAccount,
    seated: bool,
    args: Vec<String>,
) -> Result<Launch> {
    if account.service != Service::Antigravity {
        bail!(
            "'{}' is a {} account; only Antigravity accounts run in agy",
            account.name,
            account.service.label()
        );
    }
    if seated {
        return Ok(Launch {
            account: account.name.clone(),
            login: Login::Normal,
            program: place.agy.clone(),
            args,
            env: Vec::new(),
        });
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
    let name = home_name(&account.name);
    if name.is_empty() {
        bail!("'{}' cannot name a directory", account.name);
    }
    let home = place.homes.join(name);
    let token_file = home.join(TOKEN_FILE);
    // A file on this grant is kept: agy refreshes its access token itself.
    let current = read_if_present(&token_file)?
        .and_then(|text| parse_cli_grant(&text).ok().flatten())
        .is_some_and(|held| held.refresh_token.as_deref() == Some(refresh_token.as_str()));
    let token_text = if current {
        None
    } else {
        let grant = grant::exchange(client, &place.token_url, &refresh_token)
            .await
            .map_err(|error| anyhow::anyhow!("{error}"))?;
        let email = grant
            .email()
            .or_else(|| account.name.contains('@').then(|| account.name.clone()))
            .with_context(|| format!("Google did not say which account '{}' is", account.name))?;
        Some(slot_values(&grant, &refresh_token, &email, Local::now())?.1)
    };
    let (furnishing, furnished) = (place.clone(), home.clone());
    tokio::task::spawn_blocking(move || furnish(&furnishing, &furnished, token_text.as_deref()))
        .await??;
    let ssh_tty = place
        .ssh_tty
        .clone()
        .unwrap_or_else(|| OsString::from("/dev/tty"));
    Ok(Launch {
        account: account.name.clone(),
        login: Login::Own { home: home.clone() },
        program: place.agy.clone(),
        args,
        env: vec![("HOME", home.into_os_string()), ("SSH_TTY", ssh_tty)],
    })
}

/// Make `home` current: owner-only directories, the links, and the token file
/// when `token_text` is given. One tokeman at a time per home.
fn furnish(place: &Place, home: &Path, token_text: Option<&str>) -> Result<()> {
    std::fs::create_dir_all(&place.homes)
        .with_context(|| format!("failed to create {}", place.homes.display()))?;
    set_mode(&place.homes, 0o700)?;
    let name = home.file_name().context("a home has a name")?;
    let lock = place.homes.join(format!("{}.lock", name.to_string_lossy()));
    let _lock = FileLock::acquire_announced(&lock, "another tokeman preparing this agy home")?;
    for dir in [
        home.to_path_buf(),
        home.join(".gemini"),
        home.join(".gemini/antigravity-cli"),
    ] {
        std::fs::create_dir_all(&dir)
            .with_context(|| format!("failed to create {}", dir.display()))?;
        set_mode(&dir, 0o700)?;
    }
    link_real_home(&place.real_home, home)?;
    link_shared(&place.real_home.join(".gemini"), &home.join(".gemini"))?;
    seed_copies(&place.real_home.join(".gemini"), &home.join(".gemini"))?;
    if let Some(text) = token_text {
        write_atomic(&home.join(TOKEN_FILE), text.as_bytes(), 0o600)?;
    }
    Ok(())
}

#[cfg(unix)]
fn symlink(target: &Path, link: &Path) -> Result<()> {
    std::os::unix::fs::symlink(target, link)
        .with_context(|| format!("failed to link {}", link.display()))
}

#[cfg(not(unix))]
fn symlink(_target: &Path, link: &Path) -> Result<()> {
    bail!("cannot link {} on this platform", link.display())
}

/// Whether `path` exists, without following a link.
fn present(path: &Path) -> Result<bool> {
    match std::fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error).with_context(|| format!("failed to read {}", path.display())),
    }
}

/// Link every entry of `real` but [`NOT_LINKED`] into `home`, and drop the
/// links tokeman made to entries that are gone. Anything else in `home` is
/// agy's or a tool's and stays.
fn link_real_home(real: &Path, home: &Path) -> Result<()> {
    let entries =
        std::fs::read_dir(real).with_context(|| format!("failed to list {}", real.display()))?;
    for entry in entries {
        let name = entry?.file_name();
        if NOT_LINKED.iter().any(|skip| name == **skip) {
            continue;
        }
        let link = home.join(&name);
        if !present(&link)? {
            symlink(&real.join(&name), &link)?;
        }
    }
    for entry in std::fs::read_dir(home)? {
        let link = entry?.path();
        let Ok(target) = std::fs::read_link(&link) else {
            continue;
        };
        let ours = target.parent() == Some(real);
        let unwanted = target
            .file_name()
            .is_some_and(|name| NOT_LINKED.iter().any(|skip| name == *skip));
        if ours && (unwanted || !present(&target)?) {
            std::fs::remove_file(&link)
                .with_context(|| format!("failed to remove {}", link.display()))?;
        }
    }
    Ok(())
}

/// Link each [`SHARED`] entry of `real_gemini` that exists into `gemini`,
/// where nothing is in its place yet.
fn link_shared(real_gemini: &Path, gemini: &Path) -> Result<()> {
    for relative in SHARED {
        let target = real_gemini.join(relative);
        let link = gemini.join(relative);
        if !present(&target)? || present(&link)? {
            continue;
        }
        if let Some(parent) = link.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("failed to create {}", parent.display()))?;
            set_mode(parent, 0o700)?;
        }
        symlink(&target, &link)?;
    }
    Ok(())
}

/// Copy each [`SEEDED`] file of `real_gemini` that exists into `gemini` (0600)
/// where the home has none. A link to the real file, as earlier builds made,
/// becomes a copy; a file of the home's own is kept.
fn seed_copies(real_gemini: &Path, gemini: &Path) -> Result<()> {
    for relative in SEEDED {
        let source = real_gemini.join(relative);
        let copy = gemini.join(relative);
        if std::fs::read_link(&copy).is_ok_and(|target| target == source) {
            std::fs::remove_file(&copy)
                .with_context(|| format!("failed to remove {}", copy.display()))?;
        }
        if present(&copy)? {
            continue;
        }
        let Some(bytes) = read_if_present(&source)? else {
            continue;
        };
        write_atomic(&copy, bytes.as_bytes(), 0o600)?;
    }
    Ok(())
}

/// An `agy` running on a per-account home, with the account it belongs to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Session {
    pub pid: u32,
    pub kind: ClientKind,
    /// The registered account whose home it runs on; `None` for a home no
    /// registered account has.
    pub account: Option<String>,
    pub home: String,
    pub cwd: Option<PathBuf>,
}

/// The `agy` processes running on per-account homes (macOS; empty
/// elsewhere). Read-only: `ps` and `lsof` with bounded waits.
pub async fn sessions(accounts: &[ServiceAccount]) -> Result<Vec<Session>> {
    if !cfg!(target_os = "macos") {
        return Ok(Vec::new());
    }
    sessions_at(&Lookout::live(), accounts).await
}

async fn sessions_at(lookout: &Lookout, accounts: &[ServiceAccount]) -> Result<Vec<Session>> {
    Ok(clients::sessions(lookout)
        .await?
        .into_iter()
        .map(|session| Session {
            pid: session.pid,
            kind: session.kind,
            account: accounts
                .iter()
                .filter(|account| account.service == Service::Antigravity)
                .find(|account| home_name(&account.name) == session.home)
                .map(|account| account.name.clone()),
            home: session.home,
            cwd: session.cwd,
        })
        .collect())
}

#[cfg(all(test, unix))]
mod tests {
    use super::super::clients::tests::{Proc, lookout, proc};
    use super::super::seat::tests::{account, token_server};
    use super::*;

    struct Fixture {
        dir: tempfile::TempDir,
        place: Place,
    }

    async fn fixture() -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("real");
        for sub in [
            ".gemini/antigravity-cli",
            ".gemini/skills",
            "Library/Keychains",
            ".ssh",
        ] {
            std::fs::create_dir_all(real.join(sub)).unwrap();
        }
        std::fs::write(real.join(".gitconfig"), "[user]\n").unwrap();
        std::fs::write(real.join(".gemini/GEMINI.md"), "rules").unwrap();
        std::fs::write(real.join(".gemini/antigravity-cli/settings.json"), "{}").unwrap();
        std::fs::write(
            real.join(".gemini/antigravity-cli/antigravity-oauth-token"),
            "the normal login",
        )
        .unwrap();
        let place = Place {
            agy: "/opt/agy".into(),
            token_url: token_server().await,
            homes: dir.path().join("homes"),
            real_home: real,
            ssh_tty: None,
        };
        Fixture { dir, place }
    }

    fn env(launch: &Launch) -> Vec<(&str, String)> {
        launch
            .env
            .iter()
            .map(|(key, value)| (*key, value.to_string_lossy().into_owned()))
            .collect()
    }

    fn mode(path: &Path) -> u32 {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    #[tokio::test]
    async fn an_account_gets_a_private_home_with_its_own_token_and_the_users_settings() {
        let f = fixture().await;
        let client = Client::new();
        let launch = prepare_at(
            &client,
            &f.place,
            &account("b@example.com", "1//b"),
            false,
            vec!["models".into()],
        )
        .await
        .unwrap();
        let home = f.place.homes.join("b@example.com");
        assert_eq!(launch.login, Login::Own { home: home.clone() });
        assert_eq!(
            env(&launch),
            vec![
                ("HOME", home.display().to_string()),
                ("SSH_TTY", "/dev/tty".into())
            ]
        );
        assert_eq!(launch.args(), ["models"]);
        assert_eq!(launch.program(), "/opt/agy");

        let token = home.join(TOKEN_FILE);
        let file: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&token).unwrap()).unwrap();
        assert_eq!(file["token"]["access_token"], "ya29.b");
        assert_eq!(file["token"]["refresh_token"], "1//b");
        assert_eq!(file["id_token_email"], "b@example.com");
        assert_eq!(file["auth_method"], "consumer");
        assert_eq!(mode(&token), 0o600);
        for dir in [&f.place.homes, &home, &home.join(".gemini")] {
            assert_eq!(mode(dir), 0o700, "{}", dir.display());
        }

        let real = &f.place.real_home;
        assert_eq!(
            std::fs::read_link(home.join(".gitconfig")).unwrap(),
            real.join(".gitconfig")
        );
        assert_eq!(
            std::fs::read_link(home.join(".ssh")).unwrap(),
            real.join(".ssh")
        );
        // The login keychain and agy's own data stay out of reach.
        assert!(!present(&home.join("Library")).unwrap());
        assert!(std::fs::read_link(home.join(".gemini")).is_err());
        assert_eq!(
            std::fs::read_link(home.join(".gemini/GEMINI.md")).unwrap(),
            real.join(".gemini/GEMINI.md")
        );
        // The settings agy writes are the home's own copy, seeded from the real ones.
        let settings = home.join(".gemini/antigravity-cli/settings.json");
        assert!(std::fs::read_link(&settings).is_err());
        assert_eq!(std::fs::read_to_string(&settings).unwrap(), "{}");
        assert_eq!(mode(&settings), 0o600);
        std::fs::write(&settings, "{\"model\":\"other\"}").unwrap();
        assert_eq!(
            std::fs::read_to_string(real.join(".gemini/antigravity-cli/settings.json")).unwrap(),
            "{}"
        );
        assert_eq!(
            std::fs::read_link(home.join(".gemini/skills")).unwrap(),
            real.join(".gemini/skills")
        );
        // Absent in the real home, so not linked.
        assert!(!present(&home.join(".gemini/config/mcp_config.json")).unwrap());
        // The normal login is not touched.
        assert_eq!(
            std::fs::read_to_string(real.join(".gemini/antigravity-cli/antigravity-oauth-token"))
                .unwrap(),
            "the normal login"
        );
    }

    #[tokio::test]
    async fn a_second_run_keeps_the_file_agy_refreshed_and_follows_the_real_home() {
        let mut f = fixture().await;
        let client = Client::new();
        let b = account("b@example.com", "1//b");
        prepare_at(&client, &f.place, &b, false, vec![])
            .await
            .unwrap();
        let home = f.place.homes.join("b@example.com");
        let token = home.join(TOKEN_FILE);
        // What agy writes after a refresh of the same grant.
        let refreshed = r#"{"token":{"access_token":"ya29.agy","token_type":"Bearer","refresh_token":"1//b","expiry":"2026-11-11T18:00:00-08:00","expires_in":3599},"auth_method":"consumer","id_token_email":"b@example.com"}"#;
        std::fs::write(&token, refreshed).unwrap();
        // Entries agy and its tools made in the home are theirs, a link
        // that leads nowhere included.
        std::fs::write(home.join(".zsh_history"), "ls").unwrap();
        let elsewhere = f.dir.path().join("gone-tool-cache");
        std::os::unix::fs::symlink(&elsewhere, home.join(".tool-cache")).unwrap();
        std::fs::remove_file(home.join(".gemini/antigravity-cli/settings.json")).unwrap();
        std::fs::write(
            home.join(".gemini/antigravity-cli/settings.json"),
            "{\"model\":1}",
        )
        .unwrap();
        // The real home changed meanwhile.
        let real = f.place.real_home.clone();
        std::fs::remove_file(real.join(".gitconfig")).unwrap();
        std::fs::create_dir(real.join(".cargo")).unwrap();
        // A grant that no longer answers is never asked: the file is current.
        f.place.token_url = "http://127.0.0.1:9/token".into();

        prepare_at(&client, &f.place, &b, false, vec![])
            .await
            .unwrap();
        assert_eq!(std::fs::read_to_string(&token).unwrap(), refreshed);
        assert!(!present(&home.join(".gitconfig")).unwrap());
        assert_eq!(
            std::fs::read_link(home.join(".cargo")).unwrap(),
            real.join(".cargo")
        );
        assert_eq!(
            std::fs::read_to_string(home.join(".zsh_history")).unwrap(),
            "ls"
        );
        assert_eq!(
            std::fs::read_link(home.join(".tool-cache")).unwrap(),
            elsewhere
        );
        assert_eq!(
            std::fs::read_to_string(home.join(".gemini/antigravity-cli/settings.json")).unwrap(),
            "{\"model\":1}"
        );
    }

    #[tokio::test]
    async fn settings_are_seeded_once_and_an_earlier_link_to_the_real_file_becomes_a_copy() {
        let f = fixture().await;
        let client = Client::new();
        let b = account("b@example.com", "1//b");
        let real = f
            .place
            .real_home
            .join(".gemini/antigravity-cli/settings.json");
        let home = f.place.homes.join("b@example.com");
        let settings = home.join(".gemini/antigravity-cli/settings.json");
        // A home an earlier build made: settings linked to the real file.
        std::fs::create_dir_all(settings.parent().unwrap()).unwrap();
        std::os::unix::fs::symlink(&real, &settings).unwrap();
        std::fs::write(&real, r#"{"model":"normal"}"#).unwrap();

        prepare_at(&client, &f.place, &b, false, vec![])
            .await
            .unwrap();
        assert!(std::fs::read_link(&settings).is_err());
        assert_eq!(
            std::fs::read_to_string(&settings).unwrap(),
            r#"{"model":"normal"}"#
        );

        // The normal login changes its model; the session keeps its own.
        std::fs::write(&real, r#"{"model":"changed"}"#).unwrap();
        prepare_at(&client, &f.place, &b, false, vec![])
            .await
            .unwrap();
        assert_eq!(
            std::fs::read_to_string(&settings).unwrap(),
            r#"{"model":"normal"}"#
        );
        // What agy only reads stays linked.
        assert_eq!(
            std::fs::read_link(home.join(".gemini/GEMINI.md")).unwrap(),
            f.place.real_home.join(".gemini/GEMINI.md")
        );
    }

    #[tokio::test]
    async fn a_file_on_another_grant_is_replaced_and_a_dead_grant_is_refused_first() {
        let f = fixture().await;
        let client = Client::new();
        let home = f.place.homes.join("b@example.com");
        std::fs::create_dir_all(home.join(".gemini/antigravity-cli")).unwrap();
        std::fs::write(
            home.join(TOKEN_FILE),
            r#"{"token":{"refresh_token":"1//old"},"id_token_email":"b@example.com"}"#,
        )
        .unwrap();
        prepare_at(
            &client,
            &f.place,
            &account("b@example.com", "1//b"),
            false,
            vec![],
        )
        .await
        .unwrap();
        let file: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(home.join(TOKEN_FILE)).unwrap()).unwrap();
        assert_eq!(file["token"]["refresh_token"], "1//b");

        let error = prepare_at(
            &client,
            &f.place,
            &account("dead", "1//dead"),
            false,
            vec![],
        )
        .await
        .unwrap_err();
        assert!(format!("{error:#}").contains("revoked"), "{error:#}");
        assert!(!present(&f.place.homes.join("dead")).unwrap());
    }

    #[tokio::test]
    async fn the_account_agy_already_holds_runs_plain_agy() {
        let f = fixture().await;
        let launch = prepare_at(
            &Client::new(),
            &f.place,
            &account("a@example.com", "1//a"),
            true,
            vec!["-c".into()],
        )
        .await
        .unwrap();
        assert_eq!(launch.login, Login::Normal);
        assert!(launch.env.is_empty());
        assert_eq!(launch.args(), ["-c"]);
        assert!(!present(&f.place.homes).unwrap());
    }

    #[tokio::test]
    async fn an_ssh_tty_already_set_is_kept_and_other_services_are_refused() {
        let mut f = fixture().await;
        f.place.ssh_tty = Some("/dev/ttys004".into());
        let launch = prepare_at(
            &Client::new(),
            &f.place,
            &account("b@example.com", "1//b"),
            false,
            vec![],
        )
        .await
        .unwrap();
        assert_eq!(env(&launch)[1], ("SSH_TTY", "/dev/ttys004".into()));
        let glm = ServiceAccount {
            service: Service::Zai,
            ..account("glm", "1//b")
        };
        assert!(
            prepare_at(&Client::new(), &f.place, &glm, false, vec![])
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn the_started_agy_gets_exactly_the_home_and_tty_and_the_arguments() {
        let f = fixture().await;
        let probe = f.dir.path().join("agy");
        std::fs::write(
            &probe,
            "#!/bin/sh\nprintf '%s|%s|%s\\n' \"$HOME\" \"$SSH_TTY\" \"$*\"\n",
        )
        .unwrap();
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&probe, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let place = Place {
            agy: probe.into(),
            ..f.place.clone()
        };
        let launch = prepare_at(
            &Client::new(),
            &place,
            &account("b@example.com", "1//b"),
            false,
            vec!["models".into(), "--x".into()],
        )
        .await
        .unwrap();
        let output = launch.command().output().unwrap();
        let home = place.homes.join("b@example.com");
        assert_eq!(
            String::from_utf8(output.stdout).unwrap(),
            format!("{}|/dev/tty|models --x\n", home.display())
        );
    }

    #[tokio::test]
    async fn sessions_name_the_account_whose_home_they_run_on() {
        let dir = tempfile::tempdir().unwrap();
        let lookout = lookout(
            dir.path(),
            &[
                Proc {
                    cwd: Some("/work"),
                    ..proc(20, "agy")
                },
                proc(30, "agy"),
                proc(40, "agy"),
            ],
        );
        let homes = lookout.homes.display().to_string();
        std::fs::write(
            dir.path().join("ps-args"),
            format!(
                "   20 agy HOME={homes}/b@example.com\n   30 agy HOME={homes}/gone\n   40 agy HOME=/real\n"
            ),
        )
        .unwrap();
        let accounts = [
            account("a@example.com", "1//a"),
            account("b@example.com", "1//b"),
        ];
        let sessions = sessions_at(&lookout, &accounts).await.unwrap();
        assert_eq!(
            sessions
                .iter()
                .map(|s| (s.pid, s.account.clone(), s.home.as_str(), s.cwd.clone()))
                .collect::<Vec<_>>(),
            vec![
                (
                    20,
                    Some("b@example.com".into()),
                    "b@example.com",
                    Some("/work".into())
                ),
                (30, None, "gone", None),
            ]
        );
        assert_eq!(home_name("we/ird name"), "we_ird_name");
        assert_eq!(home_name("..up"), "up");
    }
}
