//! The rotation daemon: the loop, one cycle, its on-disk state, the lock,
//! the launchd service, the service-quota poll, the notifications, and the
//! commands that change what it manages.

pub mod cadence;
pub mod codex;
pub mod cycle;
pub mod lock;
pub mod log;
pub mod notify;
pub mod refresh;
pub mod service;
pub mod services;

use std::path::PathBuf;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};

use self::lock::{RotationLock, is_paused};
use self::log::log;
use crate::account::Fleet;
use crate::claude::{keychain, settings as claude_settings};
use crate::history::{Event, EventKind};
use crate::paths;

/// Epoch seconds, as a float, the way the cadence file stores them.
pub fn now_epoch() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs_f64()
}

/// Size and modification time of the running executable. A long-lived process
/// compares it against the value at startup and exits when the binary on disk
/// has been replaced, so launchd (or the user) restarts into the new code.
pub fn executable_identity() -> Option<(u64, SystemTime)> {
    let path = std::env::current_exe().ok()?;
    let metadata = std::fs::metadata(path).ok()?;
    Some((metadata.len(), metadata.modified().ok()?))
}

const TICK: Duration = Duration::from_secs(20);

/// The loop `tokeman rotate daemon` runs until the binary is replaced.
pub async fn run() -> Result<()> {
    let Some(_alive) = lock::DaemonLock::take()? else {
        anyhow::bail!("another tokeman daemon is already running");
    };
    let started_as = executable_identity();
    let mut wake = tokio::time::interval(TICK);
    wake.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut next_codex_sweep = Instant::now();
    let mut refresh_backoff = refresh::Backoff::default();
    let mut codex_state = crate::codex::maintenance::MaintenanceState::default();
    let mut services_poller = services::Poller::default();
    let mut notifier = notify::Notifier::default();
    let mut codex_mover = codex::Mover::default();
    loop {
        wake.tick().await;
        if started_as.is_some() && executable_identity().is_some_and(|now| Some(now) != started_as)
        {
            log("daemon executable was replaced; exiting so launchd loads the new binary");
            return Ok(());
        }
        if !is_paused()
            && let Err(error) = refresh::refresh_due(&mut refresh_backoff).await
        {
            log(&format!("ERROR login refresh sweep failed: {error:#}"));
        }
        let fleet = match Fleet::load() {
            Ok(fleet) => fleet,
            Err(error) => {
                log(&format!("ERROR config reload failed: {error:#}"));
                crate::notice::notice(&format!("tokeman config reload failed: {error:#}"));
                continue;
            }
        };
        // Runs in its own task; the cycle below never waits on it.
        for line in services_poller.tick(&fleet).await {
            log(&line);
        }
        // Likewise: a round of notifications is a task of its own.
        for line in notifier.tick(&fleet).await {
            log(&line);
        }
        // Likewise: a look at the running Codexes, in its own bounded task.
        for line in codex_mover.tick(&fleet).await {
            log(&line);
        }
        let scheduled = cycle::Options {
            scheduled: true,
            ..cycle::Options::default()
        };
        match cycle::run(&fleet, scheduled).await {
            Ok(outcome) if outcome.is_notable() => crate::notice::notice(&outcome.message),
            Ok(_) => {}
            Err(error) => {
                log(&format!("ERROR rotation cycle failed: {error:#}"));
                crate::notice::notice(&format!("tokeman rotation cycle failed: {error:#}"));
            }
        }
        if Instant::now() >= next_codex_sweep {
            next_codex_sweep = Instant::now() + crate::codex::maintenance::SWEEP_INTERVAL;
            for event in crate::codex::maintenance::maintenance(&fleet, &mut codex_state).await {
                log(&event.to_string());
            }
        }
    }
}

/// Install the launchd agent, the SessionStart hook, and re-offer the
/// managed default's credential.
pub fn install() -> Result<PathBuf> {
    // Stop the old daemon before waiting for the lock: a probe can own it for
    // a network timeout, and a 20-second loop can starve an installer.
    service::stop();
    let _lock = RotationLock::acquire()?;
    let binary = std::env::current_exe()?.canonicalize()?;
    let fleet = Fleet::load()?;
    if keychain::suppress(false)? {
        log("suppressed short-lived /login Keychain auth during service install");
    }
    let mut settings = claude_settings::load()?;
    let now_ms = chrono::Utc::now().timestamp_millis();
    let managed =
        claude_settings::configured_token(&settings).and_then(|value| fleet.account_holding(value));
    let event =
        match managed {
            Some(account) if account.credential(now_ms).is_some() => Some(
                claude_settings::install(&mut settings, account, now_ms, EventKind::Install)?,
            ),
            _ => None,
        };
    claude_settings::install_observer_hook(&mut settings, &binary);
    claude_settings::commit(&settings, event)?;
    let plist = service::install(&binary)?;
    log(&format!(
        "installed rotation service using {}",
        binary.display()
    ));
    Ok(plist)
}

/// Remove the agent and the hook, and put the /login record back.
pub fn uninstall() -> Result<()> {
    let _lock = RotationLock::acquire()?;
    service::uninstall()?;
    let mut settings = claude_settings::load()?;
    if claude_settings::remove_observer_hook(&mut settings) {
        claude_settings::save(&settings)?;
    }
    if keychain::restore()? {
        log("restored saved /login credential during service uninstall");
    }
    log("uninstalled rotation service");
    Ok(())
}

