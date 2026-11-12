//! The running clients that share `agy`'s login slot, and what each can be
//! told about a new login.
//!
//! - The Antigravity hub app writes its ports and a CSRF token to
//!   `~/.gemini/antigravity/daemon/hub_<port>.json`. Its `GetAuthStatus` RPC
//!   makes it read the slot again; `GetUserStatus` says which account it is on.
//! - An `agy acp` server (started by an editor) answers `GetUserStatus` on a
//!   local port without a token. It signs in again only when its own ACP
//!   client asks, so tokeman can ask which account it holds and no more.
//! - An interactive `agy` has no reachable control surface.
//! - A print-mode run (`agy -p ...`) signs in from the slot when it starts
//!   and exits; the next run follows a switch by itself.
//!
//! `agy` appends each prompt to `~/.gemini/antigravity-cli/history.jsonl` as
//! `{"conversationId", "display", "timestamp", "workspace"}` (the id is
//! absent on some lines); the last id recorded for a working directory is
//! the conversation to resume there.
//!
//! An `agy` whose environment sets `HOME` to a per-account home under
//! [`paths::agy_homes`] was started by `tokeman agy run`: it keeps its login in
//! a file there, never in the slot, so it is a [`Session`], not a client.
//!
//! Everything here reads: `ps` and `lsof` with bounded waits, the daemon
//! files, and those two RPCs. No process is signalled, and no RPC that logs a
//! client out is ever sent (a logout deletes the slot for every client).

use std::fmt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::paths;

const CENSUS_TIMEOUT: Duration = Duration::from_secs(if cfg!(test) { 2 } else { 10 });
const LSOF_TIMEOUT: Duration = Duration::from_secs(if cfg!(test) { 2 } else { 5 });
const RPC_TIMEOUT: Duration = Duration::from_secs(if cfg!(test) { 2 } else { 5 });
const RPC_SERVICE: &str = "exa.language_server_pb.LanguageServerService";
const CSRF_HEADER: &str = "x-codeium-csrf-token";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ClientKind {
    /// The Antigravity app's language server.
    Hub,
    /// `agy acp`, an editor's agent server.
    Acp,
    /// `agy` at a terminal.
    Interactive,
    /// `agy -p ...`: one prompt, then it exits.
    Print,
}

impl ClientKind {
    pub fn label(self) -> &'static str {
        match self {
            Self::Hub => "Antigravity app",
            Self::Acp => "agy acp",
            Self::Interactive => "interactive agy",
            Self::Print => "agy -p",
        }
    }
}

/// The hub's CSRF token; never shown.
#[derive(Clone, PartialEq, Eq)]
struct Csrf(String);

impl fmt::Debug for Csrf {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Csrf(..)")
    }
}

/// One running client of the slot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Running {
    pub kind: ClientKind,
    pub pid: u32,
    pub cwd: Option<PathBuf>,
    /// Local ports its RPC may answer on.
    ports: Vec<u16>,
    csrf: Option<Csrf>,
}

/// Where the census looks; tests point every field at doubles.
#[derive(Debug, Clone)]
pub(super) struct Lookout {
    pub ps: PathBuf,
    pub lsof: PathBuf,
    pub daemon_dir: PathBuf,
    /// `agy`'s prompt history.
    pub history: PathBuf,
    /// Where `tokeman agy run` keeps its per-account homes.
    pub homes: PathBuf,
}

impl Lookout {
    pub fn live() -> Self {
        Self {
            ps: "/bin/ps".into(),
            lsof: "/usr/sbin/lsof".into(),
            daemon_dir: paths::expand_tilde("~/.gemini/antigravity/daemon"),
            history: paths::expand_tilde("~/.gemini/antigravity-cli/history.jsonl"),
            homes: paths::agy_homes(),
        }
    }
}

