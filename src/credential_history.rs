//! Which credential `settings.json` offered Claude, and when.
//!
//! A running Claude process caches the OAuth token it resolved at startup and
//! keeps it until the API refuses it. Only then does it re-read its
//! environment, which by that time mirrors whatever `settings.json` holds. So
//! the account a process is spending is a function of two things tokeman
//! controls: the token installed when the process started, and every token
//! installed at the moments earlier ones expired. This file records exactly
//! that, keyed by fingerprint so no secret is ever written here.
//!
//! Replaying the history gives a derived account for any process at any time,
//! which is what admission feedback and drain accounting need. It replaces two
//! older signals that were both wrong once settings hot-reload is involved:
//! the `TOKEMAN_ACCOUNT` a SessionStart hook sees (the current settings, not
//! the cached credential) and the token visible in `ps eww` (the exec-time
//! environment, which settings override).

use anyhow::Result;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::path::PathBuf;

const HISTORY_FILE: &str = "claude-credential-history.jsonl";
/// Old entries only matter while a process that started under them may still
/// be alive; processes rarely live a month.
const RETENTION_SECS: i64 = 30 * 24 * 3600;
const COMPACT_ABOVE_BYTES: u64 = 512 * 1024;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CredentialEvent {
    /// Epoch seconds when `settings.json` started offering this credential.
    pub at: i64,
    /// `None` when settings stopped offering a managed credential (pause).
    pub account: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fingerprint: Option<String>,
    /// Epoch seconds; `None` for a credential that does not expire in
    /// practice (setup tokens), or for legacy events of unknown kind.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<i64>,
    /// `switch`, `activate`, `refresh`, `upgrade`, `pause`, or `legacy`.
    pub kind: String,
}

/// Non-reversible identifier for a credential value.
pub fn fingerprint(value: &str) -> String {
    let digest = Sha256::digest(value.as_bytes());
    digest[..8]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn path() -> Result<PathBuf> {
    Ok(crate::private_fs::state_dir()?.join(HISTORY_FILE))
}

pub fn load() -> Vec<CredentialEvent> {
    let Ok(path) = path() else {
        return Vec::new();
    };
    let Ok(contents) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    let mut events: Vec<CredentialEvent> = contents
        .lines()
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect();
    events.sort_by_key(|event| event.at);
    events
}

/// Append one event. Callers hold the rotation lock, so appends never
/// interleave with compaction.
pub fn append(event: &CredentialEvent) -> Result<()> {
    let path = path()?;
    crate::private_fs::append_line(&path, &serde_json::to_string(event)?)?;
    if std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0) > COMPACT_ABOVE_BYTES {
        compact(event.at)?;
    }
    Ok(())
}

/// Drop entries that no living process can still be attributed through: keep
/// everything newer than the retention window plus the newest older entry,
/// which is the state in effect at the window's start.
fn compact(now: i64) -> Result<()> {
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
    crate::private_fs::write_atomic(&path()?, contents.as_bytes(), 0o600)
}

/// The account a fingerprint was installed for.
pub fn account_for_fingerprint(events: &[CredentialEvent], fingerprint: &str) -> Option<String> {
    events
        .iter()
        .rev()
        .find(|event| event.fingerprint.as_deref() == Some(fingerprint))
        .and_then(|event| event.account.clone())
}

/// Combine recorded events with default changes reconstructed from the
/// rotation log, for the period before this file existed. Legacy entries
/// carry no expiry, which reproduces the old semantics exactly: a process
/// keeps its launch account for its whole life.
pub fn with_legacy(
    mut events: Vec<CredentialEvent>,
    legacy: impl IntoIterator<Item = (i64, Option<String>)>,
) -> Vec<CredentialEvent> {
    let first_recorded = events.first().map(|event| event.at).unwrap_or(i64::MAX);
    events.extend(
        legacy
            .into_iter()
            .filter(|(at, _)| *at < first_recorded)
            .map(|(at, account)| CredentialEvent {
                at,
                account,
                fingerprint: None,
                expires_at: None,
                kind: "legacy".into(),
            }),
    );
    events.sort_by_key(|event| event.at);
    events
}

