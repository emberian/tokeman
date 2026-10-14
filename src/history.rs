//! Which credential settings.json offered Claude, and when.
//!
//! A live Claude process spends whatever settings offer *now*, from its next
//! request on. The history answers which account a rejection seen in a
//! transcript was charged to at the moment it happened. Entries carry
//! fingerprints only; no secret is written here.
//!
//! Before this file existed the rotation log was the only record; its
//! default-change lines are parsed once per log revision and merged in.

use anyhow::Result;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::SystemTime;

use crate::fs::{append_line, read_if_present, write_atomic};
use crate::paths;

/// Old entries only matter while a transcript rejection from their period may
/// still be scanned; a month covers any session still being read.
const RETENTION_SECS: i64 = 30 * 24 * 3600;
const COMPACT_ABOVE_BYTES: u64 = 512 * 1024;

/// Why settings changed. The serde names are the on-disk strings.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EventKind {
    /// The policy moved the default.
    Switch,
    /// `tokeman rotate use`.
    Activate,
    /// `tokeman rotate install` re-offered the managed account.
    Install,
    /// The same account's token was renewed.
    Refresh,
    /// The account's setup key was replaced by its login.
    Upgrade,
    /// The account's login was replaced by its setup key.
    Fallback,
    /// The settings entry was repaired without changing the account.
    Repair,
    /// Settings stopped offering a managed credential.
    Pause,
    /// Reconstructed from the rotation log; account and moment only.
    Legacy,
    /// Not an offer: the token with this fingerprint stopped working at `at`.
    Revoke,
    #[serde(other)]
    Unknown,
}

impl EventKind {
    /// The on-disk spelling, for log lines.
    pub fn label(self) -> &'static str {
        match self {
            Self::Switch => "switch",
            Self::Activate => "activate",
            Self::Install => "install",
            Self::Refresh => "refresh",
            Self::Upgrade => "upgrade",
            Self::Fallback => "fallback",
            Self::Repair => "repair",
            Self::Pause => "pause",
            Self::Legacy => "legacy",
            Self::Revoke => "revoke",
            Self::Unknown => "unknown",
        }
    }
}

/// A non-reversible identifier for a credential value (16 hex characters).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Fingerprint(String);