/// Run a read-only tool with a bounded wait; the child is dropped (and with
/// it killed) when the wait ends.
async fn output(bin: &Path, args: &[&str], timeout: Duration) -> Result<String> {
    let run = tokio::process::Command::new(bin)
        .args(args)
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true)
        .output();
    let output = tokio::time::timeout(timeout, run)
        .await
        .with_context(|| {
            format!(
                "{} did not answer within {}s",
                bin.display(),
                timeout.as_secs()
            )
        })?
        .with_context(|| format!("could not run {}", bin.display()))?;
    if !output.status.success() {
        bail!("{} exited with {}", bin.display(), output.status);
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// `pid command` lines of `ps -o pid=,<column>=`.
fn pid_lines(text: &str) -> impl Iterator<Item = (u32, &str)> {
    text.lines().filter_map(|line| {
        let line = line.trim();
        let (pid, rest) = line.split_once(char::is_whitespace).unwrap_or((line, ""));
        Some((pid.parse().ok()?, rest.trim()))
    })
}

fn is_agy(command: &str) -> bool {
    command == "agy" || command.ends_with("/agy")
}

/// What an `agy` command line is: an `acp` word makes it an editor's agent
/// server, a print flag a one-prompt run, anything else interactive.
fn classify(args: &str) -> ClientKind {
    let mut words = args.split_whitespace().skip(1);
    if words.clone().any(|word| word == "acp") {
        return ClientKind::Acp;
    }
    if words.any(|word| word == "-p" || word == "--print" || word.starts_with("--print=")) {
        return ClientKind::Print;
    }
    ClientKind::Interactive
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct HubFile {
    pid: u32,
    http_port: u16,
    #[serde(default)]
    csrf_token: Option<String>,
}

/// The hub daemon files whose process is among `alive`.
fn hubs(daemon_dir: &Path, alive: &[u32]) -> Vec<Running> {
    let Ok(entries) = std::fs::read_dir(daemon_dir) else {
        return Vec::new();
    };
    let mut hubs: Vec<Running> = entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("hub_") && name.ends_with(".json"))
        })
        .filter_map(|path| {
            let file: HubFile = serde_json::from_slice(&std::fs::read(path).ok()?).ok()?;
            alive.contains(&file.pid).then(|| Running {
                kind: ClientKind::Hub,
                pid: file.pid,
                cwd: None,
                ports: vec![file.http_port],
                csrf: file.csrf_token.map(Csrf),
            })
        })
        .collect();
    hubs.sort_by_key(|hub| hub.pid);
    hubs.dedup_by_key(|hub| hub.pid);
    hubs
}

/// A history file larger than this is not read for a resume hint.
const HISTORY_MAX_BYTES: u64 = 16 * 1024 * 1024;

#[derive(Deserialize)]
struct HistoryLine {
    #[serde(default, rename = "conversationId")]
    conversation_id: Option<String>,
    #[serde(default)]
    workspace: Option<PathBuf>,
}

/// The conversation `agy` last recorded for the working directory `cwd`:
/// what `agy --conversation=<id>` resumes there. `None` when the history is
/// absent, in another shape, or names no conversation for that directory.
pub(super) fn latest_conversation(history: &Path, cwd: &Path) -> Option<String> {
    if std::fs::metadata(history).ok()?.len() > HISTORY_MAX_BYTES {
        return None;
    }
    let text = std::fs::read_to_string(history).ok()?;
    text.lines()
        .rev()
        .filter_map(|line| serde_json::from_str::<HistoryLine>(line).ok())
        .filter(|line| line.workspace.as_deref() == Some(cwd))
        .find_map(|line| line.conversation_id)
        // The id goes into a command line the user is told to run.
        .filter(|id| {
            !id.is_empty()
                && id.len() <= 128
                && id
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        })
}

/// `n`-prefixed fields of `lsof -Fn` output.
fn lsof_names(text: &str) -> impl Iterator<Item = &str> {
    text.lines().filter_map(|line| line.strip_prefix('n'))
}

async fn cwd(lookout: &Lookout, pid: u32) -> Option<PathBuf> {
    let pid = pid.to_string();
    let text = output(
        &lookout.lsof,
        &["-a", "-p", &pid, "-d", "cwd", "-Fn"],
        LSOF_TIMEOUT,
    )
    .await
    .ok()?;
    lsof_names(&text).next().map(PathBuf::from)
}

async fn listening_ports(lookout: &Lookout, pid: u32) -> Vec<u16> {
    let pid = pid.to_string();
    let Ok(text) = output(
        &lookout.lsof,
        &["-nP", "-a", "-p", &pid, "-iTCP", "-sTCP:LISTEN", "-Fn"],
        LSOF_TIMEOUT,
    )
    .await
    else {
        return Vec::new();
    };
    let mut ports: Vec<u16> = lsof_names(&text)
        .filter_map(|name| name.rsplit_once(':')?.1.parse().ok())
        .collect();
    ports.sort_unstable();
    ports.dedup();
    ports
}

