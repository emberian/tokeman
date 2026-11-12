//! The seats tokeman pushed: for each CODEX_HOME whose running Codex was
//! handed an account's token over the control socket, which account, which
//! token (as a fingerprint) and when that token expires.
//!
//! A Codex on a pushed token cannot renew it: the app-server holds it in
//! memory, never refreshes it ahead of expiry, and on a 401 asks its
//! connected clients for a new one for ten seconds. This file is how the
//! daemon knows which seats to renew ([`super::keep`]) and how every surface
//! knows one is about to run out. `codex-seats.json`; this module is its
//! only writer, under a lock.

use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

use super::mover::By;
use crate::fs::{FileLock, read_if_present, write_atomic};
use crate::history::Fingerprint;
use crate::paths;

/// Why a seat could not be kept alive. The serde names are the on-disk
/// strings.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Cause {
    /// The profile's refresh token was refused for good, or it has none.
    Refused,
    /// The refresh failed in a way a later try may not.
    RefreshFailed,
    /// The control socket is there and did not answer.
    Unanswered,
    /// The app-server refused the renewed token.
    PushFailed,
    /// The profile is no longer registered.
    ProfileGone,
    #[serde(other)]
    Unknown,
}

impl Cause {
    pub fn name(self) -> &'static str {
        match self {
            Self::Refused => "refused",
            Self::RefreshFailed => "refresh-failed",
            Self::Unanswered => "unanswered",
            Self::PushFailed => "push-failed",
            Self::ProfileGone => "profile-gone",
            Self::Unknown => "unknown",
        }
    }
}

/// The last failed attempt to renew a seat. `message` never holds a token.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Trouble {
    pub cause: Cause,
    /// Epoch seconds.
    pub at: i64,
    pub message: String,
}

/// One pushed seat.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Pushed {
    /// The CODEX_HOME whose Codex holds the token, `~`-relative.
    pub home: String,
    /// The registered profile the token came from.
    pub account: String,
    /// The backend's id for that account.
    pub account_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,
    /// The access token that was pushed, as a fingerprint.
    pub token: Fingerprint,
    /// Epoch seconds when that token stops working; `None` when the token
    /// does not say.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<i64>,
    /// Epoch seconds of the push.
    pub pushed_at: i64,
    pub by: By,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trouble: Option<Trouble>,
}

/// The seat file. `Seats::default()` is the live one.
#[derive(Debug, Clone)]
pub struct Seats {
    path: PathBuf,
}

impl Default for Seats {
    fn default() -> Self {
        Self::at(paths::codex_seats())
    }
}

impl Seats {
    pub fn at(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    /// Every pushed seat. An entry this build does not read is left out;
    /// no file is no seats.
    pub fn load(&self) -> Vec<Pushed> {
        read_at(&self.path)
    }

    /// The seat on `home` (`~`-relative), if tokeman pushed one.
    pub fn on(&self, home: &str) -> Option<Pushed> {
        self.load().into_iter().find(|seat| seat.home == home)
    }

    fn change(&self, change: impl FnOnce(&mut Vec<Pushed>)) -> Result<()> {
        crate::fs::ensure_private_parent(&self.path)?;
        let _lock = FileLock::acquire_announced(
            &self.path.with_extension("lock"),
            "the Codex seat file lock",
        )?;
        let mut seats = read_at(&self.path);
        let before = seats.clone();
        change(&mut seats);
        if seats == before {
            return Ok(());
        }
        let mut bytes = serde_json::to_vec_pretty(&seats)?;
        bytes.push(b'\n');
        write_atomic(&self.path, &bytes, 0o600)
    }

    /// Record a push; it replaces what was known about that home.
    pub fn put(&self, seat: Pushed) -> Result<()> {
        self.change(|seats| {
            seats.retain(|known| known.home != seat.home);
            seats.push(seat);
        })
    }

    /// Forget the seat on `home`: its Codex ended or left the account.
    pub fn forget(&self, home: &str) -> Result<()> {
        self.change(|seats| seats.retain(|known| known.home != home))
    }

    /// Set or clear the trouble of the seat on `home`, if it still holds
    /// the token `of` was read with.
    pub fn trouble(&self, of: &Pushed, trouble: Option<Trouble>) -> Result<()> {
        self.change(|seats| {
            if let Some(seat) = seats
                .iter_mut()
                .find(|seat| seat.home == of.home && seat.token == of.token)
            {
                seat.trouble = trouble;
            }
        })
    }
}

fn read_at(path: &Path) -> Vec<Pushed> {
    let Some(text) = read_if_present(path).ok().flatten() else {
        return Vec::new();
    };
    serde_json::from_str::<Vec<serde_json::Value>>(&text)
        .unwrap_or_default()
        .into_iter()
        .filter_map(|entry| serde_json::from_value(entry).ok())
        .collect()
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub(crate) fn pushed(home: &str, account: &str, expires_at: Option<i64>) -> Pushed {
        Pushed {
            home: home.into(),
            account: account.into(),
            account_id: format!("acct-{account}"),
            email: Some(format!("{account}@x.y")),
            token: Fingerprint::of(&format!("token-{account}")),
            expires_at,
            pushed_at: 100,
            by: By::Daemon,
            trouble: None,
        }
    }

    #[test]
    fn seats_are_kept_per_home_and_trouble_sticks_to_the_token_it_was_about() {
        let dir = tempfile::tempdir().unwrap();
        let seats = Seats::at(dir.path().join("codex-seats.json"));
        assert!(seats.load().is_empty());
        seats.put(pushed("~/.codex", "a", Some(500))).unwrap();
        seats.put(pushed("~/other", "b", None)).unwrap();
        // A second push to a home replaces the first.
        seats.put(pushed("~/.codex", "c", Some(900))).unwrap();
        let known = seats.load();
        assert_eq!(known.len(), 2);
        assert_eq!(seats.on("~/.codex").unwrap().account, "c");

        let trouble = Trouble {
            cause: Cause::Refused,
            at: 7,
            message: "revoked".into(),
        };
        // Trouble about a token that has since been replaced is dropped.
        seats
            .trouble(&pushed("~/.codex", "a", Some(500)), Some(trouble.clone()))
            .unwrap();
        assert_eq!(seats.on("~/.codex").unwrap().trouble, None);
        let current = seats.on("~/.codex").unwrap();
        seats.trouble(&current, Some(trouble.clone())).unwrap();
        assert_eq!(seats.on("~/.codex").unwrap().trouble, Some(trouble));
        seats.trouble(&current, None).unwrap();
        assert_eq!(seats.on("~/.codex").unwrap().trouble, None);

        seats.forget("~/other").unwrap();
        assert_eq!(seats.load().len(), 1);
        let text = std::fs::read_to_string(dir.path().join("codex-seats.json")).unwrap();
        assert!(text.contains(r#""by": "daemon""#), "{text}");
        assert!(!text.contains("token-c"), "only a fingerprint is stored");
        // An entry in another shape is dropped alone.
        std::fs::write(
            dir.path().join("codex-seats.json"),
            format!(
                "[{{\"home\": 3}}, {}]",
                serde_json::to_string(&current).unwrap()
            ),
        )
        .unwrap();
        assert_eq!(seats.load(), vec![current]);
    }
}
