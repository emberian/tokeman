//! Google Antigravity quota, read from the Cloud Code private API the `agy`
//! CLI and the Antigravity app use for their Model Quota panel.
//!
//! - Credentials: a Google OAuth grant from Antigravity's installed-app
//!   client. Google does not rotate the refresh token, so tokeman keeps its own
//!   copy, refreshes as often as it likes, and never writes back to `agy`'s
//!   Keychain item or the app's `state.vscdb`. Access tokens are cached in
//!   [`paths::antigravity_tokens`], which only this module writes.
//! - Plan: `loadCodeAssist` → `paidTier.id` (`currentTier` says `free-tier`
//!   even for Ultra), plus the project the quota calls name.
//! - Quota: `retrieveUserQuotaSummary` → groups ("Gemini Models",
//!   "Claude and GPT models") of buckets with a `5h` and a `weekly` window.
//!   The server returns the Gemini group only to an `antigravity/hub/` client
//!   of version 2.8 or later.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use chrono::DateTime;
use reqwest::{Client, StatusCode, header};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

use super::{Meter, Service, ServiceAccount, Usage};
use crate::fs::{FileLock, read_if_present, write_atomic};
use crate::paths;
use crate::quota::Window;

const API: &str = "https://daily-cloudcode-pa.googleapis.com";
const TOKEN_URL: &str = "https://oauth2.googleapis.com/token";
/// Antigravity's installed-app OAuth client, as shipped in `agy` and the
/// app's `language_server`; both sign-ins (`auth_method: consumer`) use it.
const CLIENT_ID: &str = "1071006060591-tmhssin2h21lcre235vtolojh4g403ep.apps.googleusercontent.com";
const CLIENT_SECRET: &str = "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";
/// The hub version announced when no newer Antigravity app is installed. The
/// server leaves the Gemini group out for hub versions below 2.8.
const PINNED_HUB_VERSION: &str = "2.28.0";
const MIN_GEMINI_HUB_VERSION: (u32, u32, u32) = (2, 8, 0);
/// The project consumer accounts share, used when `loadCodeAssist` is down.
const CONSUMER_PROJECT: &str = "aicode-consumers";
const REQUEST_TIMEOUT: Duration = Duration::from_secs(20);
/// Refresh this long before Google's expiry.
const EXPIRY_MARGIN_MS: i64 = 60_000;
/// An unused window reports `resetTime = now + window`; a reset this close to
/// that is a window that has not started.
const IDLE_RESET_TOLERANCE_SECS: i64 = 300;

const FIVE_HOURS: i64 = 5 * 3600;
const WEEK: i64 = 7 * 86_400;

/// Where requests go; tests point these at a loopback server.
#[derive(Debug, Clone)]
struct Endpoints {
    api: String,
    token_url: String,
    cache: PathBuf,
}

impl Endpoints {
    fn live() -> Self {
        Self {
            api: API.into(),
            token_url: TOKEN_URL.into(),
            cache: paths::antigravity_tokens(),
        }
    }
}

/// One poll of one Antigravity account. Never fails: an error becomes
/// [`Usage::error`].
pub async fn read(client: &Client, account: &ServiceAccount, now: i64) -> Usage {
    read_with(client, &Endpoints::live(), account, now).await
}

async fn read_with(
    client: &Client,
    endpoints: &Endpoints,
    account: &ServiceAccount,
    now: i64,
) -> Usage {
    match poll(client, endpoints, account, now).await {
        Ok(usage) => usage,
        Err(error) => Usage::failed(
            Service::Antigravity,
            &account.name,
            now,
            format!("{error:#}"),
        ),
    }
}

async fn poll(
    client: &Client,
    endpoints: &Endpoints,
    account: &ServiceAccount,
    now: i64,
) -> Result<Usage> {
    let now_ms = now * 1000;
    let mut session = Session::start(client, endpoints, account, now_ms).await?;

    // The plan is decoration: a failed loadCodeAssist still reads quota.
    let (plan, project) = match session.post("loadCodeAssist", &load_body()).await {
        Ok(value) => plan_and_project(&value),
        Err(_) => (None, None),
    };
    let project = project.unwrap_or_else(|| CONSUMER_PROJECT.into());
    let summary = session
        .post(
            "retrieveUserQuotaSummary",
            &serde_json::json!({ "project": project }),
        )
        .await?;
    Ok(Usage {
        service: Service::Antigravity,
        account: account.name.clone(),
        at: now,
        plan,
        meters: meters(&summary, now)?,
        balances: Vec::new(),
        error: None,
    })
}

fn load_body() -> Value {
    serde_json::json!({
        "metadata": {
            "ideType": "ANTIGRAVITY",
            "platform": platform(),
            "pluginType": "GEMINI",
        }
    })
}

fn platform() -> &'static str {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("macos", "aarch64") => "DARWIN_ARM64",
        ("macos", _) => "DARWIN_AMD64",
        ("linux", "aarch64") => "LINUX_ARM64",
        ("linux", _) => "LINUX_AMD64",
        ("windows", _) => "WINDOWS_AMD64",
        _ => "PLATFORM_UNSPECIFIED",
    }
}

fn plan_and_project(value: &Value) -> (Option<String>, Option<String>) {
    let tier = |key: &str| {
        value
            .pointer(&format!("/{key}/id"))
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty())
            .map(str::to_owned)
    };
    let project = value
        .get("cloudaicompanionProject")
        .and_then(|project| {
            project
                .as_str()
                .or_else(|| project.get("id").and_then(Value::as_str))
        })
        .filter(|project| !project.is_empty())
        .map(str::to_owned);
    (tier("paidTier").or_else(|| tier("currentTier")), project)
}