/// An `agy` started by `tokeman agy run`, signed in from its own home.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Session {
    pub pid: u32,
    pub kind: ClientKind,
    /// The per-account home's directory name under [`paths::agy_homes`].
    pub home: String,
    pub cwd: Option<PathBuf>,
}

/// The per-account home named by a `ps -E` line: the directory under `homes`
/// that the environment's `HOME` points into. The environment follows the
/// arguments, so the last `HOME=` wins.
fn own_home(line: &str, homes: &Path) -> Option<String> {
    let prefix = format!(" HOME={}/", homes.display());
    let start = line.rfind(&prefix)? + prefix.len();
    let name: String = line[start..]
        .chars()
        .take_while(|c| !c.is_whitespace() && *c != '/')
        .collect();
    (!name.is_empty()).then_some(name)
}

/// Every running client of the slot, hubs first, then `agy` processes by pid.
/// An error is a census that could not be taken, never an empty list.
pub(super) async fn census(lookout: &Lookout) -> Result<Vec<Running>> {
    Ok(take(lookout).await?.0)
}

/// The `agy` processes running on a per-account home, by pid.
pub(super) async fn sessions(lookout: &Lookout) -> Result<Vec<Session>> {
    Ok(take(lookout).await?.1)
}

async fn take(lookout: &Lookout) -> Result<(Vec<Running>, Vec<Session>)> {
    let everything = output(&lookout.ps, &["-axo", "pid=,comm="], CENSUS_TIMEOUT).await?;
    let alive: Vec<u32> = pid_lines(&everything).map(|(pid, _)| pid).collect();
    let agy: Vec<u32> = pid_lines(&everything)
        .filter(|(_, command)| is_agy(command))
        .map(|(pid, _)| pid)
        .collect();
    let mut clients = hubs(&lookout.daemon_dir, &alive);
    let mut sessions = Vec::new();
    if agy.is_empty() {
        return Ok((clients, sessions));
    }
    let pids = agy.iter().map(u32::to_string).collect::<Vec<_>>().join(",");
    // macOS `ps -E` appends the environment of the user's own processes.
    let environment: &[&str] = if cfg!(target_os = "macos") {
        &["-E"]
    } else {
        &[]
    };
    let mut args = environment.to_vec();
    args.extend(["-ww", "-o", "pid=,args=", "-p", &pids]);
    let lines = output(&lookout.ps, &args, CENSUS_TIMEOUT).await?;
    let mut processes: Vec<(u32, ClientKind, Option<String>)> = pid_lines(&lines)
        .filter(|(pid, _)| agy.contains(pid))
        .map(|(pid, line)| (pid, classify(line), own_home(line, &lookout.homes)))
        .collect();
    processes.sort_by_key(|(pid, _, _)| *pid);
    for (pid, kind, home) in processes {
        if let Some(home) = home {
            sessions.push(Session {
                pid,
                kind,
                home,
                cwd: cwd(lookout, pid).await,
            });
            continue;
        }
        clients.push(Running {
            kind,
            pid,
            cwd: cwd(lookout, pid).await,
            ports: match kind {
                ClientKind::Acp => listening_ports(lookout, pid).await,
                _ => Vec::new(),
            },
            csrf: None,
        });
    }
    Ok((clients, sessions))
}

/// The client for loopback RPCs only: no proxy, short waits. Built once;
/// building one loads the system's trust store.
fn rpc_client() -> Result<&'static reqwest::Client> {
    static CLIENT: std::sync::OnceLock<Option<reqwest::Client>> = std::sync::OnceLock::new();
    CLIENT
        .get_or_init(|| {
            reqwest::Client::builder()
                .no_proxy()
                // Each call is its own connection: nothing idle outlives it.
                .pool_max_idle_per_host(0)
                .timeout(RPC_TIMEOUT)
                .build()
                .ok()
        })
        .as_ref()
        .context("could not build the local RPC client")
}

