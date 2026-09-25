//! Client for Codex's app-server control socket.
//!
//! This is how a *running* Codex — the desktop GUI or a `codex remote-control`
//! daemon — gets migrated to another account without losing its session. The
//! transport is JSON-RPC over WebSocket over a Unix socket, matching
//! `app-server-daemon/src/client.rs`.
//!
//! Why go through the app-server at all instead of just rewriting `auth.json`:
//! a live Codex caches its auth and only re-reads on an explicit `reload()`.
//! Rewriting the file behind its back leaves it on the stale account until it
//! 401s, and the 401 recovery path checks account-id equality
//! (`login/src/auth/manager.rs:2361`) and fails permanently on mismatch.
//! `account/login/start` writes the credentials *and* calls the unguarded
//! `reload()`, so the next request picks up the new identity cleanly.

use anyhow::{Context, Result, anyhow, bail};
use futures::{SinkExt, StreamExt};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::net::UnixStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{WebSocketStream, client_async};

/// `app-server-transport/src/transport/mod.rs:54-55`.
const CONTROL_SOCKET_DIR: &str = "app-server-control";
const CONTROL_SOCKET_FILE: &str = "app-server-control.sock";

const RESPONSE_TIMEOUT: Duration = Duration::from_secs(30);
const INITIALIZE_REQUEST_ID: i64 = 0;

/// Where a given `CODEX_HOME` exposes its control socket. Note this is *not*
/// `$CODEX_HOME/ipc/ipc.sock`, which is the TUI's IDE-context socket and speaks
/// a different protocol.
pub fn control_socket_path(codex_home: &Path) -> PathBuf {
    codex_home
        .join(CONTROL_SOCKET_DIR)
        .join(CONTROL_SOCKET_FILE)
}

pub struct AppServer {
    socket: WebSocketStream<UnixStream>,
    next_id: i64,
}

impl AppServer {
    /// Connect and perform the initialize handshake.
    ///
    /// `experimental_api` must be true to use `chatgptAuthTokens`, which is
    /// gated behind the experimental capability.
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

        // The server expects this before it will service further requests.
        self.send(&json!({ "method": "initialized" })).await
    }

    async fn send(&mut self, message: &Value) -> Result<()> {
        self.socket
            .send(Message::Text(serde_json::to_string(message)?.into()))
            .await
            .context("failed to write to control socket")?;
        Ok(())
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

    /// Migrate the running Codex onto a different account.
    ///
    /// `access_token` must be a live ChatGPT access token carrying the same
    /// scopes Codex-managed tokens have — refresh it first if it is anywhere
    /// near expiry, because this call does not perform a refresh.
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

    /// Current rate limits as the running Codex sees them.
    pub async fn rate_limits(&mut self) -> Result<Value> {
        self.request("account/rateLimits/read", json!({})).await
    }

    /// The account the running Codex is currently using.
    ///
    /// Worth calling after a re-seat: `chatgptAuthTokens` installs *external*
    /// auth, an in-memory slot that deliberately leaves `auth.json` alone, so
    /// the profile on disk is not evidence either way about what the running
    /// process is actually using.
    pub async fn read_account(&mut self) -> Result<Value> {
        self.request("account/read", json!({})).await
    }

    /// Block until the app-server pushes a notification matching `method`.
    /// Used to follow `account/rateLimits/updated` without polling.
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