// ---- quota summary ----

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Summary {
    #[serde(default)]
    groups: Vec<Group>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Group {
    #[serde(default)]
    display_name: Option<String>,
    #[serde(default)]
    buckets: Vec<Bucket>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Bucket {
    #[serde(default)]
    bucket_id: Option<String>,
    #[serde(default)]
    window: Option<String>,
    #[serde(default)]
    remaining_fraction: Option<f64>,
    #[serde(default)]
    remaining_amount: Option<Value>,
    #[serde(default)]
    disabled: bool,
    #[serde(default)]
    reset_time: Option<String>,
}

/// Meters from a `retrieveUserQuotaSummary` answer (bare, or wrapped in
/// `response`).
fn meters(value: &Value, now: i64) -> Result<Vec<Meter>> {
    let body = value.get("response").unwrap_or(value);
    let summary: Summary =
        serde_json::from_value(body.clone()).context("unexpected quota summary shape")?;
    if summary.groups.is_empty() {
        bail!("quota summary carried no groups");
    }
    let mut meters = Vec::new();
    for group in summary.groups {
        for bucket in group.buckets {
            if bucket.disabled {
                continue;
            }
            if let Some(meter) = meter(group.display_name.clone(), bucket, now) {
                meters.push(meter);
            }
        }
    }
    Ok(meters)
}

fn meter(group: Option<String>, bucket: Bucket, now: i64) -> Option<Meter> {
    let label = bucket.window.clone().or(bucket.bucket_id.clone())?;
    let span_secs = match bucket.window.as_deref() {
        Some("5h") => Some(FIVE_HOURS),
        Some("weekly") => Some(WEEK),
        _ => None,
    };
    let reset = bucket.reset_time.as_deref().and_then(parse_time);
    let floating = match (reset, span_secs) {
        (Some(reset), Some(span)) => (reset - (now + span)).abs() <= IDLE_RESET_TOLERANCE_SECS,
        _ => false,
    };
    let remaining = match (bucket.remaining_fraction, &bucket.remaining_amount) {
        (Some(fraction), _) => fraction,
        // `remaining` is a oneof; a zero amount is exhausted.
        (None, Some(amount)) if amount_is_zero(amount) => 0.0,
        (None, Some(_)) => return None,
        // Neither member: proto3 JSON leaves a zero out. A window whose reset
        // has started counting is spent; one still floating is untouched.
        (None, None) if reset.is_some() && !floating => 0.0,
        (None, None) => return None,
    };
    let idle = remaining >= 1.0 && floating;
    let resets_at = if idle { 0 } else { reset.unwrap_or(0) };
    Some(Meter {
        group,
        label,
        window: Window::new(1.0 - remaining, resets_at),
        span_secs,
        amount: None,
    })
}

fn amount_is_zero(amount: &Value) -> bool {
    amount.as_i64() == Some(0) || amount.as_str() == Some("0")
}

fn parse_time(text: &str) -> Option<i64> {
    DateTime::parse_from_rfc3339(text)
        .ok()
        .map(|time| time.timestamp())
}

// ---- HTTP session ----

/// The `antigravity/hub/<version> (...)` identity the Gemini group needs.
fn user_agent() -> &'static str {
    use std::sync::OnceLock;
    static AGENT: OnceLock<String> = OnceLock::new();
    AGENT.get_or_init(|| {
        let version = installed_app_version()
            .filter(|version| {
                parse_version(version).is_some_and(|v| {
                    v >= MIN_GEMINI_HUB_VERSION && Some(v) > parse_version(PINNED_HUB_VERSION)
                })
            })
            .unwrap_or_else(|| PINNED_HUB_VERSION.into());
        let os = match std::env::consts::OS {
            "macos" => "darwin",
            other => other,
        };
        let arch = match std::env::consts::ARCH {
            "aarch64" => "arm64",
            "x86_64" => "amd64",
            other => other,
        };
        format!("antigravity/hub/{version} (aidev_client; os_type={os}; arch={arch})")
    })
}

/// `CFBundleShortVersionString` of the installed Antigravity app.
fn installed_app_version() -> Option<String> {
    if !cfg!(target_os = "macos") {
        return None;
    }
    let plist =
        std::fs::read_to_string("/Applications/Antigravity.app/Contents/Info.plist").ok()?;
    plist_string(&plist, "CFBundleShortVersionString")
}

fn plist_string(plist: &str, key: &str) -> Option<String> {
    let after = &plist[plist.find(&format!("<key>{key}</key>"))?..];
    let start = after.find("<string>")? + "<string>".len();
    let end = after[start..].find("</string>")?;
    Some(after[start..start + end].trim().to_owned())
}

fn parse_version(text: &str) -> Option<(u32, u32, u32)> {
    let mut parts = text.split('.').map(|part| part.parse::<u32>().ok());
    let major = parts.next()??;
    let minor = parts.next()??;
    let patch = parts.next().unwrap_or(Some(0))?;
    Some((major, minor, patch))
}

struct Session<'a> {
    client: &'a Client,
    endpoints: &'a Endpoints,
    account: &'a ServiceAccount,
    now_ms: i64,
    access_token: String,
    /// Whether `access_token` came from a refresh made by this session.
    fresh: bool,
}

impl<'a> Session<'a> {
    async fn start(
        client: &'a Client,
        endpoints: &'a Endpoints,
        account: &'a ServiceAccount,
        now_ms: i64,
    ) -> Result<Self> {
        let mut session = Self {
            client,
            endpoints,
            account,
            now_ms,
            access_token: String::new(),
            fresh: false,
        };
        match session.stored_token() {
            Some(token) => session.access_token = token,
            None => session.refresh().await?,
        }
        Ok(session)
    }

    /// An unexpired access token from the account or the cache.
    fn stored_token(&self) -> Option<String> {
        let live = |token: &str, expires_at: i64| {
            (!token.is_empty() && expires_at > self.now_ms + EXPIRY_MARGIN_MS)
                .then(|| token.to_owned())
        };
        if let (Some(token), Some(expires_at)) =
            (&self.account.access_token, self.account.expires_at)
            && let Some(token) = live(token, expires_at)
        {
            return Some(token);
        }
        let refresh_token = self.account.refresh_token.as_deref()?;
        let cache = TokenCache::load(&self.endpoints.cache);
        let cached = cache.tokens.get(&self.account.name)?;
        if cached.refresh_fingerprint != fingerprint(refresh_token) {
            return None;
        }
        live(&cached.access_token, cached.expires_at)
    }