async fn call(client: &Running, port: u16, method: &str) -> Result<Value> {
    let mut request = rpc_client()?
        .post(format!("http://127.0.0.1:{port}/{RPC_SERVICE}/{method}"))
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .body("{}");
    if let Some(Csrf(token)) = &client.csrf {
        request = request.header(CSRF_HEADER, token);
    }
    let response = request
        .send()
        .await
        .map_err(|error| anyhow::anyhow!("{method} on port {port} got no answer: {error}"))?;
    let status = response.status();
    if !status.is_success() {
        bail!("{method} on port {port} answered HTTP {}", status.as_u16());
    }
    response
        .json()
        .await
        .with_context(|| format!("{method} on port {port} answered something other than JSON"))
}

/// Call `method` on whichever of the client's ports answers it.
async fn call_any(client: &Running, method: &str) -> Result<Value> {
    let mut last = anyhow::anyhow!("no local port found");
    for port in &client.ports {
        match call(client, *port, method).await {
            Ok(value) => return Ok(value),
            Err(error) => last = error,
        }
    }
    Err(last)
}

/// The account a hub or an `agy acp` server is signed in as (`GetUserStatus`).
pub(super) async fn account(client: &Running) -> Result<String> {
    call_any(client, "GetUserStatus")
        .await?
        .pointer("/userStatus/email")
        .and_then(Value::as_str)
        .filter(|email| !email.is_empty())
        .map(str::to_owned)
        .context("it is signed in to no account")
}

/// Make a hub read the slot again (`GetAuthStatus`). Only a hub does.
pub(super) async fn reread(client: &Running) -> Result<()> {
    call_any(client, "GetAuthStatus").await.map(|_| ())
}

#[cfg(all(test, unix))]
pub(super) mod tests {
    use super::*;
    use axum::Router;
    use axum::extract::{Path as UrlPath, State};
    use axum::http::{HeaderMap, StatusCode};
    use axum::routing::post;
    use std::os::unix::fs::PermissionsExt;
    use std::sync::{Arc, Mutex};

    pub const CSRF: &str = "csrf-secret";

    /// A language server double. It reports `account` until `GetAuthStatus`
    /// is called, then `after` (when there is one), and records every method.
    #[derive(Clone)]
    pub struct Server {
        pub seen: Arc<Mutex<Vec<String>>>,
        account: Arc<Mutex<String>>,
        after: Option<String>,
        csrf: Option<&'static str>,
        /// HTTP status `GetAuthStatus` answers with.
        auth_status: StatusCode,
    }

    impl Server {
        pub fn hub(account: &str, after: Option<&str>) -> Self {
            Self {
                seen: Arc::default(),
                account: Arc::new(Mutex::new(account.to_owned())),
                after: after.map(str::to_owned),
                csrf: Some(CSRF),
                auth_status: StatusCode::OK,
            }
        }

        pub fn acp(account: &str) -> Self {
            Self {
                csrf: None,
                ..Self::hub(account, None)
            }
        }

        pub fn failing_auth(mut self) -> Self {
            self.auth_status = StatusCode::INTERNAL_SERVER_ERROR;
            self
        }

        pub fn methods(&self) -> Vec<String> {
            self.seen.lock().unwrap().clone()
        }

        /// Serve on a loopback port and return it.
        pub async fn serve(&self) -> u16 {
            async fn rpc(
                State(server): State<Server>,
                UrlPath(method): UrlPath<String>,
                headers: HeaderMap,
            ) -> (StatusCode, String) {
                server.seen.lock().unwrap().push(method.clone());
                if let Some(csrf) = server.csrf
                    && headers.get(CSRF_HEADER).and_then(|v| v.to_str().ok()) != Some(csrf)
                {
                    return (
                        StatusCode::UNAUTHORIZED,
                        r#"{"code":"unauthenticated"}"#.into(),
                    );
                }
                match method.as_str() {
                    "GetUserStatus" => {
                        let email = server.account.lock().unwrap().clone();
                        let body = if email.is_empty() {
                            "{}".to_owned()
                        } else {
                            serde_json::json!({"userStatus": {"email": email}}).to_string()
                        };
                        (StatusCode::OK, body)
                    }
                    "GetAuthStatus" => {
                        if server.auth_status == StatusCode::OK
                            && let Some(after) = &server.after
                        {
                            *server.account.lock().unwrap() = after.clone();
                        }
                        (server.auth_status, "{}".into())
                    }
                    _ => (StatusCode::NOT_FOUND, "{}".into()),
                }
            }
            let router = Router::new()
                .route(&format!("/{RPC_SERVICE}/{{method}}"), post(rpc))
                .with_state(self.clone());
            let base = crate::testing::serve(router).await;
            base.rsplit_once(':').unwrap().1.parse().unwrap()
        }
    }

