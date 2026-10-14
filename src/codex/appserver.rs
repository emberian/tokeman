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

const CONTROL_SOCKET_DIR: &str = "app-server-control";
const CONTROL_SOCKET_FILE: &str = "app-server-control.sock";
const RESPONSE_TIMEOUT: Duration = Duration::from_secs(30);
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
        let stream = UnixStream::connect(socket_path).await.with_context(|| {
            format!(
                "failed to connect to {} (is a Codex app-server running? try `codex remote-control start`)",
                socket_path.display()
            )
        })?;
        let (socket, _response) = client_async("ws://localhost/", stream)
            .await
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

    /// The limits as the running Codex sees them.
    pub async fn rate_limits(&mut self) -> Result<Value> {
        self.request("account/rateLimits/read", json!({})).await
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

/// The remaining fraction of the fullest window in a rate-limits payload,
/// whatever its nesting; `None` when it reports no `usedPercent`.
pub fn remaining_from_rate_limits(value: &Value) -> Option<f64> {
    let mut used = Vec::new();
    json_values(value, "usedPercent", &mut used);
    json_values(value, "used_percent", &mut used);
    used.into_iter()
        .filter_map(Value::as_f64)
        .reduce(f64::max)
        .map(|worst| (1.0 - worst / 100.0).clamp(0.0, 1.0))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remaining_is_the_fullest_window_at_any_depth() {
        let limits = json!({
            "rateLimits": {
                "primary": {"usedPercent": 30.0},
                "secondary": {"used_percent": 75},
                "extra": [{"rateLimit": {"primary": {"usedPercent": 10}}}],
            }
        });
        assert_eq!(remaining_from_rate_limits(&limits), Some(0.25));
        assert_eq!(remaining_from_rate_limits(&json!({"other": 1})), None);
        assert_eq!(
            remaining_from_rate_limits(&json!({"usedPercent": 150})),
            Some(0.0)
        );
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