impl Fingerprint {
    pub fn of(value: &str) -> Self {
        let digest = Sha256::digest(value.as_bytes());
        Self(
            digest[..8]
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect(),
        )
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Event {
    /// Epoch seconds when settings started offering this credential.
    pub at: i64,
    /// `None` when settings stopped offering a managed credential.
    pub account: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fingerprint: Option<Fingerprint>,
    /// Epoch seconds; `None` for a setup key or a legacy event.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<i64>,
    pub kind: EventKind,
}

impl Event {
    pub fn offer(
        at: i64,
        account: &str,
        credential: &str,
        expires_at: Option<i64>,
        kind: EventKind,
    ) -> Self {
        Self {
            at,
            account: Some(account.to_owned()),
            fingerprint: Some(Fingerprint::of(credential)),
            expires_at,
            kind,
        }
    }

    pub fn pause(at: i64) -> Self {
        Self {
            at,
            account: None,
            fingerprint: None,
            expires_at: None,
            kind: EventKind::Pause,
        }
    }

    pub fn revoke(at: i64, account: &str, credential: &str) -> Self {
        Self {
            at,
            account: Some(account.to_owned()),
            fingerprint: Some(Fingerprint::of(credential)),
            expires_at: None,
            kind: EventKind::Revoke,
        }
    }
}

/// The recorded events, oldest first.
pub fn load() -> Vec<Event> {
    let Ok(Some(contents)) = read_if_present(&paths::credential_history()) else {
        return Vec::new();
    };
    let mut events: Vec<Event> = contents
        .lines()
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect();
    events.sort_by_key(|event| event.at);
    events
}

/// Append one event. Callers hold the rotation lock, so appends never
/// interleave with compaction.
pub fn append(event: &Event) -> Result<()> {
    let path = paths::credential_history();
    append_line(&path, &serde_json::to_string(event)?)?;
    if std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0) > COMPACT_ABOVE_BYTES {
        compact(&path, event.at)?;
    }
    Ok(())
}

/// Keep everything inside the retention window plus the newest older entry,
/// which is the state in effect at the window's start.
fn compact(path: &Path, now: i64) -> Result<()> {
    let cutoff = now - RETENTION_SECS;
    let events = load();
    let first_kept = events
        .iter()
        .rposition(|event| event.at <= cutoff)
        .unwrap_or(0);
    let mut contents = String::new();
    for event in &events[first_kept..] {
        contents.push_str(&serde_json::to_string(event)?);
        contents.push('\n');
    }
    write_atomic(path, contents.as_bytes(), 0o600)
}

/// Recorded events with the rotation log's default changes filled in before
/// the first recorded one.
pub fn timeline() -> Vec<Event> {
    with_legacy(load(), legacy_events())
}

pub fn with_legacy(
    mut events: Vec<Event>,
    legacy: impl IntoIterator<Item = (i64, Option<String>)>,
) -> Vec<Event> {
    let first_recorded = events.first().map(|event| event.at).unwrap_or(i64::MAX);
    events.extend(
        legacy
            .into_iter()
            .filter(|(at, _)| *at < first_recorded)
            .map(|(at, account)| Event {
                at,
                account,
                fingerprint: None,
                expires_at: None,
                kind: EventKind::Legacy,
            }),
    );
    events.sort_by_key(|event| event.at);
    events
}

/// The account settings offered at `at`. Outer `None`: nothing is known that
/// early. Inner `None`: no managed credential was offered (paused).
/// Revocations are not offers: a revoked token settings still offer is still
/// the account the process is charged to.
pub fn account_at(events: &[Event], at: i64) -> Option<Option<String>> {
    events
        .iter()
        .rfind(|event| event.kind != EventKind::Revoke && event.at <= at)
        .map(|event| event.account.clone())
}

/// The account a fingerprint was last offered for.
pub fn account_for(events: &[Event], fingerprint: &Fingerprint) -> Option<String> {
    events
        .iter()
        .rev()
        .find(|event| event.fingerprint.as_ref() == Some(fingerprint))
        .and_then(|event| event.account.clone())
}

// Legacy: default changes parsed from the rotation log and its predecessor.

type Stamp = Option<(u64, Option<SystemTime>)>;

struct LegacyCache {
    stamps: Vec<Stamp>,
    events: Vec<(i64, Option<String>)>,
}

static LEGACY: Mutex<Option<LegacyCache>> = Mutex::new(None);

fn stamp(path: &Path) -> Stamp {
    let metadata = std::fs::metadata(path).ok()?;
    Some((metadata.len(), metadata.modified().ok()))
}

fn legacy_events() -> Vec<(i64, Option<String>)> {
    let log = paths::rotation_log();
    let logs: [PathBuf; 2] = [log.with_extension("log.old"), log];
    let stamps: Vec<Stamp> = logs.iter().map(|path| stamp(path)).collect();
    let mut cache = LEGACY
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if let Some(cached) = cache.as_ref()
        && cached.stamps == stamps
    {
        return cached.events.clone();
    }
    let contents: Vec<String> = logs
        .iter()
        .filter_map(|path| std::fs::read_to_string(path).ok())
        .collect();
    let events = parse_log_events(contents.iter().map(String::as_str));
    *cache = Some(LegacyCache {
        stamps,
        events: events.clone(),
    });
    events
}

fn parse_log_events<'a>(logs: impl IntoIterator<Item = &'a str>) -> Vec<(i64, Option<String>)> {
    let mut events: Vec<_> = logs
        .into_iter()
        .flat_map(|contents| contents.lines().filter_map(parse_log_line))
        .collect();
    events.sort_by_key(|(at, _)| *at);
    events
}