    fn script(path: &Path, body: &str) {
        std::fs::write(
            path,
            format!(
                "#!/bin/sh\ndir=\"{}\"\n{body}",
                path.parent().unwrap().display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    /// One process the doubles report.
    pub struct Proc {
        pub pid: u32,
        pub comm: &'static str,
        pub args: &'static str,
        pub cwd: Option<&'static str>,
        pub ports: Vec<u16>,
    }

    pub fn proc(pid: u32, args: &'static str) -> Proc {
        Proc {
            pid,
            comm: args.split_whitespace().next().unwrap(),
            args,
            cwd: None,
            ports: Vec::new(),
        }
    }

    /// `ps` and `lsof` doubles in `dir` that report `procs`, an empty daemon
    /// directory beside them, and a log of every `ps`/`lsof` argument list.
    pub fn lookout(dir: &Path, procs: &[Proc]) -> Lookout {
        let comm: String = procs
            .iter()
            .map(|p| format!("{:>5} {}\n", p.pid, p.comm))
            .collect();
        let args: String = procs
            .iter()
            .map(|p| format!("{:>5} {}\n", p.pid, p.args))
            .collect();
        std::fs::write(dir.join("ps-comm"), comm).unwrap();
        std::fs::write(dir.join("ps-args"), args).unwrap();
        for p in procs {
            if let Some(cwd) = p.cwd {
                std::fs::write(
                    dir.join(format!("cwd-{}", p.pid)),
                    format!("p{}\nfcwd\nn{cwd}\n", p.pid),
                )
                .unwrap();
            }
            let ports: String = p
                .ports
                .iter()
                .map(|port| format!("f9\nn127.0.0.1:{port}\n"))
                .collect();
            std::fs::write(dir.join(format!("ports-{}", p.pid)), ports).unwrap();
        }
        script(
            &dir.join("ps"),
            r#"echo "ps $*" >> "$dir/tools"
case "$*" in
  *comm=*) cat "$dir/ps-comm" ;;
  *) cat "$dir/ps-args" ;;
esac
"#,
        );
        script(
            &dir.join("lsof"),
            r#"echo "lsof $*" >> "$dir/tools"
pid=""
while [ $# -gt 0 ]; do
  [ "$1" = "-p" ] && pid="$2"
  case "$1" in cwd) kind=cwd ;; -iTCP) kind=ports ;; esac
  shift
done
[ -f "$dir/$kind-$pid" ] || exit 1
cat "$dir/$kind-$pid"
"#,
        );
        let daemon_dir = dir.join("daemon");
        std::fs::create_dir_all(&daemon_dir).unwrap();
        Lookout {
            ps: dir.join("ps"),
            lsof: dir.join("lsof"),
            daemon_dir,
            history: dir.join("history.jsonl"),
            homes: dir.join("agy-homes"),
        }
    }

    /// Write a hub daemon file the way the app does.
    pub fn hub_file(lookout: &Lookout, pid: u32, port: u16) {
        std::fs::write(
            lookout.daemon_dir.join(format!("hub_{port}.json")),
            serde_json::json!({
                "pid": pid, "httpsPort": port - 1, "httpPort": port,
                "lsVersion": "2.31.2", "csrfToken": CSRF,
            })
            .to_string(),
        )
        .unwrap();
    }

    #[test]
    fn an_agy_command_line_is_an_acp_server_a_print_run_or_interactive() {
        assert_eq!(classify("/Users/x/.local/bin/agy acp"), ClientKind::Acp);
        assert_eq!(classify("agy --model gemini acp"), ClientKind::Acp);
        assert_eq!(classify("agy -p /usage"), ClientKind::Print);
        assert_eq!(classify("agy --print=hello"), ClientKind::Print);
        assert_eq!(classify("agy"), ClientKind::Interactive);
        assert_eq!(classify("agy -c"), ClientKind::Interactive);
        assert_eq!(classify("agy --conversation=abc"), ClientKind::Interactive);
        assert!(is_agy("/Users/x/.local/bin/agy") && is_agy("agy"));
        assert!(!is_agy("/usr/bin/magy") && !is_agy("/opt/agy/bin/node"));
        assert!(!is_agy(
            "/Applications/Antigravity.app/Contents/MacOS/Antigravity"
        ));
    }