    async fn refresh(&mut self) -> Result<()> {
        let refresh_token = self
            .account
            .refresh_token
            .as_deref()
            .filter(|token| !token.is_empty())
            .context("no refresh token; run `agy` and sign in, then import the account")?;
        let grant = exchange(self.client, &self.endpoints.token_url, refresh_token).await?;
        let expires_at = self.now_ms + grant.expires_in.unwrap_or(3599) * 1000;
        // A cache that cannot be written costs one extra refresh next poll.
        let _ = TokenCache::store(
            &self.endpoints.cache,
            &self.account.name,
            CachedToken {
                access_token: grant.access_token.clone(),
                expires_at,
                refresh_fingerprint: fingerprint(refresh_token),
            },
        );
        self.access_token = grant.access_token;
        self.fresh = true;
        Ok(())
    }

    /// POST `v1internal:<method>`; a 401 on a stored token refreshes once.
    async fn post(&mut self, method: &str, body: &Value) -> Result<Value> {
        loop {
            let response = self
                .client
                .post(format!("{}/v1internal:{method}", self.endpoints.api))
                .bearer_auth(&self.access_token)
                .header(header::USER_AGENT, user_agent())
                .timeout(REQUEST_TIMEOUT)
                .json(body)
                .send()
                .await
                .with_context(|| format!("{method} request failed"))?;
            let status = response.status();
            if status == StatusCode::UNAUTHORIZED && !self.fresh {
                self.refresh().await?;
                continue;
            }
            let text = response
                .text()
                .await
                .with_context(|| format!("{method} response could not be read"))?;
            if !status.is_success() {
                bail!("{method} returned {status}: {}", error_message(&text));
            }
            return serde_json::from_str(&text)
                .with_context(|| format!("{method} returned something other than JSON"));
        }
    }
}

/// Google's `error.message` (or `error_description`), never the raw body.
fn error_message(text: &str) -> String {
    let value: Value = serde_json::from_str(text).unwrap_or(Value::Null);
    value
        .pointer("/error/message")
        .or_else(|| value.get("error_description"))
        .or_else(|| value.get("error"))
        .and_then(Value::as_str)
        .unwrap_or("no error message")
        .chars()
        .take(200)
        .collect()
}

#[derive(Deserialize)]
struct Grant {
    access_token: String,
    #[serde(default)]
    expires_in: Option<i64>,
}

async fn exchange(client: &Client, token_url: &str, refresh_token: &str) -> Result<Grant> {
    let response = client
        .post(token_url)
        .timeout(REQUEST_TIMEOUT)
        .form(&[
            ("grant_type", "refresh_token"),
            ("client_id", CLIENT_ID),
            ("client_secret", CLIENT_SECRET),
            ("refresh_token", refresh_token),
        ])
        .send()
        .await
        .context("token refresh request failed")?;
    let status = response.status();
    let text = response
        .text()
        .await
        .context("token refresh response could not be read")?;
    if !status.is_success() {
        let value: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
        if value.get("error").and_then(Value::as_str) == Some("invalid_grant") {
            bail!("refresh token revoked or expired; sign in again with `agy`");
        }
        bail!("token refresh returned {status}: {}", error_message(&text));
    }
    serde_json::from_str(&text).context("token refresh answer carried no access_token")
}

