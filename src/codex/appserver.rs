//! JSON-RPC over Codex's app-server control socket (WebSocket over a Unix
//! socket). A running Codex caches its auth and re-reads it only on
//! `reload()`; `account/login/start` writes the credentials and calls the
//! unguarded reload, so the next request carries the new identity.

use anyhow::{Context, Result, anyhow, bail};
use futures::{SinkExt, StreamExt};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::net::UnixStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{WebSocketStream, client_async};

use crate::quota::Span;

const CONTROL_SOCKET_DIR: &str = "app-server-control";
const CONTROL_SOCKET_FILE: &str = "app-server-control.sock";
const RESPONSE_TIMEOUT: Duration = Duration::from_secs(30);
/// A socket file nothing listens behind must not hold a caller up.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const INITIALIZE_REQUEST_ID: i64 = 0;

/// Where a `CODEX_HOME` exposes its control socket. Not `ipc/ipc.sock`, which
/// is the TUI's IDE-context socket and speaks another protocol.
pub fn control_socket_path(codex_home: &Path) -> PathBuf {
    codex_home
        .join(CONTROL_SOCKET_DIR)
        .join(CONTROL_SOCKET_FILE)
}

/// What a running Codex reports about its account, from `account/read`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct AccountInfo {
    pub email: Option<String>,
    pub plan_type: Option<String>,
}

impl AccountInfo {
    /// The first `email` and `planType` anywhere in the response.
    pub fn from_value(value: &Value) -> Self {
        let first = |key: &str| {
            let mut found = Vec::new();
            json_values(value, key, &mut found);
            found.first().and_then(|v| v.as_str()).map(str::to_owned)
        };
        Self {
            email: first("email"),
            plan_type: first("planType"),
        }
    }
}

impl std::fmt::Display for AccountInfo {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} ({})",
            self.email.as_deref().unwrap_or("?"),
            self.plan_type.as_deref().unwrap_or("?")
        )
    }
}

pub struct AppServer {
    socket: WebSocketStream<UnixStream>,
    next_id: i64,
}

impl AppServer {
    /// Connect and initialize. The experimental capability gates
    /// `chatgptAuthTokens`.
    pub async fn connect(socket_path: &Path) -> Result<Self> {
        let stream = tokio::time::timeout(CONNECT_TIMEOUT, UnixStream::connect(socket_path))
            .await
            .map_err(|_| anyhow!("timed out"))
            .and_then(|connected| connected.map_err(anyhow::Error::new))
            .with_context(|| {
                format!(
                    "failed to connect to {} (is a Codex app-server running? try `codex remote-control start`)",
                    socket_path.display()
                )
            })?;
        let (socket, _response) =
            tokio::time::timeout(CONNECT_TIMEOUT, client_async("ws://localhost/", stream))
                .await
                .map_err(|_| anyhow!("timed out"))
                .and_then(|upgraded| upgraded.map_err(anyhow::Error::new))
                .with_context(|| format!("failed to upgrade {}", socket_path.display()))?;
        let mut server = Self {
            socket,
            next_id: INITIALIZE_REQUEST_ID + 1,
        };
        server.initialize().await?;
        Ok(server)
    }

    async fn initialize(&mut self) -> Result<()> {
        self.send(&json!({
            "id": INITIALIZE_REQUEST_ID,
            "method": "initialize",
            "params": {
                "clientInfo": {
                    "name": "tokeman",
                    "title": "tokeman account rotator",
                    "version": env!("CARGO_PKG_VERSION"),
                },
                "capabilities": { "experimentalApi": true },
            },
        }))
        .await?;
        self.await_response(INITIALIZE_REQUEST_ID).await?;
        // Required before the server services further requests.
        self.send(&json!({ "method": "initialized" })).await
    }

    async fn send(&mut self, message: &Value) -> Result<()> {
        self.socket
            .send(Message::Text(serde_json::to_string(message)?))
            .await
            .context("failed to write to control socket")
    }

    async fn read_message(&mut self) -> Result<Value> {
        loop {
            let frame = self
                .socket
                .next()
                .await
                .ok_or_else(|| anyhow!("app-server closed the control socket"))??;
            let Message::Text(payload) = frame else {
                continue;
            };
            return serde_json::from_str(&payload)
                .context("failed to parse app-server JSON-RPC message");
        }
    }

    /// Read until the response with `id` arrives, discarding notifications.
    async fn await_response(&mut self, id: i64) -> Result<Value> {
        let deadline = tokio::time::Instant::now() + RESPONSE_TIMEOUT;
        loop {
            let message = tokio::time::timeout_at(deadline, self.read_message())
                .await
                .context("timed out waiting for app-server response")??;
            if message.get("id").and_then(Value::as_i64) != Some(id) {
                continue;
            }
            if let Some(error) = message.get("error") {
                bail!("app-server returned an error: {error}");
            }
            return Ok(message.get("result").cloned().unwrap_or(Value::Null));
        }
    }

