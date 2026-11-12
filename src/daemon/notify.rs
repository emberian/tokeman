//! Telling the user what needs them: the daemon looks at the fleet view's
//! attention list every [`CHECK_INTERVAL`] and sends a desktop notification
//! for each item it has not told yet. What was told is the ledger
//! (`notified.json`), which only this module writes, and only from the
//! daemon. A round runs in a task of its own; the rotation cycle never waits
//! on it, and every notifier process is bounded by [`DELIVERY_TIMEOUT`].
//!
//! macOS: `osascript` `display notification`. Elsewhere `notify-send` when
//! it is on PATH. Without either, and with `notifications = false`, an item
//! is a line in the rotation log and nothing more.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::Result;
use serde::{Deserialize, Serialize};
use tokio::task::JoinHandle;

use crate::account::Fleet;
use crate::fs::{read_if_present, write_atomic};
use crate::paths;
use crate::ui::overview::{AttentionView, Facts, Overview};
use crate::util::one_line;

/// How often the daemon looks for something to tell.
pub const CHECK_INTERVAL: Duration = Duration::from_secs(120);
/// A notifier process that has not finished by then is killed.
pub const DELIVERY_TIMEOUT: Duration = Duration::from_secs(10);
/// An item that has not stood for this long is forgotten, so it is told
/// again if it comes back. Shorter gaps are one episode.
pub const REARM_SECS: i64 = 6 * 3_600;
/// More due items than this in one round are told as one notification.
const AT_ONCE: usize = 3;

pub const TITLE: &str = "tokeman";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
struct Told {
    notified_at: i64,
    /// When the item last stood.
    seen_at: i64,
}

/// Which attention keys have been told.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Ledger {
    told: BTreeMap<String, Told>,
}

impl Ledger {
    /// The ledger at `path`; a missing or unreadable file is an empty one,
    /// and an entry in a shape this build does not read is dropped alone.
    pub fn load(path: &Path) -> Self {
        let entries: BTreeMap<String, serde_json::Value> = read_if_present(path)
            .ok()
            .flatten()
            .and_then(|json| serde_json::from_str(&json).ok())
            .unwrap_or_default();
        Self {
            told: entries
                .into_iter()
                .filter_map(|(key, value)| Some((key, serde_json::from_value(value).ok()?)))
                .collect(),
        }
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        write_atomic(path, &serde_json::to_vec_pretty(&self.told)?, 0o600)
    }

    /// The items of `standing` not told yet, each now recorded as told at
    /// `now`. Entries whose item has not stood for [`REARM_SECS`] are
    /// forgotten.
    pub fn due<'a>(&mut self, standing: &'a [AttentionView], now: i64) -> Vec<&'a AttentionView> {
        let mut due = Vec::new();
        for item in standing {
            match self.told.get_mut(&item.key) {
                Some(told) => told.seen_at = now,
                None => {
                    self.told.insert(
                        item.key.clone(),
                        Told {
                            notified_at: now,
                            seen_at: now,
                        },
                    );
                    due.push(item);
                }
            }
        }
        self.told.retain(|_, told| now - told.seen_at < REARM_SECS);
        due
    }
}

/// What carried a notification.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Channel {
    Osascript,
    NotifySend,
}

/// What became of one notification.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "outcome", rename_all = "kebab-case")]
pub enum Delivery {
    /// The notifier took it. Whether the desktop shows it is the system's
    /// decision (permission, Focus).
    Sent {
        channel: Channel,
    },
    /// This system has no notifier tokeman knows.
    NoNotifier,
    Failed {
        channel: Channel,
        reason: String,
    },
}

/// The words of one item as a notification says them.
pub fn body(item: &AttentionView) -> String {
    match &item.fix {
        Some(fix) => format!("{}. Run: {fix}", item.text),
        None => item.text.clone(),
    }
}

/// `osascript` reads the title and the body as arguments, so neither is
/// ever part of the script.
const DISPLAY_NOTIFICATION: &str =
    "on run argv\ndisplay notification (item 2 of argv) with title (item 1 of argv)\nend run";

#[cfg(target_os = "macos")]
fn notifier(title: &str, body: &str) -> Option<(Channel, PathBuf, Vec<String>)> {
    Some((
        Channel::Osascript,
        PathBuf::from("/usr/bin/osascript"),
        vec![
            "-e".to_owned(),
            DISPLAY_NOTIFICATION.to_owned(),
            title.to_owned(),
            body.to_owned(),
        ],
    ))
}