fn fingerprint(secret: &str) -> String {
    let digest = Sha256::digest(secret.as_bytes());
    digest[..8]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

// ---- access-token cache ----

#[derive(Debug, Default, Serialize, Deserialize)]
struct TokenCache {
    #[serde(default)]
    tokens: BTreeMap<String, CachedToken>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct CachedToken {
    access_token: String,
    /// Epoch milliseconds.
    expires_at: i64,
    /// Which refresh token minted it; a re-imported grant ignores the entry.
    refresh_fingerprint: String,
}

impl TokenCache {
    fn load(path: &Path) -> Self {
        read_if_present(path)
            .ok()
            .flatten()
            .and_then(|text| serde_json::from_str(&text).ok())
            .unwrap_or_default()
    }

    /// Read-modify-write under the cache's lock; skipped when another process
    /// holds it.
    fn store(path: &Path, account: &str, token: CachedToken) -> Result<()> {
        let Some(_lock) = FileLock::try_acquire(&path.with_extension("lock"))? else {
            return Ok(());
        };
        let mut cache = Self::load(path);
        cache.tokens.insert(account.to_owned(), token);
        let mut bytes = serde_json::to_vec_pretty(&cache)?;
        bytes.push(b'\n');
        write_atomic(path, &bytes, 0o600)
    }
}

// ---- discovery ----

/// The Antigravity accounts signed in on this machine: `agy`'s login (its
/// Keychain item, else its token file) and the Antigravity app's login (its
/// `state.vscdb`), one per email. Read-only.
pub fn discover() -> Result<Vec<ServiceAccount>> {
    let mut found: Vec<ServiceAccount> = Vec::new();
    let mut problems = Vec::new();
    let cli = match cli_keychain_grant() {
        Ok(Some(grant)) => Ok(Some(grant)),
        Ok(None) | Err(_) => cli_file_grant(),
    };
    for source in [cli, app_grant()] {
        match source {
            Ok(Some(account)) => {
                if !found.iter().any(|known| known.name == account.name) {
                    found.push(account);
                }
            }
            Ok(None) => {}
            Err(error) => problems.push(format!("{error:#}")),
        }
    }
    if found.is_empty() && !problems.is_empty() {
        bail!("no Antigravity login found: {}", problems.join("; "));
    }
    Ok(found)
}

fn account(email: String, refresh_token: String, access: Option<(String, i64)>) -> ServiceAccount {
    let (access_token, expires_at) = match access {
        Some((token, expires_at)) => (Some(token), Some(expires_at)),
        None => (None, None),
    };
    ServiceAccount {
        name: email,
        service: Service::Antigravity,
        key: None,
        key_file: None,
        refresh_token: Some(refresh_token),
        access_token,
        expires_at,
        label: None,
    }
}

/// `agy`'s stored login: `{"token": <oauth2.Token>, "auth_method", "id_token_email"}`,
/// optionally behind go-keyring's `go-keyring-base64:` prefix.
fn parse_cli_grant(text: &str) -> Result<Option<ServiceAccount>> {
    use base64::Engine;
    let text = text.trim();
    let json = match text.strip_prefix("go-keyring-base64:") {
        Some(encoded) => base64::engine::general_purpose::STANDARD
            .decode(encoded)
            .context("agy's Keychain item is not valid base64")?,
        None => text.as_bytes().to_vec(),
    };
    let value: Value = serde_json::from_slice(&json).context("agy's login is not JSON")?;
    let Some(refresh) = value
        .pointer("/token/refresh_token")
        .and_then(Value::as_str)
        .filter(|token| !token.is_empty())
    else {
        return Ok(None);
    };
    let email = value
        .get("id_token_email")
        .and_then(Value::as_str)
        .filter(|email| !email.is_empty())
        .context("agy's login names no account (id_token_email)")?;
    let access = value
        .pointer("/token/access_token")
        .and_then(Value::as_str)
        .zip(
            value
                .pointer("/token/expiry")
                .and_then(Value::as_str)
                .and_then(|expiry| DateTime::parse_from_rfc3339(expiry).ok()),
        )
        .map(|(token, expiry)| (token.to_owned(), expiry.timestamp_millis()));
    Ok(Some(account(email.to_owned(), refresh.to_owned(), access)))
}

fn cli_file_grant() -> Result<Option<ServiceAccount>> {
    let path = paths::expand_tilde("~/.gemini/antigravity-cli/antigravity-oauth-token");
    match read_if_present(&path)? {
        Some(text) => parse_cli_grant(&text),
        None => Ok(None),
    }
}

/// `agy` stores its login through go-keyring, which drives `/usr/bin/security`;
/// the item's access list trusts that tool, so it is read the same way.
#[cfg(target_os = "macos")]
fn cli_keychain_grant() -> Result<Option<ServiceAccount>> {
    let output = run_bounded(
        "/usr/bin/security",
        &[
            "find-generic-password",
            "-s",
            "gemini",
            "-a",
            "antigravity",
            "-w",
        ],
        Duration::from_secs(5),
    )?;
    match output {
        Some(text) => parse_cli_grant(&text),
        None => Ok(None),
    }
}

#[cfg(not(target_os = "macos"))]
fn cli_keychain_grant() -> Result<Option<ServiceAccount>> {
    Ok(None)
}

/// Run `program`, waiting at most `timeout`; `None` when it exits non-zero
/// (`security` does so for a missing item).
#[cfg(target_os = "macos")]
fn run_bounded(program: &str, args: &[&str], timeout: Duration) -> Result<Option<String>> {
    use std::io::Read;
    use std::process::{Command, Stdio};
    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .with_context(|| format!("could not run {program}"))?;
    let mut stdout = child.stdout.take().context("no stdout")?;
    let reader = std::thread::spawn(move || {
        let mut text = String::new();
        let _ = stdout.read_to_string(&mut text);
        text
    });
    let deadline = std::time::Instant::now() + timeout;
    let status = loop {
        match child.try_wait()? {
            Some(status) => break status,
            None if std::time::Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(50));
            }
            None => {
                let _ = child.kill();
                let _ = child.wait();
                bail!("{program} did not answer within {}s", timeout.as_secs());
            }
        }
    };
    let text = reader.join().unwrap_or_default();
    Ok(status.success().then_some(text))
}

/// The Antigravity app's (VS Code fork) global state database.
fn app_state_db() -> Option<PathBuf> {
    Some(dirs::config_dir()?.join("Antigravity/User/globalStorage/state.vscdb"))
}

