//! Starting a tool on the account tokeman picked for it. `tokeman claude
//! launch` picks the default and then starts claude in this terminal; every
//! other `tokeman <service> launch` is a [`Launch`]: a
//! program, its arguments and the environment that seats the account.

use anyhow::{Context, Result, bail};
use std::ffi::{OsStr, OsString};
use std::process::Command;

use crate::account::Fleet;
use crate::daemon::cycle::{self, Options};
use crate::notice::notice;
use crate::quota::Service;

const TAG: &str = " \x1b[1mtokeman:\x1b[0m";

/// Variables inherited from the caller's shell that would pin Claude Code to a
/// single credential instead of reading the managed default from settings.json.
const CLAUDE_UNSET: [&str; 5] = [
    "CLAUDE_CODE_OAUTH_TOKEN",
    "CLAUDE_CODE_OAUTH_SCOPES",
    "TOKEMAN_ACCOUNT",
    "ANTHROPIC_API_KEY",
    "ANTHROPIC_AUTH_TOKEN",
];

/// Run one cycle, then exec claude with the configured arguments. With `auto`
/// when no background service is running, supervise the child and keep cycling
/// on the sip cadence; when the background service already runs, delegate
/// ongoing rotation to it and exec claude directly so it owns the terminal.
pub async fn run(fleet: Fleet, auto: bool, claude_args: Vec<String>) -> Result<()> {
    if fleet.accounts.is_empty() {
        bail!(
            "No accounts configured. Use `tokeman claude login <name>` or `tokeman claude add <name> <key>` to add one."
        );
    }
    notice(&format!(
        "{TAG} probing {} accounts and selecting the default...",
        fleet.accounts.len()
    ));
    let outcome = cycle::run(&fleet, Options::default()).await?;
    notice(&format!("{TAG} {}", outcome.message));
    let (binary, args) = fleet.launch.command(claude_args);
    let shown: Vec<String> = args.iter().map(|arg| format!("{arg:?}")).collect();
    notice(&format!("{TAG} launching {binary} {}", shown.join(" ")));

    let daemon_running =
        crate::ui::overview::Monitor::now() == crate::ui::overview::Monitor::Running;
    if !auto || daemon_running {
        if auto && daemon_running {
            notice(&format!(
                "{TAG} background service is running; delegating auto-rotation to it"
            ));
        }
        notice("");
        let mut launch = Launch::new(Service::Claude, "", None, &binary, args);
        for var in CLAUDE_UNSET {
            launch = launch.unset(var);
        }
        return Err(launch.exec());
    }

    notice(&format!(
        "{TAG} default monitor enabled ({}s normal / {}s sip-and-drain; events logged to claude-rotate.log)",
        fleet.policy.normal_probe_interval_secs, fleet.policy.sip_probe_interval_secs
    ));
    notice("");

    let mut command = tokio::process::Command::new(&binary);
    command.args(&args);
    for var in CLAUDE_UNSET {
        command.env_remove(var);
    }
    let mut child = command
        .spawn()
        .with_context(|| format!("failed to launch {binary}"))?;
    let child_pid = child.id();

    let monitor = tokio::spawn(async move {
        let mut wake = tokio::time::interval(std::time::Duration::from_secs(
            fleet.policy.sip_probe_interval_secs.max(1),
        ));
        wake.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        wake.tick().await; // the launch itself just ran a cycle
        let mut refresh_backoff = crate::daemon::refresh::Backoff::default();
        loop {
            wake.tick().await;
            if !crate::daemon::lock::is_paused()
                && let Err(error) = crate::daemon::refresh::refresh_due(&mut refresh_backoff).await
            {
                crate::daemon::log::log(&format!("ERROR launch monitor refresh failed: {error:#}"));
            }
            let current_fleet = Fleet::load().unwrap_or_else(|_| fleet.clone());
            let scheduled = Options {
                scheduled: true,
                ..Options::default()
            };
            match cycle::run(&current_fleet, scheduled).await {
                Ok(outcome) if outcome.is_notable() => {
                    crate::daemon::log::log(&format!("launch monitor: {}", outcome.message));
                }
                Ok(_) => {}
                Err(error) => crate::daemon::log::log(&format!(
                    "ERROR launch monitor probe failed: {error:#}"
                )),
            }
        }
    });

    let status = wait_supervising_signals(&mut child, child_pid).await?;
    monitor.abort();
    if !status.success() {
        std::process::exit(status.code().unwrap_or(1));
    }
    Ok(())
}

#[cfg(unix)]
async fn wait_supervising_signals(
    child: &mut tokio::process::Child,
    child_pid: Option<u32>,
) -> Result<std::process::ExitStatus> {
    use tokio::signal::unix::{SignalKind, signal};
    let mut sigint = signal(SignalKind::interrupt()).ok();
    let mut sigquit = signal(SignalKind::quit()).ok();
    let mut sigterm = signal(SignalKind::terminate()).ok();
    let mut sighup = signal(SignalKind::hangup()).ok();
    let forward = |sig_flag: &str| {
        if let Some(pid) = child_pid {
            let _ = Command::new("/bin/kill")
                .args([sig_flag, &pid.to_string()])
                .status();
        }
    };
    loop {
        tokio::select! {
            status = child.wait() => return Ok(status?),
            // Terminal Ctrl-C / Ctrl-\ already goes to the foreground process
            // group (`claude`); absorb it here so the supervisor outlives a
            // cancelled turn.
            Some(()) = async {
                match &mut sigint {
                    Some(s) => s.recv().await,
                    None => None,
                }
            } => {}
            Some(()) = async {
                match &mut sigquit {
                    Some(s) => s.recv().await,
                    None => None,
                }
            } => {}
            Some(()) = async {
                match &mut sigterm {
                    Some(s) => s.recv().await,
                    None => None,
                }
            } => forward("-TERM"),
            Some(()) = async {
                match &mut sighup {
                    Some(s) => s.recv().await,
                    None => None,
                }
            } => forward("-HUP"),
        }
    }
}