/// Stop managing the credential and give /login back its precedence.
pub fn pause() -> Result<()> {
    let _lock = RotationLock::acquire()?;
    let marker = paths::pause_marker();
    crate::fs::ensure_private_parent(&marker)?;
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&marker)?;
    crate::fs::set_mode(&marker, 0o600)?;
    let mut settings = claude_settings::load()?;
    if claude_settings::clear(&mut settings) {
        claude_settings::commit(
            &settings,
            Some(Event::pause(chrono::Utc::now().timestamp())),
        )?;
    }
    let restored = keychain::restore()?;
    log(if restored {
        "rotation paused; removed settings OAuth token and restored saved /login credential"
    } else {
        "rotation paused; removed settings OAuth token and restored /login precedence"
    });
    Ok(())
}

/// Resume managing, then run one cycle; a healthy explicit choice is kept.
pub async fn resume(fleet: &Fleet) -> Result<cycle::Outcome> {
    {
        let _lock = RotationLock::acquire()?;
        let marker = paths::pause_marker();
        if marker.exists() {
            std::fs::remove_file(&marker)
                .with_context(|| format!("failed to remove {}", marker.display()))?;
        }
        log("rotation resumed");
    }
    cycle::run(fleet, cycle::Options::default()).await
}

/// `tokeman claude use <name>`: make this account the default now.
pub fn activate(fleet: &Fleet, name: &str) -> Result<()> {
    let _lock = RotationLock::acquire()?;
    let account = fleet
        .account(name)
        .with_context(|| format!("account '{name}' is not configured"))?;
    if keychain::suppress(false)? {
        log("suppressed short-lived /login Keychain auth before explicit activation");
    }
    let mut settings = claude_settings::load()?;
    let now_ms = chrono::Utc::now().timestamp_millis();
    let event = claude_settings::install(&mut settings, account, now_ms, EventKind::Activate)?;
    claude_settings::commit(&settings, Some(event))?;
    log(&format!("explicitly activated {name}"));
    Ok(())
}

/// After the user ran /login deliberately (to enroll or capture an account):
/// managed auth stays authoritative, so the fresh Keychain record is set aside.
pub fn reconcile_after_login() -> Result<()> {
    let _lock = RotationLock::acquire()?;
    if is_paused() {
        return Ok(());
    }
    if keychain::suppress(true)? {
        log("suppressed short-lived /login Keychain auth after account enrollment");
    }
    Ok(())
}

/// The account settings.json offers now, if tokeman manages it.
pub fn default_account(fleet: &Fleet) -> Result<Option<String>> {
    let settings = claude_settings::load()?;
    Ok(claude_settings::configured_token(&settings)
        .and_then(|value| fleet.account_holding(value))
        .map(|account| account.name.clone()))
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    fn sources(dir: &Path, into: &mut Vec<PathBuf>) {
        for entry in std::fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                sources(&path, into);
            } else if path.extension().is_some_and(|extension| extension == "rs") {
                into.push(path);
            }
        }
    }

    /// A source file up to its test module.
    fn shipped(text: &str) -> &str {
        [
            "\nmod tests {",
            "\npub(crate) mod tests {",
            "\npub(super) mod tests {",
        ]
        .iter()
        .filter_map(|marker| text.find(marker))
        .min()
        .map_or(text, |end| &text[..end])
    }

    /// What reaches a reset claim, and the only files that may name it:
    /// the two claims, the journal entry a claim opens, the requests that
    /// send one, and the two commands that call them.
    const CLAIM_PATH: [(&str, &[&str]); 6] = [
        (
            "resets::claim(",
            &["commands/claude.rs", "commands/codex.rs"],
        ),
        (
            "reset_claim(",
            &["commands/claude.rs", "commands/codex.rs", "main.rs"],
        ),
        (".begin(", &["anthropic/resets.rs", "codex/resets.rs"]),
        ("consume_url(", &["codex/mod.rs", "codex/resets.rs"]),
        ("reset_rate_limits", &["anthropic/resets.rs"]),
        ("oauth_post(", &["anthropic/mod.rs", "anthropic/resets.rs"]),
    ];

    /// The rotation cycle and `launch` talk to a live endpoint, so that they
    /// claim nothing is pinned on the source: nothing outside the reset
    /// commands names a step of a claim. The Codex mover's side is pinned by
    /// behaviour in `daemon::codex` and `commands::codex`.
    #[test]
    fn nothing_but_the_reset_commands_can_reach_a_claim() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut files = Vec::new();
        sources(&root, &mut files);
        let mut named = [0usize; CLAIM_PATH.len()];
        for file in &files {
            let text = std::fs::read_to_string(file).unwrap();
            let name = file.strip_prefix(&root).unwrap().to_string_lossy();
            for (index, (step, allowed)) in CLAIM_PATH.iter().enumerate() {
                if shipped(&text).contains(step) {
                    assert!(
                        allowed.contains(&name.as_ref()),
                        "{name} names {step}: only {allowed:?} may"
                    );
                    named[index] += 1;
                }
            }
        }
        for ((step, allowed), count) in CLAIM_PATH.iter().zip(named) {
            assert_eq!(count, allowed.len(), "{step} is no longer where it was");
        }

        // `main` sends a claim only after `--yes` or a yes at the prompt.
        let main = std::fs::read_to_string(root.join("main.rs")).unwrap();
        let calls: Vec<usize> = main
            .match_indices("reset_claim(&plan)")
            .map(|(at, _)| at)
            .collect();
        assert_eq!(calls.len(), 2, "one claim per service");
        for at in calls {
            let before = &main[at.saturating_sub(400)..at];
            let asked = before
                .rfind("if !yes && !prompt::confirm(")
                .expect("a claim follows the confirmation");
            assert!(
                before[asked..].contains("return Ok(());"),
                "a refusal returns before the claim"
            );
        }
    }
}