    pub async fn request(&mut self, method: &str, params: Value) -> Result<Value> {
        let id = self.next_id;
        self.next_id += 1;
        self.send(&json!({ "id": id, "method": method, "params": params }))
            .await?;
        self.await_response(id).await
    }

    /// Move the running Codex onto another account. `access_token` must be
    /// live: this call does not refresh.
    pub async fn reseat(
        &mut self,
        access_token: &str,
        chatgpt_account_id: &str,
        plan_type: Option<&str>,
    ) -> Result<Value> {
        self.request(
            "account/login/start",
            json!({
                "type": "chatgptAuthTokens",
                "accessToken": access_token,
                "chatgptAccountId": chatgpt_account_id,
                "chatgptPlanType": plan_type,
            }),
        )
        .await
    }

    /// The account-wide limits as the running Codex sees them.
    pub async fn rate_limits(&mut self) -> Result<ReportedLimits> {
        self.request("account/rateLimits/read", json!({}))
            .await
            .map(|value| ReportedLimits::from_value(&value))
    }

    /// The account the running Codex is using. `chatgptAuthTokens` installs
    /// in-memory auth and leaves auth.json alone, so disk is no evidence.
    pub async fn read_account(&mut self) -> Result<AccountInfo> {
        self.request("account/read", json!({}))
            .await
            .map(|value| AccountInfo::from_value(&value))
    }

    /// Block until the server pushes a notification for `method`.
    pub async fn next_notification(&mut self, method: &str) -> Result<Value> {
        loop {
            let message = self.read_message().await?;
            if message.get("method").and_then(Value::as_str) == Some(method) {
                return Ok(message.get("params").cloned().unwrap_or(Value::Null));
            }
        }
    }

    pub async fn close(mut self) {
        let _ = self.socket.close(None).await;
    }
}

/// Every value stored under `key` anywhere in `value`, each object's own
/// entry before those nested inside it.
pub fn json_values<'a>(value: &'a Value, key: &str, out: &mut Vec<&'a Value>) {
    match value {
        Value::Object(map) => {
            out.extend(map.get(key));
            for nested in map.values() {
                json_values(nested, key, out);
            }
        }
        Value::Array(items) => {
            for item in items {
                json_values(item, key, out);
            }
        }
        _ => {}
    }
}

/// One account-wide window as a running Codex reports it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ReportedWindow {
    pub span: Span,
    /// Fraction used; may exceed 1.0.
    pub used: f64,
    /// Epoch seconds; 0 when none was sent.
    pub resets_at: i64,
}

/// The account-wide view in an `account/rateLimits/read` answer or an
/// `account/rateLimits/updated` push.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct ReportedLimits {
    /// `rateLimits.primary` and `.secondary`, named by their length. The
    /// per-feature buckets of `rateLimitsByLimitId` are metered apart and
    /// are not here.
    pub windows: Vec<ReportedWindow>,
    /// The backend refuses ordinary work on the account.
    pub refused: bool,
    /// The account the snapshot is of, when the backend said.
    pub account_id: Option<String>,
}