    #[tokio::test]
    async fn the_census_finds_live_hubs_and_every_agy_with_its_directory_and_ports() {
        let dir = tempfile::tempdir().unwrap();
        let lookout = lookout(
            dir.path(),
            &[
                proc(
                    10,
                    "/Applications/Antigravity.app/language_server --subclient_type hub",
                ),
                Proc {
                    cwd: Some("/work/editor"),
                    ports: vec![4100, 4101],
                    ..proc(30, "/Users/x/.local/bin/agy acp")
                },
                Proc {
                    cwd: Some("/work/repo"),
                    ..proc(20, "agy -c")
                },
                proc(40, "agy -p /usage"),
                proc(50, "/usr/bin/magy"),
            ],
        );
        hub_file(&lookout, 10, 5001);
        // A daemon file left by a hub that exited is not a running client.
        hub_file(&lookout, 999, 6001);
        std::fs::write(lookout.daemon_dir.join("hub_bad.json"), "not json").unwrap();
        let clients = census(&lookout).await.unwrap();
        let seen: Vec<_> = clients
            .iter()
            .map(|c| (c.kind, c.pid, c.cwd.clone(), c.ports.clone()))
            .collect();
        assert_eq!(
            seen,
            vec![
                (ClientKind::Hub, 10, None, vec![5001]),
                (
                    ClientKind::Interactive,
                    20,
                    Some("/work/repo".into()),
                    vec![]
                ),
                (
                    ClientKind::Acp,
                    30,
                    Some("/work/editor".into()),
                    vec![4100, 4101]
                ),
                (ClientKind::Print, 40, None, vec![]),
            ]
        );
        // The token is carried for the RPC and never shown.
        assert!(clients[0].csrf.is_some());
        assert!(!format!("{clients:?}").contains(CSRF));
        // Only listings: nothing that signals a process.
        let tools = std::fs::read_to_string(dir.path().join("tools")).unwrap();
        assert!(tools.lines().all(|line| {
            line.starts_with("ps -axo pid=,comm=")
                || line.starts_with("ps -ww -o pid=,args= -p ")
                || line.starts_with("ps -E -ww -o pid=,args= -p ")
                || line.starts_with("lsof -a -p ")
                || line.starts_with("lsof -nP -a -p ")
        }));
    }

    #[tokio::test]
    async fn an_agy_on_a_per_account_home_is_a_session_not_a_client_of_the_slot() {
        let dir = tempfile::tempdir().unwrap();
        let lookout = lookout(
            dir.path(),
            &[
                Proc {
                    cwd: Some("/work/repo"),
                    ..proc(20, "agy")
                },
                proc(30, "agy acp"),
                proc(40, "agy"),
            ],
        );
        let homes = lookout.homes.display().to_string();
        // What `ps -E` prints: the arguments, then the environment.
        std::fs::write(
            dir.path().join("ps-args"),
            format!(
                "   20 agy -c PATH=/bin HOME={homes}/second@example.com SSH_TTY=/dev/tty\n\
                 \x20  30 agy acp HOME={homes}/second@example.com/.gemini PWD=/x\n\
                 \x20  40 agy --print=HOME={homes}/fake PATH=/bin HOME=/Users/first\n"
            ),
        )
        .unwrap();
        let clients = census(&lookout).await.unwrap();
        assert_eq!(
            clients.iter().map(|c| (c.kind, c.pid)).collect::<Vec<_>>(),
            vec![(ClientKind::Print, 40)],
            "only the agy on the real home shares the slot"
        );
        let sessions = super::sessions(&lookout).await.unwrap();
        assert_eq!(
            sessions,
            vec![
                Session {
                    pid: 20,
                    kind: ClientKind::Interactive,
                    home: "second@example.com".into(),
                    cwd: Some("/work/repo".into()),
                },
                Session {
                    pid: 30,
                    kind: ClientKind::Acp,
                    home: "second@example.com".into(),
                    cwd: None,
                },
            ]
        );
        assert_eq!(own_home(" HOME=/h/", Path::new("/h")), None);
        assert_eq!(own_home("agy HOME=/other/x", Path::new("/h")), None);
    }

