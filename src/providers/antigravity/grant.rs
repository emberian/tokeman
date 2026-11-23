//! Antigravity's Google OAuth grant: the browser login (`tokeman agy login`),
//! the refresh exchange, and finding the logins already on this machine
//! (`agy`'s Keychain item or token file, and the Antigravity app's
//! `state.vscdb`).

use std::fmt;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use chrono::DateTime;
use reqwest::Client;
use serde::Deserialize;
use serde_json::Value;
use sha2::{Digest, Sha256};

use super::{REQUEST_TIMEOUT, error_message, seat};
use crate::fs::read_if_present;
use crate::providers::{ServiceAccount, held};
use crate::quota::{ReadError, Service};

pub(super) const TOKEN_URL: &str = "https://oauth2.googleapis.com/token";
pub(super) const AUTHORIZE_URL: &str = "https://accounts.google.com/o/oauth2/v2/auth";
/// Antigravity's installed-app OAuth client, as shipped in `agy` and the
/// app's `language_server`; both sign-ins (`auth_method: consumer`) use it.
pub(super) const CLIENT_ID: &str =
    "1071006060591-tmhssin2h21lcre235vtolojh4g403ep.apps.googleusercontent.com";
pub(super) const CLIENT_SECRET: &str = "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";
/// The scopes `agy`'s consumer Google OAuth sign-in requests.
pub(super) const SCOPES: &str = "https://www.googleapis.com/auth/cloud-platform \
    https://www.googleapis.com/auth/cclog \
    https://www.googleapis.com/auth/userinfo.profile \
    openid \
    https://www.googleapis.com/auth/experimentsandconfigs \
    https://www.googleapis.com/auth/userinfo.email";
const CALLBACK_PATH: &str = "/auth/callback";
const CALLBACK_READ_TIMEOUT: Duration = Duration::from_secs(5);
const CALLBACK_MAX_BYTES: usize = 8 * 1024;

#[derive(Deserialize)]
pub(super) struct Grant {
    pub access_token: String,
    #[serde(default)]
    pub expires_in: Option<i64>,
    #[serde(default)]
    pub id_token: Option<String>,
}

impl Grant {
    /// The account's email, from the ID token's claims.
    pub fn email(&self) -> Option<String> {
        use base64::Engine;
        let claims = self.id_token.as_deref()?.split('.').nth(1)?;
        let claims = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(claims.trim_end_matches('='))
            .ok()?;
        serde_json::from_slice::<Value>(&claims)
            .ok()?
            .get("email")?
            .as_str()
            .filter(|email| !email.is_empty())
            .map(str::to_owned)
    }
}

/// Trade a refresh token for an access token. A revoked grant is a
/// credential error that names the fix.
pub(super) async fn exchange(
    client: &Client,
    token_url: &str,
    refresh_token: &str,
) -> Result<Grant, ReadError> {
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
        .map_err(|error| ReadError::Network {
            message: format!("token refresh request failed: {error}"),
        })?;
    let status = response.status();
    let text = response.text().await.map_err(|error| ReadError::Network {
        message: format!("token refresh response could not be read: {error}"),
    })?;
    if !status.is_success() {
        let value: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
        if value.get("error").and_then(Value::as_str) == Some("invalid_grant") {
            return Err(ReadError::Credential {
                message: "refresh token revoked or expired; sign in again with `tokeman agy login` (or `agy`, then `tokeman agy import`)".into(),
            });
        }
        return Err(ReadError::Http {
            status: status.as_u16(),
            body: format!("token refresh: {}", error_message(&text)),
        });
    }
    let grant: Grant = serde_json::from_str(&text).map_err(|_| ReadError::Decode {
        message: "token refresh answer carried no access_token".into(),
    })?;
    if grant.access_token.is_empty() {
        return Err(ReadError::Decode {
            message: "token refresh answer carried an empty access_token".into(),
        });
    }
    Ok(grant)
}

// ---- browser login ----

/// A browser login in progress: the authorize URL to open, the loopback
/// listener(s) Google redirects to, and the PKCE verifier that redeems the code.
pub struct LoginSession {
    url: String,
    redirect_uri: String,
    verifier: String,
    state: String,
    listener: tokio::net::TcpListener,
    listener_v6: Option<tokio::net::TcpListener>,
}