impl ReportedLimits {
    pub fn from_value(value: &Value) -> Self {
        let snapshot = value.get("rateLimits").unwrap_or(value);
        let either = |map: &Value, camel: &str, snake: &str| -> Option<Value> {
            map.get(camel).or_else(|| map.get(snake)).cloned()
        };
        let windows = ["primary", "secondary"]
            .into_iter()
            .filter_map(|slot| snapshot.get(slot))
            .filter_map(|window| {
                let used = either(window, "usedPercent", "used_percent")?.as_f64()?;
                let minutes = either(window, "windowDurationMins", "window_minutes")
                    .and_then(|minutes| minutes.as_i64());
                let resets_at = either(window, "resetsAt", "resets_at")
                    .and_then(|at| at.as_i64())
                    .unwrap_or(0);
                Some(ReportedWindow {
                    span: Span::from_seconds(minutes.map(|minutes| minutes * 60)),
                    used: used / 100.0,
                    resets_at,
                })
            })
            .collect();
        let reached = snapshot
            .get("rateLimitReachedType")
            .is_some_and(|reached| !reached.is_null());
        let disallowed = value.get("ordinaryUsageAllowed").and_then(Value::as_bool) == Some(false);
        Self {
            windows,
            refused: reached || disallowed,
            account_id: value
                .get("accountId")
                .and_then(Value::as_str)
                .map(str::to_owned),
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::sync::{Arc, Mutex};
    use tokio::net::UnixListener;

    /// What the fake app-server holds: the account it runs on, each
    /// account's `account/rateLimits/read` answer, and every
    /// `account/login/start` it was sent.
    #[derive(Debug, Default)]
    pub(crate) struct Running {
        /// `chatgptAccountId` of the account in use.
        pub account_id: String,
        /// Per account id: its email and its rate-limit answer.
        pub accounts: BTreeMap<String, (String, Value)>,
        pub logins: Vec<Value>,
        /// Answer `account/rateLimits/read` with an error, as a Codex whose
        /// token the backend rejects does.
        pub limits_fail: bool,
    }

    /// A Codex app-server on a control socket under a short-lived home.
    /// Socket paths are short on every platform, so the home is under `/tmp`.
    pub(crate) struct FakeAppServer {
        pub home: tempfile::TempDir,
        pub running: Arc<Mutex<Running>>,
    }

    impl FakeAppServer {
        /// Serve `accounts` (`(account id, email, rate-limit answer)`), running on the first.
        pub fn start(accounts: Vec<(&str, &str, Value)>) -> Self {
            let home = tempfile::Builder::new()
                .prefix("tk")
                .tempdir_in("/tmp")
                .unwrap();
            let socket = control_socket_path(home.path());
            std::fs::create_dir_all(socket.parent().unwrap()).unwrap();
            let running = Arc::new(Mutex::new(Running {
                account_id: accounts[0].0.to_owned(),
                accounts: accounts
                    .into_iter()
                    .map(|(id, email, limits)| (id.to_owned(), (email.to_owned(), limits)))
                    .collect(),
                logins: Vec::new(),
                limits_fail: false,
            }));
            let listener = UnixListener::bind(&socket).unwrap();
            let state = running.clone();
            tokio::spawn(async move {
                while let Ok((stream, _)) = listener.accept().await {
                    tokio::spawn(serve(stream, state.clone()));
                }
            });
            Self { home, running }
        }

        pub fn logins(&self) -> Vec<Value> {
            self.running.lock().unwrap().logins.clone()
        }

        pub fn account_id(&self) -> String {
            self.running.lock().unwrap().account_id.clone()
        }

        /// Replace the rate-limit answer of the account in use.
        pub fn set_limits(&self, limits: Value) {
            let mut running = self.running.lock().unwrap();
            let id = running.account_id.clone();
            running.accounts.get_mut(&id).unwrap().1 = limits;
        }
    }

    async fn serve(stream: UnixStream, state: Arc<Mutex<Running>>) {
        let Ok(mut socket) = tokio_tungstenite::accept_async(stream).await else {
            return;
        };
        while let Some(Ok(frame)) = socket.next().await {
            let Message::Text(payload) = frame else {
                continue;
            };
            let request: Value = serde_json::from_str(&payload).unwrap();
            let Some(id) = request.get("id").cloned() else {
                continue;
            };
            let failed =
                request["method"] == "account/rateLimits/read" && state.lock().unwrap().limits_fail;
            if failed {
                let answer = json!({"id": id, "error": {
                    "code": -32603,
                    "message": "failed to fetch codex rate limits: 401 Unauthorized",
                }});
                if socket
                    .send(Message::Text(answer.to_string()))
                    .await
                    .is_err()
                {
                    return;
                }
                continue;
            }
            let result = {
                let mut running = state.lock().unwrap();
                match request["method"].as_str().unwrap_or_default() {
                    "account/rateLimits/read" => {
                        let mut limits = running.accounts[&running.account_id].1.clone();
                        limits["accountId"] = json!(running.account_id);
                        limits
                    }
                    "account/read" => json!({"account": {
                        "type": "chatgpt",
                        "email": running.accounts[&running.account_id].0,
                        "planType": "plus",
                    }}),
                    "account/login/start" => {
                        let params = request["params"].clone();
                        if let Some(id) = params["chatgptAccountId"].as_str()
                            && running.accounts.contains_key(id)
                        {
                            running.account_id = id.to_owned();
                        }
                        // A new token is one the backend accepts.
                        running.limits_fail = false;
                        running.logins.push(params);
                        json!({"type": "chatgptAuthTokens"})
                    }
                    _ => json!({}),
                }
            };
            let answer = json!({"id": id, "result": result});
            if socket
                .send(Message::Text(answer.to_string()))
                .await
                .is_err()
            {
                return;
            }
        }
    }

    /// A rate-limit answer with a 5-hour and a weekly window, in percent used.
    pub(crate) fn limits_answer(session_used: f64, weekly_used: f64) -> Value {
        json!({
            "rateLimits": {
                "primary": {"usedPercent": session_used, "windowDurationMins": 300, "resetsAt": 1_800_000_000},
                "secondary": {"usedPercent": weekly_used, "windowDurationMins": 10080, "resetsAt": 1_800_500_000},
                "rateLimitReachedType": null,
            },
            "ordinaryUsageAllowed": true,
        })
    }

    #[tokio::test]
    async fn the_client_reads_limits_and_the_account_and_a_login_moves_the_server() {
        let fake = FakeAppServer::start(vec![
            ("acct-a", "a@x.y", limits_answer(90.0, 10.0)),
            ("acct-b", "b@x.y", limits_answer(5.0, 5.0)),
        ]);
        let socket = control_socket_path(fake.home.path());
        let mut server = AppServer::connect(&socket).await.unwrap();
        let limits = server.rate_limits().await.unwrap();
        assert_eq!(limits.account_id.as_deref(), Some("acct-a"));
        assert_eq!(limits.windows[0].used, 0.9);
        assert_eq!(
            server.read_account().await.unwrap().email.as_deref(),
            Some("a@x.y")
        );
        server
            .reseat("token-b", "acct-b", Some("plus"))
            .await
            .unwrap();
        assert_eq!(
            server.read_account().await.unwrap().email.as_deref(),
            Some("b@x.y")
        );
        server.close().await;
        assert_eq!(
            fake.logins(),
            vec![json!({
                "type": "chatgptAuthTokens",
                "accessToken": "token-b",
                "chatgptAccountId": "acct-b",
                "chatgptPlanType": "plus",
            })]
        );
        // A socket file with nothing behind it fails within the connect bound.
        let dead = tempfile::Builder::new()
            .prefix("tk")
            .tempdir_in("/tmp")
            .unwrap();
        let path = control_socket_path(dead.path());
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        drop(std::os::unix::net::UnixListener::bind(&path).unwrap());
        assert!(AppServer::connect(&path).await.is_err());
    }

    #[test]
    fn the_account_wide_windows_are_read_and_named_by_length() {
        let limits = ReportedLimits::from_value(&json!({
            "accountId": "acct-1",
            "ordinaryUsageAllowed": true,
            "rateLimits": {
                "primary": {"usedPercent": 30, "windowDurationMins": 300, "resetsAt": 1_800_000_000},
                "secondary": {"usedPercent": 75, "windowDurationMins": 10080, "resetsAt": null},
                "rateLimitReachedType": null,
            },
            // A per-feature bucket at its wall is not the account's.
            "rateLimitsByLimitId": {
                "codex_spark": {"primary": {"usedPercent": 100, "windowDurationMins": 300}},
            },
        }));
        assert_eq!(
            limits.windows,
            vec![
                ReportedWindow {
                    span: Span::FiveHour,
                    used: 0.30,
                    resets_at: 1_800_000_000
                },
                ReportedWindow {
                    span: Span::Weekly,
                    used: 0.75,
                    resets_at: 0
                },
            ]
        );
        assert!(!limits.refused);
        assert_eq!(limits.account_id.as_deref(), Some("acct-1"));
        // Weekly-only: the weekly window arrives as `primary`. A push carries
        // the snapshot alone.
        let weekly_only = ReportedLimits::from_value(&json!({
            "rateLimits": {
                "primary": {"usedPercent": 42, "windowDurationMins": 10080, "resetsAt": 1},
                "secondary": null,
            }
        }));
        assert_eq!(weekly_only.windows.len(), 1);
        assert_eq!(weekly_only.windows[0].span, Span::Weekly);
        assert!((weekly_only.windows[0].used - 0.42).abs() < 1e-9);
        assert_eq!(ReportedLimits::from_value(&json!({"other": 1})).windows, []);
        let refused = ReportedLimits::from_value(&json!({
            "ordinaryUsageAllowed": false,
            "rateLimits": {"primary": {"usedPercent": 150}},
        }));
        assert!(refused.refused);
        assert_eq!(refused.windows[0].span, Span::Unknown);
        assert_eq!(refused.windows[0].used, 1.5);
        let reached = ReportedLimits::from_value(&json!({
            "rateLimits": {"rateLimitReachedType": "rate_limit_reached"},
        }));
        assert!(reached.refused);
    }

    #[test]
    fn account_info_takes_the_outermost_email_and_plan() {
        let info = AccountInfo::from_value(&json!({
            "account": {"email": "outer@x.y", "planType": "pro", "nested": {"email": "inner@x.y"}}
        }));
        assert_eq!(info.email.as_deref(), Some("outer@x.y"));
        assert_eq!(info.to_string(), "outer@x.y (pro)");
        assert_eq!(AccountInfo::default().to_string(), "? (?)");
    }

    #[test]
    fn the_control_socket_is_not_the_ipc_socket() {
        let path = control_socket_path(Path::new("/h/.codex"));
        assert_eq!(
            path,
            PathBuf::from("/h/.codex/app-server-control/app-server-control.sock")
        );
    }
}