#[cfg(not(unix))]
async fn wait_supervising_signals(
    child: &mut tokio::process::Child,
    _child_pid: Option<u32>,
) -> Result<std::process::ExitStatus> {
    loop {
        tokio::select! {
            status = child.wait() => return Ok(status?),
            _ = tokio::signal::ctrl_c() => {}
        }
    }
}

/// One variable of a launch's environment.
#[derive(Clone, PartialEq, Eq)]
pub struct EnvVar {
    pub name: &'static str,
    pub value: OsString,
    /// A credential: named when the launch is described, never shown.
    pub secret: bool,
}

impl std::fmt::Debug for EnvVar {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let value: &dyn std::fmt::Debug = if self.secret { &"<set>" } else { &self.value };
        f.debug_struct("EnvVar")
            .field("name", &self.name)
            .field("value", value)
            .field("secret", &self.secret)
            .finish()
    }
}

/// A tool ready to start on one account. The account is seated through the
/// child's environment alone: nothing is written to the tool's own files.
#[derive(Debug, Clone, PartialEq)]
pub struct Launch {
    /// The service whose account pays.
    pub service: Service,
    pub account: String,
    /// The fraction left on the account's tightest counted limit.
    pub room: Option<f64>,
    program: OsString,
    args: Vec<String>,
    env: Vec<EnvVar>,
    /// Inherited variables the child must not see.
    unset: Vec<&'static str>,
}

impl Launch {
    pub fn new(
        service: Service,
        account: &str,
        room: Option<f64>,
        program: impl Into<OsString>,
        args: Vec<String>,
    ) -> Self {
        Self {
            service,
            account: account.to_owned(),
            room,
            program: program.into(),
            args,
            env: Vec::new(),
            unset: Vec::new(),
        }
    }

    pub fn set(mut self, name: &'static str, value: impl Into<OsString>) -> Self {
        self.env.push(EnvVar {
            name,
            value: value.into(),
            secret: false,
        });
        self
    }

    pub fn set_secret(mut self, name: &'static str, value: impl Into<OsString>) -> Self {
        self.env.push(EnvVar {
            name,
            value: value.into(),
            secret: true,
        });
        self
    }

    pub fn unset(mut self, name: &'static str) -> Self {
        self.unset.push(name);
        self
    }

    pub fn program(&self) -> &OsStr {
        &self.program
    }

    pub fn args(&self) -> &[String] {
        &self.args
    }

    pub fn env(&self) -> &[EnvVar] {
        &self.env
    }

    pub fn removed(&self) -> &[&'static str] {
        &self.unset
    }

    fn command(&self) -> Command {
        let mut command = Command::new(&self.program);
        command.args(&self.args);
        for name in &self.unset {
            command.env_remove(name);
        }
        for var in &self.env {
            command.env(var.name, &var.value);
        }
        command
    }

    /// Replace this process with the tool, which takes over the terminal and
    /// its signals. Returns only when it could not be started.
    #[cfg(unix)]
    pub fn exec(self) -> anyhow::Error {
        use std::os::unix::process::CommandExt;
        let error = self.command().exec();
        anyhow::Error::new(error)
            .context(format!("could not run {}", self.program.to_string_lossy()))
    }

    /// Run the tool in this terminal and exit with its status.
    #[cfg(not(unix))]
    pub fn exec(self) -> anyhow::Error {
        match self.command().status() {
            Ok(status) => std::process::exit(status.code().unwrap_or(1)),
            Err(error) => anyhow::Error::new(error)
                .context(format!("could not run {}", self.program.to_string_lossy())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn a_launch_seats_the_account_in_the_childs_environment_only() {
        let launch = Launch::new(
            Service::Codex,
            "work",
            Some(0.5),
            "/bin/sh",
            vec![
                "-c".into(),
                "printf '%s|%s|%s' \"$SEATED\" \"$SECRET\" \"${GONE-unset}\"".into(),
            ],
        )
        .set("SEATED", "home")
        .set_secret("SECRET", "k")
        .unset("GONE");
        let output = launch.command().output().unwrap();
        assert_eq!(String::from_utf8_lossy(&output.stdout), "home|k|unset");
        // An inherited variable of that name is taken out of the child's
        // environment, whatever this process holds.
        let command = launch.command();
        let removed: Vec<_> = command
            .get_envs()
            .filter(|(_, value)| value.is_none())
            .map(|(name, _)| name.to_owned())
            .collect();
        assert_eq!(removed, [OsStr::new("GONE")]);
        assert_eq!(launch.removed(), ["GONE"]);
        let shown = format!("{launch:?}");
        assert!(shown.contains(r#"value: "home""#), "{shown}");
        assert!(!shown.contains(r#""k""#), "{shown}");
        assert!(
            launch
                .env()
                .iter()
                .any(|var| var.secret && var.name == "SECRET")
        );
    }
}
