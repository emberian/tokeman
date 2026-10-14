//! Which credential `settings.json` offered Claude, and when.
//!
//! tokeman hands Claude its credential through the `env` block of
//! `~/.claude/settings.json`. A running Claude process follows that file: on
//! every change it copies the settings `env` into its own environment, and
//! every API client it builds first clears its in-memory OAuth token and
//! re-derives it from `CLAUDE_CODE_OAUTH_TOKEN` as it stands at that moment
//! (Claude Code 2.1.282 through 2.1.287, read from the binary). So a live
//! process spends whatever settings offer *now*, from its next request on;
//! it does not keep the token it started with.
//!
//! Measured, not only read: over the 39 default switches from 2026-09-29 to
//! 2026-10-01 the outgoing account stopped burning by the first probe after
//! the switch (0% further 5h use in 37 of 39), and the incoming one took the
//! whole load. A session that tokeman attributed to its launch account was
//! refused, minutes after a switch, with the 5h reset time of the new default.
//!
//! The one known exception is a session whose workspace was never trusted:
//! Claude re-applies settings `env` on change only once trust is accepted, so
//! such a session keeps its launch-time token. tokeman cannot see trust per
//! process, so it does not try.
//!
//! The history is still worth keeping: it answers which account a rejection
//! seen in a transcript was charged to, at the moment it happened. Entries are
//! keyed by fingerprint, so no secret is ever written here.

use anyhow::Result;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::path::PathBuf;

const HISTORY_FILE: &str = "claude-credential-history.jsonl";
/// Old entries only matter while a transcript rejection from their period may
/// still be scanned; a month covers any session that is still being read.
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
    /// `switch`, `activate`, `refresh`, `upgrade`, `pause`, `legacy`, or
    /// `revoke`. A `revoke` event is not an offer: it records that the token
    /// with this fingerprint stopped working at `at` (a refresh revokes the
    /// access token it replaces).
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

pub const REVOKE: &str = "revoke";

/// Combine recorded events with default changes reconstructed from the
/// rotation log, for the period before this file existed. Legacy entries
/// carry no fingerprint or expiry; only the account and the moment matter.
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

/// The account a live Claude process was spending at `at`: the one settings
/// offered at that moment. When the process started does not matter, because
/// it re-reads the settings credential for every API client it builds.
///
/// Outer `None`: nothing is known about the credential at `at`.
/// Inner `None`: settings offered no managed credential then (pause).
///
/// Revocations are not offers. A revoked or expired token that settings still
/// offer is still the account the process is charged to: it is refused, and
/// waits for settings to offer a replacement.
pub fn account_at(events: &[CredentialEvent], at: i64) -> Option<Option<String>> {
    events
        .iter()
        .rfind(|event| event.kind != REVOKE && event.at <= at)
        .map(|event| event.account.clone())
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
    fn a_live_process_follows_a_switch_before_its_token_expires() {
        // The process started on `a`, whose token is good for hours more.
        // Settings switch to `b`; the process spends `b` from then on.
        let events = vec![
            event(0, "a", "a0", Some(100_000)),
            event(500, "b", "b0", Some(100_000)),
        ];
        assert_eq!(account_at(&events, 400), Some(Some("a".into())));
        assert_eq!(account_at(&events, 600), Some(Some("b".into())));
    }

    #[test]
    fn a_setup_token_process_follows_the_default_too() {
        let events = vec![event(0, "a", "a0", None), event(100, "b", "b0", None)];
        assert_eq!(account_at(&events, 10_000), Some(Some("b".into())));
    }

    #[test]
    fn a_revocation_is_not_an_offer() {
        let revoke = CredentialEvent {
            at: 400,
            account: Some("a".into()),
            fingerprint: Some("a0".into()),
            expires_at: None,
            kind: REVOKE.into(),
        };
        let events = vec![event(0, "a", "a0", Some(10_000)), revoke];
        assert_eq!(account_at(&events, 500), Some(Some("a".into())));
    }

    #[test]
    fn a_pause_offers_no_managed_credential() {
        let mut pause = event(200, "a", "a0", None);
        pause.account = None;
        pause.fingerprint = None;
        let events = vec![event(0, "a", "a0", None), pause];
        assert_eq!(account_at(&events, 300), Some(None));
    }

    #[test]
    fn nothing_known_before_the_first_event() {
        let events = vec![event(100, "a", "a0", None)];
        assert_eq!(account_at(&events, 50), None);
    }

    #[test]
    fn legacy_events_fill_in_before_the_recorded_history() {
        let recorded = vec![event(1_000, "b", "b0", None)];
        let merged = with_legacy(
            recorded,
            [(10, Some("a".into())), (2_000, Some("z".into()))],
        );
        assert_eq!(merged.len(), 2);
        assert_eq!(account_at(&merged, 500), Some(Some("a".into())));
        assert_eq!(account_at(&merged, 5_000), Some(Some("b".into())));
    }

    #[test]
    fn fingerprints_are_stable_and_short() {
        assert_eq!(fingerprint("abc"), fingerprint("abc"));
        assert_ne!(fingerprint("abc"), fingerprint("abd"));
        assert_eq!(fingerprint("abc").len(), 16);
    }
}