/// The account a Claude process started at `started_at` is spending at `at`.
///
/// Outer `None`: nothing is known about the credential at process start.
/// Inner `None`: settings offered no managed credential at that point.
///
/// The process holds the credential installed when it started. When that
/// credential expires, its next request is refused and it adopts whatever
/// settings offer at that moment. If settings still offer the same expired
/// token (tokeman was asleep), it keeps retrying until a newer one appears.
pub fn account_at(events: &[CredentialEvent], started_at: i64, at: i64) -> Option<Option<String>> {
    let mut index = events.iter().rposition(|event| event.at <= started_at)?;
    loop {
        let current = &events[index];
        let Some(expired_at) = current.expires_at.filter(|expiry| *expiry <= at) else {
            return Some(current.account.clone());
        };
        let offered = events
            .iter()
            .rposition(|event| event.at <= expired_at)
            .unwrap_or(index)
            .max(index);
        let next = if events[offered].fingerprint != current.fingerprint {
            offered
        } else {
            // Settings still offered the refused token: the first later
            // installation is what the retrying process picks up.
            match events[offered + 1..]
                .iter()
                .position(|event| event.fingerprint != current.fingerprint)
            {
                Some(offset) => offered + 1 + offset,
                None => return Some(current.account.clone()),
            }
        };
        if events[next].at > at {
            // The replacement arrived after the moment asked about.
            return Some(current.account.clone());
        }
        index = next;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(at: i64, account: &str, fp: &str, expires_at: Option<i64>) -> CredentialEvent {
        CredentialEvent {
            at,
            account: Some(account.into()),
            fingerprint: Some(fp.into()),
            expires_at,
            kind: "test".into(),
        }
    }

    #[test]
    fn a_setup_token_process_keeps_its_launch_account() {
        let events = vec![event(0, "a", "a0", None), event(100, "b", "b0", None)];
        assert_eq!(account_at(&events, 50, 10_000), Some(Some("a".into())));
    }

    #[test]
    fn an_expired_login_token_moves_the_process_to_the_current_default() {
        let events = vec![
            event(0, "a", "a0", Some(1_000)),
            event(500, "b", "b0", Some(3_000)),
        ];
        // Still on a's token before it expires, even though b is the default.
        assert_eq!(account_at(&events, 10, 900), Some(Some("a".into())));
        // Refused at 1000; settings then offer b.
        assert_eq!(account_at(&events, 10, 1_200), Some(Some("b".into())));
    }

    #[test]
    fn a_refresh_of_the_same_account_keeps_attribution() {
        let events = vec![
            event(0, "a", "a0", Some(1_000)),
            event(800, "a", "a1", Some(2_000)),
            event(1_800, "a", "a2", Some(3_000)),
        ];
        assert_eq!(account_at(&events, 10, 2_500), Some(Some("a".into())));
    }

    #[test]
    fn a_late_refresh_is_adopted_when_it_arrives() {
        // a0 expired at 1000 but tokeman only installed b at 1500.
        let events = vec![
            event(0, "a", "a0", Some(1_000)),
            event(1_500, "b", "b0", None),
        ];
        assert_eq!(account_at(&events, 10, 1_200), Some(Some("a".into())));
        assert_eq!(account_at(&events, 10, 1_600), Some(Some("b".into())));
    }

    #[test]
    fn nothing_known_before_the_first_event() {
        let events = vec![event(100, "a", "a0", None)];
        assert_eq!(account_at(&events, 50, 200), None);
    }

    #[test]
    fn a_chain_of_expiries_is_followed() {
        let events = vec![
            event(0, "a", "a0", Some(100)),
            event(50, "b", "b0", Some(200)),
            event(150, "c", "c0", None),
        ];
        assert_eq!(account_at(&events, 10, 150), Some(Some("b".into())));
        assert_eq!(account_at(&events, 10, 250), Some(Some("c".into())));
    }

    #[test]
    fn legacy_events_fill_in_before_the_recorded_history() {
        let recorded = vec![event(1_000, "b", "b0", None)];
        let merged = with_legacy(
            recorded,
            [(10, Some("a".into())), (2_000, Some("z".into()))],
        );
        assert_eq!(merged.len(), 2);
        assert_eq!(account_at(&merged, 20, 5_000), Some(Some("a".into())));
    }

    #[test]
    fn fingerprints_are_stable_and_short() {
        assert_eq!(fingerprint("abc"), fingerprint("abc"));
        assert_ne!(fingerprint("abc"), fingerprint("abd"));
        assert_eq!(fingerprint("abc").len(), 16);
    }
}