fn app_grant() -> Result<Option<ServiceAccount>> {
    let Some(path) = app_state_db().filter(|path| path.exists()) else {
        return Ok(None);
    };
    let db = rusqlite::Connection::open_with_flags(
        &path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .with_context(|| format!("could not open {}", path.display()))?;
    db.busy_timeout(Duration::from_secs(2))?;
    let item = |key: &str| -> Result<Option<String>> {
        use rusqlite::OptionalExtension;
        db.query_row("SELECT value FROM ItemTable WHERE key = ?1", [key], |row| {
            row.get::<_, String>(0)
        })
        .optional()
        .with_context(|| format!("could not read {key} from {}", path.display()))
    };
    let Some(token_item) = item("antigravityUnifiedStateSync.oauthToken")? else {
        return Ok(None);
    };
    let Some(token) = vscdb::oauth_token(&token_item) else {
        bail!("the Antigravity app's oauthToken entry has an unknown layout");
    };
    if token.refresh_token.is_empty() {
        return Ok(None);
    }
    let email = item("antigravityUnifiedStateSync.userStatus")?
        .and_then(|status| vscdb::user_email(&status))
        .context("the Antigravity app's userStatus names no account")?;
    let access = (!token.access_token.is_empty() && token.expiry > 0)
        .then(|| (token.access_token.clone(), token.expiry * 1000));
    Ok(Some(account(email, token.refresh_token, access)))
}

/// The app's `antigravityUnifiedStateSync.*` values: base64 of a protobuf
/// map whose entries are `{1: key, 2: {1: value}}`, each value itself base64
/// of a protobuf message.
mod vscdb {
    use base64::Engine;
    use base64::engine::general_purpose::STANDARD;

    use super::proto::{self, Field};

    #[derive(Debug, PartialEq)]
    pub struct OauthToken {
        pub access_token: String,
        pub refresh_token: String,
        /// Epoch seconds.
        pub expiry: i64,
    }

    /// The value of the entry named `sentinel`, base64-decoded.
    fn entry(item: &str, sentinel: &str) -> Option<Vec<u8>> {
        let outer = STANDARD.decode(item.trim()).ok()?;
        for (number, field) in proto::fields(&outer)? {
            let (1, Field::Bytes(entry)) = (number, field) else {
                continue;
            };
            let entry = proto::fields(entry)?;
            if proto::string(&entry, 1).as_deref() != Some(sentinel) {
                continue;
            }
            let wrapper = proto::fields(proto::bytes(&entry, 2)?)?;
            return STANDARD.decode(proto::string(&wrapper, 1)?.trim()).ok();
        }
        None
    }

    /// `oauthTokenInfoSentinelKey`: `{1: access, 2: type, 3: refresh, 4: {1: seconds}}`.
    pub fn oauth_token(item: &str) -> Option<OauthToken> {
        let message = entry(item, "oauthTokenInfoSentinelKey")?;
        let fields = proto::fields(&message)?;
        let expiry = proto::bytes(&fields, 4)
            .and_then(proto::fields)
            .and_then(|timestamp| proto::varint(&timestamp, 1))
            .unwrap_or(0);
        Some(OauthToken {
            access_token: proto::string(&fields, 1).unwrap_or_default(),
            refresh_token: proto::string(&fields, 3)?,
            expiry: expiry as i64,
        })
    }

    /// `userStatusSentinelKey`: field 7 is the signed-in email.
    pub fn user_email(item: &str) -> Option<String> {
        let message = entry(item, "userStatusSentinelKey")?;
        proto::string(&proto::fields(&message)?, 7).filter(|email| email.contains('@'))
    }
}

/// Just enough protobuf wire format to walk the app's state.
mod proto {
    #[derive(Debug, Clone, Copy, PartialEq)]
    pub enum Field<'a> {
        Varint(u64),
        Bytes(&'a [u8]),
        Fixed64(u64),
        Fixed32(u32),
    }

    fn varint_at(bytes: &[u8], at: &mut usize) -> Option<u64> {
        let mut value = 0u64;
        for shift in (0..64).step_by(7) {
            let byte = *bytes.get(*at)?;
            *at += 1;
            value |= u64::from(byte & 0x7f) << shift;
            if byte < 0x80 {
                return Some(value);
            }
        }
        None
    }

    /// Every field of one message, or `None` when `bytes` is not one.
    pub fn fields(bytes: &[u8]) -> Option<Vec<(u64, Field<'_>)>> {
        let mut at = 0;
        let mut out = Vec::new();
        while at < bytes.len() {
            let key = varint_at(bytes, &mut at)?;
            let number = key >> 3;
            if number == 0 {
                return None;
            }
            let field = match key & 7 {
                0 => Field::Varint(varint_at(bytes, &mut at)?),
                1 => {
                    let raw = bytes.get(at..at + 8)?;
                    at += 8;
                    Field::Fixed64(u64::from_le_bytes(raw.try_into().ok()?))
                }
                2 => {
                    let len = usize::try_from(varint_at(bytes, &mut at)?).ok()?;
                    let raw = bytes.get(at..at.checked_add(len)?)?;
                    at += len;
                    Field::Bytes(raw)
                }
                5 => {
                    let raw = bytes.get(at..at + 4)?;
                    at += 4;
                    Field::Fixed32(u32::from_le_bytes(raw.try_into().ok()?))
                }
                _ => return None,
            };
            out.push((number, field));
        }
        Some(out)
    }

    pub fn bytes<'a>(fields: &[(u64, Field<'a>)], number: u64) -> Option<&'a [u8]> {
        fields.iter().find_map(|(n, field)| match field {
            Field::Bytes(raw) if *n == number => Some(*raw),
            _ => None,
        })
    }

    pub fn string(fields: &[(u64, Field<'_>)], number: u64) -> Option<String> {
        String::from_utf8(bytes(fields, number)?.to_vec()).ok()
    }

    pub fn varint(fields: &[(u64, Field<'_>)], number: u64) -> Option<u64> {
        fields.iter().find_map(|(n, field)| match field {
            Field::Varint(value) if *n == number => Some(*value),
            _ => None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::extract::State;
    use axum::http::HeaderMap;
    use axum::routing::post;
    use axum::{Form, Json, Router};
    use std::sync::{Arc, Mutex};

    const NOW: i64 = 1_762_893_000;

    fn at(offset: i64) -> String {
        DateTime::from_timestamp(NOW + offset, 0)
            .unwrap()
            .to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
    }

    /// The live answer of 2026-11-11 (one Ultra account), shifted to [`NOW`]:
    /// Gemini weekly started and partly spent, Gemini 5h untouched, the
    /// Claude/GPT weekly anchored at full, its 5h untouched.
    fn live_summary() -> Value {
        serde_json::json!({
            "groups": [
                {"displayName": "Gemini Models",
                 "description": "Models within this group: Gemini Flash, Gemini Pro",
                 "buckets": [
                    {"bucketId": "gemini-weekly", "displayName": "Weekly Limit Remaining",
                     "window": "weekly", "resetTime": at(6 * 86_400 + 43_000),
                     "remainingFraction": 0.9466626},
                    {"bucketId": "gemini-5h", "displayName": "Five Hour Limit Remaining",
                     "window": "5h", "resetTime": at(FIVE_HOURS), "remainingFraction": 1}]},
                {"displayName": "Claude and GPT models",
                 "description": "Models within this group: Claude Opus, Claude Sonnet, GPT-OSS",
                 "buckets": [
                    {"bucketId": "3p-weekly", "displayName": "Weekly Limit Remaining",
                     "window": "weekly", "resetTime": at(6 * 86_400 + 43_000),
                     "remainingFraction": 1},
                    {"bucketId": "3p-5h", "displayName": "Five Hour Limit Remaining",
                     "window": "5h", "resetTime": at(FIVE_HOURS - 7), "remainingFraction": 1}]}],
            "description": "Within each group, models share a weekly limit and a 5-hour limit."
        })
    }

    fn load_answer() -> Value {
        serde_json::json!({
            "currentTier": {"id": "free-tier", "name": "Antigravity"},
            "cloudaicompanionProject": "aicode-consumers",
            "paidTier": {"id": "g1-ultra-tier", "name": "Google AI Ultra"}
        })
    }

    #[test]
    fn the_live_summary_becomes_one_meter_per_bucket() {
        let meters = meters(&live_summary(), NOW).unwrap();
        let view: Vec<_> = meters
            .iter()
            .map(|m| (m.group.as_deref().unwrap(), m.label.as_str(), m.span_secs))
            .collect();
        assert_eq!(
            view,
            [
                ("Gemini Models", "weekly", Some(WEEK)),
                ("Gemini Models", "5h", Some(FIVE_HOURS)),
                ("Claude and GPT models", "weekly", Some(WEEK)),
                ("Claude and GPT models", "5h", Some(FIVE_HOURS)),
            ]
        );
        assert!((meters[0].window.used - (1.0 - 0.9466626)).abs() < 1e-9);
        assert_eq!(meters[0].window.resets_at, NOW + 6 * 86_400 + 43_000);
    }

    #[test]
    fn an_untouched_window_has_no_reset_but_an_anchored_full_one_keeps_its_own() {
        let meters = meters(&live_summary(), NOW).unwrap();
        // Floating resets (now + 5h, a few seconds either way) are idle windows.
        assert_eq!(meters[1].window, Window::new(0.0, 0));
        assert_eq!(meters[3].window, Window::new(0.0, 0));
        // A full weekly window whose reset is not now + 7d has started.
        assert_eq!(
            meters[2].window,
            Window::new(0.0, NOW + 6 * 86_400 + 43_000)
        );
    }

    #[test]
    fn a_summary_without_the_gemini_group_still_reads() {
        let mut summary = live_summary();
        summary["groups"].as_array_mut().unwrap().remove(0);
        let meters = meters(&summary, NOW).unwrap();
        assert_eq!(meters.len(), 2);
        assert!(
            meters
                .iter()
                .all(|m| m.group.as_deref() == Some("Claude and GPT models"))
        );
    }

    #[test]
    fn missing_fractions_disabled_buckets_and_wrappers() {
        let summary = serde_json::json!({"response": {"groups": [{"displayName": "G", "buckets": [
            // Spent: proto3 JSON drops the zero, the reset is anchored.
            {"window": "5h", "resetTime": at(3600)},
            // Untouched and floating with no fraction: nothing to say.
            {"window": "weekly", "resetTime": at(WEEK)},
            {"window": "5h", "remainingAmount": "0", "resetTime": at(60)},
            {"window": "5h", "disabled": true, "remainingFraction": 0.5},
            {"bucketId": "argon-weekly", "remainingFraction": 0.25}
        ]}]}});
        let meters = meters(&summary, NOW).unwrap();
        let view: Vec<_> = meters
            .iter()
            .map(|m| (m.label.as_str(), m.window.used, m.window.resets_at))
            .collect();
        assert_eq!(
            view,
            [
                ("5h", 1.0, NOW + 3600),
                ("5h", 1.0, NOW + 60),
                ("argon-weekly", 0.75, 0),
            ]
        );
        assert!(super::meters(&serde_json::json!({"groups": []}), NOW).is_err());
    }

    #[test]
    fn the_plan_is_the_paid_tier_not_the_current_one() {
        assert_eq!(
            plan_and_project(&load_answer()),
            (
                Some("g1-ultra-tier".into()),
                Some("aicode-consumers".into())
            )
        );
        let free = serde_json::json!({"currentTier": {"id": "free-tier"}});
        assert_eq!(plan_and_project(&free), (Some("free-tier".into()), None));
    }

    #[test]
    fn the_user_agent_is_a_hub_client_new_enough_for_the_gemini_group() {
        let agent = user_agent();
        let version = agent
            .strip_prefix("antigravity/hub/")
            .and_then(|rest| rest.split(' ').next())
            .unwrap();
        assert!(parse_version(version).unwrap() >= MIN_GEMINI_HUB_VERSION);
        assert!(agent.contains("aidev_client"));
        assert_eq!(parse_version("2.9"), Some((2, 9, 0)));
        assert_eq!(parse_version("2.x"), None);
        let plist = "<dict>\n<key>CFBundleShortVersionString</key>\n\t<string>2.0.1</string>";
        assert_eq!(
            plist_string(plist, "CFBundleShortVersionString").as_deref(),
            Some("2.0.1")
        );
    }

    // ---- loopback ----

    #[derive(Default)]
    struct Seen {
        refreshes: Vec<Vec<(String, String)>>,
        bearers: Vec<(String, String)>,
        agents: Vec<String>,
        summary_bodies: Vec<Value>,
    }

    #[derive(Clone)]
    struct Fake {
        seen: Arc<Mutex<Seen>>,
        token_status: StatusCode,
        /// Bearer tokens the API accepts.
        valid: Vec<&'static str>,
        summary: Value,
    }

    fn bearer(headers: &HeaderMap) -> String {
        headers
            .get(header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer "))
            .unwrap_or_default()
            .to_owned()
    }

    async fn token(
        State(fake): State<Fake>,
        Form(form): Form<Vec<(String, String)>>,
    ) -> (StatusCode, Json<Value>) {
        fake.seen.lock().unwrap().refreshes.push(form);
        if fake.token_status.is_success() {
            (
                fake.token_status,
                Json(
                    serde_json::json!({"access_token": "minted", "expires_in": 3599,
                    "token_type": "Bearer", "id_token": "x.y.z"}),
                ),
            )
        } else {
            (
                fake.token_status,
                Json(serde_json::json!({"error": "invalid_grant",
                    "error_description": "Token has been expired or revoked."})),
            )
        }
    }

    async fn api(
        fake: &Fake,
        method: &str,
        headers: &HeaderMap,
        body: Value,
        answer: Value,
    ) -> (StatusCode, Json<Value>) {
        let token = bearer(headers);
        let mut seen = fake.seen.lock().unwrap();
        seen.bearers.push((method.into(), token.clone()));
        seen.agents.push(
            headers
                .get(header::USER_AGENT)
                .and_then(|v| v.to_str().ok())
                .unwrap_or_default()
                .into(),
        );
        if method == "summary" {
            seen.summary_bodies.push(body);
        }
        if fake.valid.contains(&token.as_str()) {
            (StatusCode::OK, Json(answer))
        } else {
            (
                StatusCode::UNAUTHORIZED,
                Json(serde_json::json!({"error": {"code": 401,
                    "message": "Request had invalid authentication credentials."}})),
            )
        }
    }

    async fn serve(fake: Fake) -> (Endpoints, tempfile::TempDir) {
        let router = Router::new()
            .route("/token", post(token))
            .route(
                "/v1internal:loadCodeAssist",
                post(
                    |State(f): State<Fake>, h: HeaderMap, Json(b): Json<Value>| async move {
                        api(&f, "load", &h, b, load_answer()).await
                    },
                ),
            )
            .route(
                "/v1internal:retrieveUserQuotaSummary",
                post(
                    |State(f): State<Fake>, h: HeaderMap, Json(b): Json<Value>| async move {
                        let answer = f.summary.clone();
                        api(&f, "summary", &h, b, answer).await
                    },
                ),
            )
            .with_state(fake);
        let base = crate::testing::serve(router).await;
        let dir = tempfile::tempdir().unwrap();
        let endpoints = Endpoints {
            api: base.clone(),
            token_url: format!("{base}/token"),
            cache: dir.path().join("antigravity-access-tokens.json"),
        };
        (endpoints, dir)
    }

    fn fake(valid: Vec<&'static str>) -> Fake {
        Fake {
            seen: Arc::default(),
            token_status: StatusCode::OK,
            valid,
            summary: live_summary(),
        }
    }

    fn grant(name: &str) -> ServiceAccount {
        account(name.into(), "1//refresh".into(), None)
    }

    #[tokio::test]
    async fn a_poll_refreshes_caches_and_reads_the_summary() {
        let fake = fake(vec!["minted"]);
        let (endpoints, _dir) = serve(fake.clone()).await;
        let client = Client::new();
        let usage = read_with(&client, &endpoints, &grant("a@example.com"), NOW).await;

        assert_eq!(usage.error, None);
        assert_eq!(usage.plan.as_deref(), Some("g1-ultra-tier"));
        assert_eq!(usage.meters.len(), 4);
        {
            let seen = fake.seen.lock().unwrap();
            let form = &seen.refreshes[0];
            let field = |k: &str| {
                form.iter()
                    .find(|(key, _)| key == k)
                    .map(|(_, v)| v.as_str())
            };
            assert_eq!(field("grant_type"), Some("refresh_token"));
            assert_eq!(field("refresh_token"), Some("1//refresh"));
            assert_eq!(field("client_id"), Some(CLIENT_ID));
            assert_eq!(field("client_secret"), Some(CLIENT_SECRET));
            assert_eq!(
                seen.summary_bodies,
                [serde_json::json!({"project": "aicode-consumers"})]
            );
            assert!(
                seen.agents
                    .iter()
                    .all(|a| a.starts_with("antigravity/hub/"))
            );
        }

        // The second poll reuses the cached token: no second refresh.
        let again = read_with(&client, &endpoints, &grant("a@example.com"), NOW + 60).await;
        assert_eq!(again.error, None);
        assert_eq!(fake.seen.lock().unwrap().refreshes.len(), 1);
        let cached = TokenCache::load(&endpoints.cache);
        assert_eq!(
            cached.tokens["a@example.com"].expires_at,
            NOW * 1000 + 3_599_000
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&endpoints.cache)
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600);
        }

        // A re-imported grant (different refresh token) ignores the cache.
        let mut other = grant("a@example.com");
        other.refresh_token = Some("1//other".into());
        read_with(&client, &endpoints, &other, NOW + 120).await;
        assert_eq!(fake.seen.lock().unwrap().refreshes.len(), 2);
    }

    #[tokio::test]
    async fn an_unexpired_token_is_used_and_a_rejected_one_is_refreshed_once() {
        let fake = fake(vec!["minted"]);
        let (endpoints, _dir) = serve(fake.clone()).await;
        let mut account = grant("b@example.com");
        account.access_token = Some("stale-but-unexpired".into());
        account.expires_at = Some(NOW * 1000 + 600_000);
        let usage = read_with(&Client::new(), &endpoints, &account, NOW).await;
        assert_eq!(usage.error, None);
        let seen = fake.seen.lock().unwrap();
        assert_eq!(seen.refreshes.len(), 1);
        assert_eq!(
            seen.bearers[0],
            ("load".into(), "stale-but-unexpired".into())
        );
        assert!(seen.bearers[1..].iter().all(|(_, token)| token == "minted"));
    }

    #[tokio::test]
    async fn a_revoked_grant_is_an_error_usage_that_names_the_fix() {
        let mut fake = fake(vec!["minted"]);
        fake.token_status = StatusCode::BAD_REQUEST;
        let (endpoints, _dir) = serve(fake.clone()).await;
        let usage = read_with(&Client::new(), &endpoints, &grant("c@example.com"), NOW).await;
        assert!(usage.meters.is_empty());
        let error = usage.error.unwrap();
        assert!(error.contains("sign in again"), "{error}");
        assert!(!error.contains("1//refresh"));
        assert!(fake.seen.lock().unwrap().bearers.is_empty());
    }

    #[tokio::test]
    async fn an_account_without_a_refresh_token_fails_before_any_request() {
        let fake = fake(vec![]);
        let (endpoints, _dir) = serve(fake.clone()).await;
        let mut account = grant("d@example.com");
        account.refresh_token = None;
        let usage = read_with(&Client::new(), &endpoints, &account, NOW).await;
        assert!(usage.error.unwrap().contains("no refresh token"));
        assert!(fake.seen.lock().unwrap().refreshes.is_empty());
    }

    /// Reads every Antigravity login on this machine against the live API,
    /// caching access tokens in a temporary directory. Prints no secrets.
    /// `cargo test --lib providers::antigravity::tests::live -- --ignored --nocapture`
    #[tokio::test]
    #[ignore = "reads the real accounts on this machine"]
    async fn live() {
        let accounts = discover().unwrap();
        assert!(!accounts.is_empty());
        let dir = tempfile::tempdir().unwrap();
        let mut endpoints = Endpoints::live();
        endpoints.cache = dir.path().join("antigravity-access-tokens.json");
        let now = chrono::Utc::now().timestamp();
        println!("user agent: {}", user_agent());
        for account in accounts {
            let usage = read_with(&Client::new(), &endpoints, &account, now).await;
            println!(
                "{} plan={:?} error={:?}",
                usage.account, usage.plan, usage.error
            );
            for meter in &usage.meters {
                println!(
                    "  {:<24} {:<7} used={:.4} resets_at={}",
                    meter.group.as_deref().unwrap_or("-"),
                    meter.label,
                    meter.window.used,
                    meter.window.resets_at
                );
            }
            assert_eq!(usage.error, None);
        }
    }

    // ---- discovery decoding ----

    fn key(number: u64, wire: u64) -> Vec<u8> {
        varint((number << 3) | wire)
    }

    fn varint(mut value: u64) -> Vec<u8> {
        let mut out = Vec::new();
        loop {
            let byte = (value & 0x7f) as u8;
            value >>= 7;
            if value == 0 {
                out.push(byte);
                return out;
            }
            out.push(byte | 0x80);
        }
    }

    fn bytes_field(number: u64, raw: &[u8]) -> Vec<u8> {
        let mut out = key(number, 2);
        out.extend(varint(raw.len() as u64));
        out.extend(raw);
        out
    }

    fn varint_field(number: u64, value: u64) -> Vec<u8> {
        let mut out = key(number, 0);
        out.extend(varint(value));
        out
    }

    /// An `antigravityUnifiedStateSync.*` item holding `entries`.
    fn state_item(entries: &[(&str, Vec<u8>)]) -> String {
        use base64::Engine;
        let b64 = &base64::engine::general_purpose::STANDARD;
        let mut outer = Vec::new();
        for (sentinel, message) in entries {
            let wrapper = bytes_field(1, b64.encode(message).as_bytes());
            let mut entry = bytes_field(1, sentinel.as_bytes());
            entry.extend(bytes_field(2, &wrapper));
            outer.extend(bytes_field(1, &entry));
        }
        b64.encode(outer)
    }

    #[test]
    fn the_app_oauth_token_decodes_from_its_nested_protobuf() {
        let mut token = bytes_field(1, b"ya29.access");
        token.extend(bytes_field(2, b"Bearer"));
        token.extend(bytes_field(3, b"1//refresh"));
        token.extend(bytes_field(4, &varint_field(1, 1_779_740_657)));
        let item = state_item(&[
            (
                "authStateWithContextSentinelKey",
                br#"{"state":"signedIn"}"#.to_vec(),
            ),
            ("oauthTokenInfoSentinelKey", token),
        ]);
        assert_eq!(
            vscdb::oauth_token(&item),
            Some(vscdb::OauthToken {
                access_token: "ya29.access".into(),
                refresh_token: "1//refresh".into(),
                expiry: 1_779_740_657,
            })
        );
        assert_eq!(vscdb::oauth_token(&state_item(&[])), None);
        assert_eq!(vscdb::oauth_token("not base64!"), None);
    }

    #[test]
    fn the_app_user_status_names_the_account() {
        let mut status = bytes_field(3, b"Some Name");
        status.extend(bytes_field(7, b"someone@example.com"));
        status.extend(bytes_field(33, &bytes_field(1, b"model config")));
        let item = state_item(&[("userStatusSentinelKey", status)]);
        assert_eq!(
            vscdb::user_email(&item).as_deref(),
            Some("someone@example.com")
        );
        let nameless = state_item(&[("userStatusSentinelKey", bytes_field(3, b"Some Name"))]);
        assert_eq!(vscdb::user_email(&nameless), None);
    }

    #[test]
    fn truncated_protobuf_is_rejected_not_misread() {
        let mut message = bytes_field(1, b"abcdef");
        message.truncate(message.len() - 2);
        assert_eq!(proto::fields(&message), None);
        assert_eq!(proto::fields(&[0x00]), None);
    }

    #[test]
    fn the_agy_login_parses_with_and_without_the_go_keyring_prefix() {
        use base64::Engine;
        let json = r#"{"token":{"access_token":"ya29.a","token_type":"Bearer",
            "refresh_token":"1//r","expiry":"2026-11-11T13:07:02.25159-08:00","expires_in":3599},
            "auth_method":"consumer","id_token_email":"cli@example.com"}"#;
        let wrapped = format!(
            "go-keyring-base64:{}\n",
            base64::engine::general_purpose::STANDARD.encode(json)
        );
        for text in [json.to_owned(), wrapped] {
            let account = parse_cli_grant(&text).unwrap().unwrap();
            assert_eq!(account.name, "cli@example.com");
            assert_eq!(account.service, Service::Antigravity);
            assert_eq!(account.refresh_token.as_deref(), Some("1//r"));
            assert_eq!(account.access_token.as_deref(), Some("ya29.a"));
            assert_eq!(account.expires_at, Some(1_794_431_222_251));
        }
        assert_eq!(parse_cli_grant(r#"{"token":{}}"#).unwrap(), None);
        assert!(parse_cli_grant("go-keyring-base64:%%%").is_err());
    }
}