    #[test]
    fn the_resume_hint_is_the_last_conversation_recorded_for_the_directory() {
        let dir = tempfile::tempdir().unwrap();
        let history = dir.path().join("history.jsonl");
        assert_eq!(latest_conversation(&history, Path::new("/work/repo")), None);
        // agy's own lines: some carry no id, some a `type`.
        std::fs::write(
            &history,
            r#"{"display":"hello","timestamp":1794442000000,"workspace":"/work/repo","conversationId":"11111111-aaaa-4bbb-8ccc-000000000001"}
{"display":"other","timestamp":1794442100000,"workspace":"/work/other","conversationId":"22222222-aaaa-4bbb-8ccc-000000000002"}
{"display":"again","timestamp":1794442200000,"workspace":"/work/repo","conversationId":"33333333-aaaa-4bbb-8ccc-000000000003"}
{"display":"/usage","timestamp":1794442300000,"workspace":"/work/repo","type":"slash_command"}
not json
{"display":"x","timestamp":1794442400000,"workspace":"/work/odd","conversationId":"a b; rm -rf"}
"#,
        )
        .unwrap();
        assert_eq!(
            latest_conversation(&history, Path::new("/work/repo")).as_deref(),
            Some("33333333-aaaa-4bbb-8ccc-000000000003")
        );
        assert_eq!(latest_conversation(&history, Path::new("/work/none")), None);
        // An id that is not one is never put into a command line.
        assert_eq!(latest_conversation(&history, Path::new("/work/odd")), None);
        // Another shape entirely: no hint, no error.
        std::fs::write(&history, r#"[{"id":"1","cwd":"/work/repo"}]"#).unwrap();
        assert_eq!(latest_conversation(&history, Path::new("/work/repo")), None);
    }

    #[tokio::test]
    async fn a_census_that_cannot_be_taken_is_an_error_and_a_hung_one_is_bounded() {
        let dir = tempfile::tempdir().unwrap();
        let mut lookout = lookout(dir.path(), &[]);
        assert_eq!(census(&lookout).await.unwrap(), vec![]);
        script(&dir.path().join("ps"), "exit 3\n");
        assert!(census(&lookout).await.is_err());
        script(&dir.path().join("slow"), "exec sleep 60\n");
        lookout.ps = dir.path().join("slow");
        let started = std::time::Instant::now();
        let error = census(&lookout).await.unwrap_err().to_string();
        assert!(error.contains("did not answer"), "{error}");
        assert!(started.elapsed() < CENSUS_TIMEOUT + Duration::from_secs(5));
    }

    #[tokio::test]
    async fn a_hub_needs_its_token_and_an_acp_server_answers_on_any_of_its_ports() {
        let hub = Server::hub("old@example.com", Some("new@example.com"));
        let port = hub.serve().await;
        let mut running = Running {
            kind: ClientKind::Hub,
            pid: 1,
            cwd: None,
            ports: vec![port],
            csrf: Some(Csrf(CSRF.into())),
        };
        assert_eq!(account(&running).await.unwrap(), "old@example.com");
        reread(&running).await.unwrap();
        assert_eq!(account(&running).await.unwrap(), "new@example.com");
        running.csrf = None;
        let refused = account(&running).await.unwrap_err().to_string();
        assert!(refused.contains("HTTP 401"), "{refused}");

        let acp = Server::acp("old@example.com");
        let port = acp.serve().await;
        // The first port is closed; the second answers.
        let running = Running {
            kind: ClientKind::Acp,
            pid: 2,
            cwd: None,
            ports: vec![1, port],
            csrf: None,
        };
        assert_eq!(account(&running).await.unwrap(), "old@example.com");
        let portless = Running {
            ports: vec![],
            ..running
        };
        assert!(account(&portless).await.is_err());
        // Signed out: an answer without an account is not an account.
        let signed_out = Server::acp("");
        let port = signed_out.serve().await;
        let running = Running {
            ports: vec![port],
            ..portless
        };
        let error = account(&running).await.unwrap_err().to_string();
        assert!(error.contains("no account"), "{error}");
    }
}