impl fmt::Debug for LoginSession {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LoginSession")
            .field("url", &self.url)
            .field("redirect_uri", &self.redirect_uri)
            .field("verifier", &"<set>")
            .field("state", &self.state)
            .finish()
    }
}

#[derive(Deserialize)]
struct CodeGrant {
    access_token: String,
    #[serde(default)]
    refresh_token: Option<String>,
    #[serde(default)]
    expires_in: Option<i64>,
    #[serde(default)]
    id_token: Option<String>,
}

impl fmt::Debug for CodeGrant {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CodeGrant")
            .field("access_token", &"<set>")
            .field("refresh_token", &held(&self.refresh_token))
            .field("expires_in", &self.expires_in)
            .field("id_token", &held(&self.id_token))
            .finish()
    }
}

fn random_b64() -> Result<String> {
    use base64::Engine;
    use std::io::Read;
    let mut bytes = [0u8; 32];
    std::fs::File::open("/dev/urandom")
        .and_then(|mut source| source.read_exact(&mut bytes))
        .context("could not read /dev/urandom")?;
    Ok(base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes))
}

fn urlencode(value: &str) -> String {
    value
        .bytes()
        .map(|byte| match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                (byte as char).to_string()
            }
            _ => format!("%{byte:02X}"),
        })
        .collect()
}