#[cfg(not(target_os = "macos"))]
fn notifier(title: &str, body: &str) -> Option<(Channel, PathBuf, Vec<String>)> {
    let program = std::env::split_paths(&std::env::var_os("PATH")?)
        .map(|dir| dir.join("notify-send"))
        .find(|candidate| candidate.is_file())?;
    Some((
        Channel::NotifySend,
        program,
        vec![
            "--app-name=tokeman".to_owned(),
            title.to_owned(),
            body.to_owned(),
        ],
    ))
}

/// Run a notifier to its end or to `timeout`, whichever is first.
async fn run(program: &Path, args: &[String], timeout: Duration) -> Result<(), String> {
    let child = tokio::process::Command::new(program)
        .args(args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .output();
    match tokio::time::timeout(timeout, child).await {
        Err(_) => Err(format!(
            "{} did not finish within {}s",
            program.display(),
            timeout.as_secs_f32()
        )),
        Ok(Err(error)) => Err(format!("could not run {}: {error}", program.display())),
        Ok(Ok(output)) if output.status.success() => Ok(()),
        Ok(Ok(output)) => Err(format!(
            "{} exited with {}: {}",
            program.display(),
            output.status,
            one_line(&String::from_utf8_lossy(&output.stderr), 200)
        )),
    }
}

/// Send one desktop notification. Bounded by [`DELIVERY_TIMEOUT`].
pub async fn deliver(title: &str, body: &str) -> Delivery {
    let Some((channel, program, args)) = notifier(title, body) else {
        return Delivery::NoNotifier;
    };
    match run(&program, &args, DELIVERY_TIMEOUT).await {
        Ok(()) => Delivery::Sent { channel },
        Err(reason) => Delivery::Failed { channel, reason },
    }
}

/// The notifications to send for `due`, and the log line of each item.
fn compose(due: &[&AttentionView]) -> (Vec<String>, Vec<String>) {
    let lines = due
        .iter()
        .map(|item| format!("attention: {}", body(item)))
        .collect();
    let bodies = match due {
        [] => Vec::new(),
        due if due.len() <= AT_ONCE => due.iter().map(|item| body(item)).collect(),
        [first, rest @ ..] => vec![format!(
            "{} things need you. {}, and {} more: run `tokeman`",
            due.len(),
            first.text,
            rest.len()
        )],
    };
    (bodies, lines)
}

/// Tell what of `standing` is new and remember it in the ledger at
/// `ledger`. The ledger is written before anything is sent, so an item is
/// never told twice whatever becomes of its notification. Returns the lines
/// for the rotation log: every new item, sent or not.
async fn tell(standing: &[AttentionView], enabled: bool, ledger: &Path, now: i64) -> Vec<String> {
    let mut told = Ledger::load(ledger);
    let before = told.clone();
    let (bodies, mut lines) = compose(&told.due(standing, now));
    if told != before
        && let Err(error) = told.save(ledger)
    {
        // Without the record the same items would be told every round.
        lines.push(format!(
            "ERROR could not record what was notified, so nothing was sent: {error:#}"
        ));
        return lines;
    }
    if !enabled {
        return lines;
    }
    for body in bodies {
        if let Delivery::Failed { reason, .. } = deliver(TITLE, &body).await {
            lines.push(format!("WARNING notification not delivered: {reason}"));
        }
    }
    lines
}

/// One round: what stands in the stored fleet view now, told.
async fn round(fleet: Fleet, ledger: PathBuf) -> Vec<String> {
    let enabled = fleet.launch.notifications;
    let now = chrono::Utc::now().timestamp();
    let view = tokio::task::spawn_blocking(move || {
        Overview::build(&fleet, Facts::on_disk(&fleet, now), None, now)
    })
    .await;
    match view {
        Ok(overview) => tell(&overview.attention, enabled, &ledger, now).await,
        Err(error) => vec![format!("ERROR attention check failed: {error}")],
    }
}

/// The daemon's side: starts a round when one is due and none is running,
/// and hands back the finished round's log lines.
pub struct Notifier {
    ledger: PathBuf,
    next: Instant,
    running: Option<JoinHandle<Vec<String>>>,
}

impl Default for Notifier {
    fn default() -> Self {
        Self::recording_to(paths::notified_ledger())
    }
}

impl Notifier {
    /// The first round waits one interval, so it sees what the daemon's
    /// first reads stored rather than what was left from before it started.
    pub fn recording_to(ledger: PathBuf) -> Self {
        Self {
            ledger,
            next: Instant::now() + CHECK_INTERVAL,
            running: None,
        }
    }

    /// Never waits on a round: a finished one is collected, an unfinished
    /// one is left to run.
    pub async fn tick(&mut self, fleet: &Fleet) -> Vec<String> {
        let mut lines = Vec::new();
        if let Some(round) = self.running.take_if(|round| round.is_finished()) {
            match round.await {
                Ok(told) => lines = told,
                Err(error) => lines.push(format!("ERROR attention task failed: {error}")),
            }
        }
        if self.running.is_none() && Instant::now() >= self.next {
            self.next = Instant::now() + CHECK_INTERVAL;
            self.running = Some(tokio::spawn(round(fleet.clone(), self.ledger.clone())));
        }
        lines
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::attention::{Attention, Within};

    const NOW: i64 = 1_794_000_000;

    fn login_ends(account: &str, within: Within) -> AttentionView {
        AttentionView::of(
            Attention::LoginEnds {
                account: account.into(),
                ends_at: NOW + 86_400,
                within,
            },
            NOW,
        )
    }

    fn keys(due: &[&AttentionView]) -> Vec<String> {
        due.iter().map(|item| item.key.clone()).collect()
    }

    #[test]
    fn a_key_is_told_once_and_again_at_its_next_threshold() {
        let mut ledger = Ledger::default();
        let three = [login_ends("a", Within::ThreeDays)];
        assert_eq!(keys(&ledger.due(&three, NOW)), ["login-ends:a:3d"]);
        // Standing for two days, looked at every hour: still told once.
        for hour in 1..48 {
            let later = NOW + hour * 3_600;
            assert!(
                ledger.due(&three, later).is_empty(),
                "told again at +{hour}h"
            );
        }
        let one = [login_ends("a", Within::OneDay)];
        assert_eq!(
            keys(&ledger.due(&one, NOW + 2 * 86_400)),
            ["login-ends:a:1d"]
        );
        assert!(ledger.due(&one, NOW + 2 * 86_400 + 120).is_empty());
    }

    #[test]
    fn an_item_that_went_away_is_told_again_only_after_the_rearm_time() {
        let mut ledger = Ledger::default();
        let floor = [AttentionView::of(
            Attention::BelowFloor {
                service: crate::quota::Service::Claude,
            },
            NOW,
        )];
        assert_eq!(ledger.due(&floor, NOW).len(), 1);
        // Gone for an hour and back: the same episode.
        assert!(ledger.due(&[], NOW + 1_800).is_empty());
        assert!(ledger.due(&floor, NOW + 3_600).is_empty());
        // Gone for longer than the rearm time: forgotten, then told anew.
        assert!(ledger.due(&[], NOW + 3_600 + REARM_SECS).is_empty());
        assert_eq!(ledger, Ledger::default());
        assert_eq!(ledger.due(&floor, NOW + 3_700 + REARM_SECS).len(), 1);
    }

    #[test]
    fn the_ledger_survives_a_restart_and_drops_only_entries_it_cannot_read() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("notified.json");
        assert_eq!(Ledger::load(&path), Ledger::default(), "no file yet");
        let mut ledger = Ledger::default();
        let standing = [
            login_ends("a", Within::ThreeDays),
            login_ends("b", Within::OneDay),
        ];
        ledger.due(&standing, NOW);
        ledger.save(&path).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        let mut loaded = Ledger::load(&path);
        assert_eq!(loaded, ledger);
        assert!(loaded.due(&standing, NOW + 60).is_empty());

        let mut raw: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        raw["login-ends:a:3d"] = serde_json::json!("yesterday");
        std::fs::write(&path, raw.to_string()).unwrap();
        let mut partial = Ledger::load(&path);
        assert_eq!(
            keys(&partial.due(&standing, NOW + 60)),
            ["login-ends:a:3d"],
            "the entry that still reads is still told"
        );
        std::fs::write(&path, "not json").unwrap();
        assert_eq!(Ledger::load(&path), Ledger::default());
    }

    #[test]
    fn a_few_items_are_told_one_by_one_and_many_as_one() {
        let items: Vec<AttentionView> = ["a", "b", "c", "d"]
            .into_iter()
            .map(|account| login_ends(account, Within::OneDay))
            .collect();
        let few: Vec<&AttentionView> = items.iter().take(2).collect();
        let (bodies, lines) = compose(&few);
        assert_eq!(bodies.len(), 2);
        assert!(
            bodies[0].ends_with("Run: tokeman claude login a"),
            "{bodies:?}"
        );
        assert_eq!(lines.len(), 2);
        assert!(lines[0].starts_with("attention: "), "{lines:?}");

        let many: Vec<&AttentionView> = items.iter().collect();
        let (bodies, lines) = compose(&many);
        assert_eq!(bodies.len(), 1);
        assert!(
            bodies[0].starts_with("4 things need you.") && bodies[0].contains("and 3 more"),
            "{bodies:?}"
        );
        assert_eq!(lines.len(), 4, "the log still names each");
        assert_eq!(compose(&[]), (Vec::new(), Vec::new()));
    }

    #[tokio::test]
    async fn with_notifications_off_an_item_is_logged_once_and_remembered() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("notified.json");
        let standing = [
            login_ends("a", Within::ThreeDays),
            login_ends("b", Within::OneDay),
        ];
        let lines = tell(&standing, false, &path, NOW).await;
        assert_eq!(lines.len(), 2, "{lines:?}");
        assert!(
            lines[0].starts_with("attention: the login of a ends ")
                && lines[0].ends_with("Run: tokeman claude login a"),
            "{lines:?}"
        );
        assert!(tell(&standing, false, &path, NOW + 120).await.is_empty());
        // The second round saw both again and told neither.
        let ledger = Ledger::load(&path);
        assert_eq!(ledger.told.len(), 2);
        assert!(
            ledger
                .told
                .values()
                .all(|told| told.notified_at == NOW && told.seen_at == NOW + 120)
        );
        // A ledger that cannot be written sends nothing and says so.
        let nowhere = dir.path().join("a-file");
        std::fs::write(&nowhere, "").unwrap();
        let lines = tell(&standing, true, &nowhere.join("notified.json"), NOW).await;
        assert!(
            lines
                .last()
                .unwrap()
                .contains("could not record what was notified"),
            "{lines:?}"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_notifier_is_bounded_and_its_failure_is_reported() {
        let sh = Path::new("/bin/sh");
        let script = |script: &str| vec!["-c".to_owned(), script.to_owned()];
        assert_eq!(
            run(sh, &script("exit 0"), Duration::from_secs(5)).await,
            Ok(())
        );
        let failed = run(
            sh,
            &script("echo denied >&2; exit 3"),
            Duration::from_secs(5),
        )
        .await
        .unwrap_err();
        assert!(
            failed.contains("denied") && failed.contains('3'),
            "{failed}"
        );
        let started = Instant::now();
        let hung = run(sh, &script("sleep 30"), Duration::from_millis(200))
            .await
            .unwrap_err();
        assert!(hung.contains("did not finish"), "{hung}");
        assert!(started.elapsed() < Duration::from_secs(5));
        let missing = run(Path::new("/nonexistent/notifier"), &[], DELIVERY_TIMEOUT)
            .await
            .unwrap_err();
        assert!(missing.contains("could not run"), "{missing}");
    }

    #[tokio::test]
    async fn the_daemon_never_waits_on_a_round() {
        let dir = tempfile::tempdir().unwrap();
        let mut notifier = Notifier::recording_to(dir.path().join("notified.json"));
        let fleet = Fleet::default();
        assert!(notifier.tick(&fleet).await.is_empty());
        assert!(notifier.running.is_none(), "the first round waits");
        // A round that never ends does not hold the tick.
        notifier.running = Some(tokio::spawn(std::future::pending()));
        notifier.next = Instant::now();
        let started = Instant::now();
        assert!(notifier.tick(&fleet).await.is_empty());
        assert!(started.elapsed() < Duration::from_secs(1));
        assert!(notifier.running.is_some(), "still the same round");
        notifier.running.take().unwrap().abort();
    }
}