/// A rotation log line that changed the default: `explicitly activated X`,
/// `switch A -> B; ...`, or `rotation paused; ...`.
fn parse_log_line(line: &str) -> Option<(i64, Option<String>)> {
    let (stamp, message) = line.split_once(' ')?;
    let at = chrono::DateTime::parse_from_rfc3339(stamp)
        .ok()?
        .timestamp();
    let account = if let Some(name) = message.strip_prefix("explicitly activated ") {
        Some(name.trim().to_owned())
    } else if let Some(rest) = message.strip_prefix("switch ") {
        let (_, destination) = rest.split_once(" -> ")?;
        Some(destination.split(';').next()?.trim().to_owned())
    } else if message.starts_with("rotation paused;") {
        None
    } else {
        return None;
    };
    Some((at, account))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(at: i64, account: &str, fp: &str, expires_at: Option<i64>) -> Event {
        Event {
            at,
            account: Some(account.into()),
            fingerprint: Some(Fingerprint(fp.into())),
            expires_at,
            kind: EventKind::Switch,
        }
    }

    #[test]
    fn a_live_process_follows_a_switch_before_its_token_expires() {
        let events = vec![
            event(0, "a", "a0", Some(100_000)),
            event(500, "b", "b0", Some(100_000)),
        ];
        assert_eq!(account_at(&events, 400), Some(Some("a".into())));
        assert_eq!(account_at(&events, 600), Some(Some("b".into())));
    }

    #[test]
    fn a_revocation_is_not_an_offer() {
        let events = vec![
            event(0, "a", "a0", Some(10_000)),
            Event::revoke(400, "a", "secret"),
        ];
        assert_eq!(account_at(&events, 500), Some(Some("a".into())));
    }

    #[test]
    fn a_pause_offers_no_managed_credential_and_nothing_is_known_before_the_first_event() {
        let events = vec![event(100, "a", "a0", None), Event::pause(200)];
        assert_eq!(account_at(&events, 300), Some(None));
        assert_eq!(account_at(&events, 50), None);
    }

    #[test]
    fn legacy_events_fill_in_before_the_recorded_history() {
        let merged = with_legacy(
            vec![event(1_000, "b", "b0", None)],
            [(10, Some("a".into())), (2_000, Some("z".into()))],
        );
        assert_eq!(merged.len(), 2);
        assert_eq!(account_at(&merged, 500), Some(Some("a".into())));
        assert_eq!(account_at(&merged, 5_000), Some(Some("b".into())));
    }

    #[test]
    fn fingerprints_are_stable_and_short() {
        assert_eq!(Fingerprint::of("abc"), Fingerprint::of("abc"));
        assert_ne!(Fingerprint::of("abc"), Fingerprint::of("abd"));
        assert_eq!(Fingerprint::of("abc").as_str().len(), 16);
    }

    #[test]
    fn events_keep_their_stored_spelling() {
        let json = r#"{"at":5,"account":"a","fingerprint":"ff","expires_at":9,"kind":"upgrade"}"#;
        let parsed: Event = serde_json::from_str(json).unwrap();
        assert_eq!(parsed.kind, EventKind::Upgrade);
        assert_eq!(serde_json::to_string(&parsed).unwrap(), json);
        let odd: Event =
            serde_json::from_str(r#"{"at":1,"account":null,"kind":"something-new"}"#).unwrap();
        assert_eq!(odd.kind, EventKind::Unknown);
    }

    #[test]
    fn log_lines_that_change_the_default_are_events_and_others_are_not() {
        let at = |line| parse_log_line(line);
        assert_eq!(
            at("2026-07-26T22:23:09Z explicitly activated boot@example.com")
                .unwrap()
                .1
                .as_deref(),
            Some("boot@example.com")
        );
        assert_eq!(
            at("2026-07-26T22:23:19Z switch boot@example.com -> ember@example.com; threshold reached")
                .unwrap()
                .1
                .as_deref(),
            Some("ember@example.com")
        );
        assert_eq!(
            at("2026-07-26T22:24:00Z rotation paused; removed settings OAuth token")
                .unwrap()
                .1,
            None
        );
        assert!(at("2026-07-26T22:25:00Z kept ember@example.com").is_none());
        let older = "2026-07-26T22:23:09Z explicitly activated a@example.com\n";
        let newer = "2026-07-26T22:23:19Z switch a@example.com -> b@example.com; floor\n";
        let accounts: Vec<_> = parse_log_events([newer, older])
            .into_iter()
            .map(|(_, a)| a.unwrap())
            .collect();
        assert_eq!(accounts, ["a@example.com", "b@example.com"]);
    }
}