fn urldecode(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut at = 0;
    while at < bytes.len() {
        match bytes[at] {
            b'+' => {
                out.push(b' ');
                at += 1;
            }
            b'%' if at + 2 < bytes.len() => {
                if let Ok(hex) = std::str::from_utf8(&bytes[at + 1..at + 3])
                    && let Ok(decoded) = u8::from_str_radix(hex, 16)
                {
                    out.push(decoded);
                    at += 3;
                    continue;
                }
                out.push(b'%');
                at += 1;
            }
            other => {
                out.push(other);
                at += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Bind a loopback callback port and build the Google OAuth URL for a
/// consumer Antigravity login. `agy`'s own Keychain slot is never touched.
pub async fn begin_login(login_hint: Option<&str>) -> Result<LoginSession> {
    use base64::Engine;
    let (listener, listener_v6) = match tokio::net::TcpListener::bind("127.0.0.1:0").await {
        Ok(v4) => {
            let port = v4
                .local_addr()
                .context("OAuth callback listener has no local address")?
                .port();
            let v6 = tokio::net::TcpListener::bind(format!("[::1]:{port}"))
                .await
                .ok();
            (v4, v6)
        }
        Err(_) => {
            let v6 = tokio::net::TcpListener::bind("[::1]:0")
                .await
                .context("could not bind a localhost port for the OAuth callback")?;
            (v6, None)
        }
    };
    let port = listener
        .local_addr()
        .context("OAuth callback listener has no local address")?
        .port();
    let redirect_uri = format!("http://localhost:{port}{CALLBACK_PATH}");
    let verifier = random_b64()?;
    let state = random_b64()?;
    let challenge = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .encode(Sha256::digest(verifier.as_bytes()));
    let mut url = format!(
        "{AUTHORIZE_URL}?client_id={CLIENT_ID}&redirect_uri={redirect}&response_type=code\
         &scope={scope}&access_type=offline&prompt=consent&code_challenge={challenge}\
         &code_challenge_method=S256&state={state}",
        redirect = urlencode(&redirect_uri),
        scope = urlencode(SCOPES),
    );
    if let Some(hint) = login_hint.filter(|hint| hint.contains('@')) {
        url.push_str(&format!("&login_hint={}", urlencode(hint)));
    }
    Ok(LoginSession {
        url,
        redirect_uri,
        verifier,
        state,
        listener,
        listener_v6,
    })
}

impl LoginSession {
    pub fn url(&self) -> &str {
        &self.url
    }

    pub fn redirect_uri(&self) -> &str {
        &self.redirect_uri
    }

    pub(crate) fn verifier(&self) -> &str {
        &self.verifier
    }

    /// Extract the authorization code from a pasted redirect URL (when the
    /// browser ran on another machine) or a bare code.
    pub fn parse_callback_input(&self, input: &str) -> Result<String> {
        let input = input.trim();
        if input.is_empty() {
            bail!("no authorization code pasted");
        }
        if let Some((_, query)) = input.split_once('?') {
            let query = query.split('#').next().unwrap_or(query);
            return self.extract_query_code(query);
        }
        if input.contains("code=") {
            return self.extract_query_code(input);
        }
        if input.chars().any(char::is_whitespace) {
            bail!("pasted text is neither a callback URL nor an authorization code");
        }
        Ok(urldecode(input))
    }

    fn extract_query_code(&self, query: &str) -> Result<String> {
        let mut code = None;
        let mut state = None;
        let mut oauth_error = None;
        for pair in query.split('&') {
            let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
            match key {
                "code" if !value.is_empty() => code = Some(urldecode(value)),
                "state" if !value.is_empty() => state = Some(urldecode(value)),
                "error" if !value.is_empty() => oauth_error = Some(urldecode(value)),
                _ => {}
            }
        }
        if let Some(error) = oauth_error {
            bail!("Google refused the sign-in: {error}");
        }
        if state.as_deref() != Some(self.state.as_str()) {
            bail!("OAuth callback state did not match this login session");
        }
        code.context("OAuth callback carried no authorization code")
    }

    /// Wait up to `timeout` for Google's browser redirect on the loopback
    /// listener, and return the authorization code.
    pub async fn wait_for_callback(&self, timeout: Duration) -> Result<String> {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        tokio::time::timeout(timeout, async {
            loop {
                let accepted = tokio::select! {
                    res = self.listener.accept() => res,
                    res = async {
                        match &self.listener_v6 {
                            Some(v6) => v6.accept().await,
                            None => std::future::pending().await,
                        }
                    } => res,
                };
                let (mut stream, _) = accepted.context("OAuth callback listener closed")?;
                let mut buf = Vec::with_capacity(1024);
                let read_headers = tokio::time::timeout(CALLBACK_READ_TIMEOUT, async {
                    let mut chunk = [0u8; 1024];
                    while buf.len() < CALLBACK_MAX_BYTES {
                        let n = stream.read(&mut chunk).await?;
                        if n == 0 {
                            break;
                        }
                        buf.extend_from_slice(&chunk[..n]);
                        if buf.windows(4).any(|w| w == b"\r\n\r\n")
                            || buf.windows(2).any(|w| w == b"\n\n")
                        {
                            break;
                        }
                    }
                    Ok::<usize, std::io::Error>(buf.len())
                })
                .await;
                let Ok(Ok(read)) = read_headers else {
                    continue;
                };
                if read == 0 {
                    continue;
                }
                let request = String::from_utf8_lossy(&buf[..read]);
                let Some(target) = request
                    .lines()
                    .next()
                    .and_then(|line| line.split_whitespace().nth(1))
                else {
                    continue;
                };
                let (path, query) = target.split_once('?').unwrap_or((target, ""));
                if path != CALLBACK_PATH {
                    let _ = stream
                        .write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
                        .await;
                    continue;
                }
                match self.extract_query_code(query) {
                    Ok(code) => {
                        let body = "<!doctype html><meta charset=utf-8><title>tokeman</title>\
                            <p>Signed in to Antigravity for tokeman. You can close this tab.</p>";
                        let reply = format!(
                            "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\n\
                             Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                            body.len()
                        );
                        let _ = stream.write_all(reply.as_bytes()).await;
                        let _ = stream.flush().await;
                        return Ok(code);
                    }
                    Err(error) => {
                        let body = "<!doctype html><meta charset=utf-8><title>tokeman</title>\
                            <p>Sign-in did not complete. Return to the terminal for details.</p>";
                        let reply = format!(
                            "HTTP/1.1 400 Bad Request\r\nContent-Type: text/html; charset=utf-8\r\n\
                             Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                            body.len()
                        );
                        let _ = stream.write_all(reply.as_bytes()).await;
                        let _ = stream.flush().await;
                        if error.to_string().starts_with("Google refused the sign-in:") {
                            return Err(error);
                        }
                        continue;
                    }
                }
            }
        })
        .await
        .map_err(|_| {
            anyhow::anyhow!(
                "timed out after {}s waiting for the Google sign-in callback",
                timeout.as_secs()
            )
        })?
    }
}

/// Trade an authorization code from [`LoginSession`] for a [`ServiceAccount`].
pub async fn exchange_code(
    client: &Client,
    session: &LoginSession,
    code: &str,
    now_ms: i64,
) -> Result<ServiceAccount> {
    exchange_code_at(client, TOKEN_URL, session, code, now_ms).await
}

async fn exchange_code_at(
    client: &Client,
    token_url: &str,
    session: &LoginSession,
    code: &str,
    now_ms: i64,
) -> Result<ServiceAccount> {
    let response = client
        .post(token_url)
        .timeout(REQUEST_TIMEOUT)
        .form(&[
            ("grant_type", "authorization_code"),
            ("code", code),
            ("client_id", CLIENT_ID),
            ("client_secret", CLIENT_SECRET),
            ("redirect_uri", session.redirect_uri()),
            ("code_verifier", session.verifier()),
        ])
        .send()
        .await
        .context("token exchange request failed")?;
    let status = response.status();
    let text = response
        .text()
        .await
        .context("token exchange response could not be read")?;
    if !status.is_success() {
        bail!(
            "Google refused the authorization code (HTTP {}): {}",
            status.as_u16(),
            error_message(&text)
        );
    }
    let received: CodeGrant =
        serde_json::from_str(&text).context("token exchange answer was not JSON")?;
    let refresh_token = received
        .refresh_token
        .filter(|token| !token.is_empty())
        .context("Google returned no refresh token; try again and approve offline access")?;
    if received.access_token.is_empty() {
        bail!("Google returned an empty access_token");
    }
    let grant = Grant {
        access_token: received.access_token,
        expires_in: received.expires_in,
        id_token: received.id_token,
    };
    let email = grant
        .email()
        .context("Google's token response did not name an account email")?;
    let expires_at = now_ms.saturating_add(grant.expires_in.unwrap_or(3599).saturating_mul(1000));
    Ok(account(
        email,
        refresh_token,
        Some((grant.access_token, expires_at)),
    ))
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

pub(super) fn account(
    email: String,
    refresh_token: String,
    access: Option<(String, i64)>,
) -> ServiceAccount {
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
pub(super) fn parse_cli_grant(text: &str) -> Result<Option<ServiceAccount>> {
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
    match read_if_present(&seat::token_file())? {
        Some(text) => parse_cli_grant(&text),
        None => Ok(None),
    }
}

fn cli_keychain_grant() -> Result<Option<ServiceAccount>> {
    match seat::read_slot()? {
        Some(text) => parse_cli_grant(&text),
        None => Ok(None),
    }
}

/// The Antigravity app's (VS Code fork) global state database.
fn app_state_db() -> Option<PathBuf> {
    Some(crate::paths::platform_config_dir()?.join("Antigravity/User/globalStorage/state.vscdb"))
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
    let access = (!token.access_token.is_empty() && token.expiry > 0).then(|| {
        (
            token.access_token.clone(),
            token.expiry.saturating_mul(1000),
        )
    });
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
        fields.iter().rev().find_map(|(n, field)| match field {
            Field::Bytes(raw) if *n == number => Some(*raw),
            _ => None,
        })
    }

    pub fn string(fields: &[(u64, Field<'_>)], number: u64) -> Option<String> {
        String::from_utf8(bytes(fields, number)?.to_vec()).ok()
    }

    pub fn varint(fields: &[(u64, Field<'_>)], number: u64) -> Option<u64> {
        fields.iter().rev().find_map(|(n, field)| match field {
            Field::Varint(value) if *n == number => Some(*value),
            _ => None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
        // Field 7 holding something that is not an address names nobody.
        let odd = state_item(&[("userStatusSentinelKey", bytes_field(7, b"signed-out"))]);
        assert_eq!(vscdb::user_email(&odd), None);
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
        // A grant that names no account is refused, not kept under no name.
        let error = parse_cli_grant(r#"{"token":{"refresh_token":"1//r"},"id_token_email":""}"#)
            .unwrap_err()
            .to_string();
        assert!(error.contains("names no account"), "{error}");
        assert!(parse_cli_grant("go-keyring-base64:%%%").is_err());
    }

    #[tokio::test]
    async fn browser_login_catches_the_loopback_callback_checks_state_and_exchanges_pkce() {
        use axum::routing::post;
        use axum::{Form, Json, Router};
        use base64::Engine;

        let session = begin_login(Some("new@example.com")).await.unwrap();
        assert!(session.url().starts_with(AUTHORIZE_URL));
        assert!(session.url().contains("login_hint=new%40example.com"));
        assert!(session.url().contains("access_type=offline"));
        assert!(session.url().contains("code_challenge_method=S256"));
        // Formatting a session never leaks its PKCE verifier.
        let shown = format!("{session:?}");
        assert!(!shown.contains(session.verifier()), "{shown}");

        // Pasted callback URLs validate state and extract the code.
        let good_url = format!(
            "{}?code=pasted%2Fcode&state={}&scope=openid",
            session.redirect_uri(),
            session.state
        );
        assert_eq!(
            session.parse_callback_input(&good_url).unwrap(),
            "pasted/code"
        );
        assert!(
            session
                .parse_callback_input(&format!("{}?code=x&state=wrong", session.redirect_uri()))
                .is_err()
        );
        assert!(
            session
                .parse_callback_input(&format!(
                    "{}?error=access_denied&state={}",
                    session.redirect_uri(),
                    session.state
                ))
                .is_err()
        );

        // Live loopback redirect: unrelated paths (favicon) are ignored first.
        let redirect = session.redirect_uri().to_owned();
        let base = redirect.strip_suffix(CALLBACK_PATH).unwrap().to_owned();
        let state = session.state.clone();
        let browser = tokio::spawn(async move {
            let client = Client::new();
            let _ = client.get(format!("{base}/favicon.ico")).send().await;
            let resp = client
                .get(format!("{redirect}?code=loopback-code&state={state}"))
                .send()
                .await
                .unwrap();
            assert!(resp.status().is_success());
        });
        let code = session
            .wait_for_callback(Duration::from_secs(5))
            .await
            .unwrap();
        browser.await.unwrap();
        assert_eq!(code, "loopback-code");

        // Exchange against a fake Google token server verifying PKCE fields.
        let expected_verifier = session.verifier().to_owned();
        let expected_redirect = session.redirect_uri().to_owned();
        let app = Router::new().route(
            "/token",
            post(move |Form(form): Form<Vec<(String, String)>>| {
                let get = |k: &str| {
                    form.iter()
                        .find(|(key, _)| key == k)
                        .map(|(_, v)| v.clone())
                };
                assert_eq!(get("grant_type").as_deref(), Some("authorization_code"));
                assert_eq!(get("client_id").as_deref(), Some(CLIENT_ID));
                assert_eq!(get("client_secret").as_deref(), Some(CLIENT_SECRET));
                assert_eq!(get("code").as_deref(), Some("loopback-code"));
                assert_eq!(
                    get("redirect_uri").as_deref(),
                    Some(expected_redirect.as_str())
                );
                assert_eq!(
                    get("code_verifier").as_deref(),
                    Some(expected_verifier.as_str())
                );
                let claims = base64::engine::general_purpose::URL_SAFE_NO_PAD
                    .encode(r#"{"email":"new@example.com"}"#);
                async move {
                    Json(serde_json::json!({
                        "access_token": "ya29.new",
                        "refresh_token": "1//new",
                        "expires_in": 3599,
                        "id_token": format!("h.{claims}.s"),
                    }))
                }
            }),
        );
        let token_base = crate::testing::serve(app).await;
        let stored = exchange_code_at(
            &Client::new(),
            &format!("{token_base}/token"),
            &session,
            &code,
            1_000_000,
        )
        .await
        .unwrap();
        assert_eq!(stored.name, "new@example.com");
        assert_eq!(stored.refresh_token.as_deref(), Some("1//new"));
        assert_eq!(stored.access_token.as_deref(), Some("ya29.new"));
        assert_eq!(stored.expires_at, Some(1_000_000 + 3_599_000));
    }
}
