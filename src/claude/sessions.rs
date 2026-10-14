//! Live Claude Code processes and the account each one spends.
//!
//! A live process adopts whatever settings offer at its next request, so the
//! account is the credential timeline's answer for now; the SessionStart
//! binding stands in only while the timeline knows nothing.

use std::collections::BTreeMap;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Local, NaiveDateTime, Utc};
use serde::{Serialize, Serializer};

use crate::history::{self, Event};
use crate::observe::{self, Binding};

/// `ps` walks every process on the box; under a load average of 200 one
/// census took 10 to 24 minutes. Past this it is reported, not awaited.
pub const CENSUS_TIMEOUT: Duration = Duration::from_secs(10);

/// How a session's account was determined.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Attribution {
    /// What settings offer now.
    Timeline,
    /// The SessionStart hook's record; the timeline knew nothing.
    SessionStart,
    /// Settings offer no managed credential (a /login).
    Unmanaged,
    /// Predates the recorded default and has no binding.
    Unknown,
}

impl Attribution {
    pub fn label(self) -> &'static str {
        match self {
            Self::Timeline => "credential timeline",
            Self::SessionStart => "SessionStart binding",
            Self::Unmanaged => "no managed credential (/login)",
            Self::Unknown => "unknown (predates recorded default)",
        }
    }
}

impl Serialize for Attribution {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.label())
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Session {
    pub pid: u32,
    pub tty: Option<String>,
    pub started_at: DateTime<Utc>,
    pub session_id: Option<String>,
    pub account: Option<String>,
    #[serde(rename = "credential_source")]
    pub source: Attribution,
}

/// Run `/bin/ps` with a bounded wait; the child is killed if the wait ends.
pub(crate) async fn ps(args: &[&str], timeout: Duration) -> Result<String> {
    let run = tokio::process::Command::new("/bin/ps")
        .args(args)
        .kill_on_drop(true)
        .output();
    let output = tokio::time::timeout(timeout, run)
        .await
        .with_context(|| format!("ps did not answer within {}s", timeout.as_secs()))?
        .context("could not run ps")?;
    if !output.status.success() {
        bail!("ps exited with {}", output.status);
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Every live Claude Code process and the account it spends, oldest first.
/// An error is a census that could not be taken, never an empty list.
pub async fn live() -> Result<Vec<Session>> {
    let census = ps(&["-axo", "pid=,tty=,lstart=,comm="], CENSUS_TIMEOUT).await?;
    let timeline = history::timeline();
    let now = Utc::now().timestamp();
    let mut sessions: Vec<Session> = census
        .lines()
        .filter_map(parse_ps_session)
        .map(|(pid, tty, started_at)| {
            let binding = observe::binding_for_pid(pid, started_at.timestamp());
            let (account, source) = attribute(&timeline, now, binding.as_ref());
            Session {
                pid,
                tty,
                started_at,
                session_id: binding.map(|binding| binding.session_id),
                account,
                source,
            }
        })
        .collect();
    sessions.sort_by_key(|session| session.started_at);
    Ok(sessions)
}

/// `ps -o lstart=` output: `Sun Jul 26 18:23:11 2026`, local time.
pub(crate) fn parse_lstart(text: &str) -> Option<DateTime<Local>> {
    let naive = NaiveDateTime::parse_from_str(text.trim(), "%a %b %e %T %Y").ok()?;
    observe::local_time(naive)
}

/// One row of `ps -axo pid=,tty=,lstart=,comm=`, when it is a `claude`.
pub fn parse_ps_session(line: &str) -> Option<(u32, Option<String>, DateTime<Utc>)> {
    let fields: Vec<&str> = line.split_whitespace().collect();
    if fields.len() < 8 || fields[7].rsplit('/').next()? != "claude" {
        return None;
    }
    let pid = fields[0].parse().ok()?;
    let tty = (fields[1] != "??").then(|| fields[1].to_owned());
    let started_at = parse_lstart(&fields[2..7].join(" "))?.with_timezone(&Utc);
    Some((pid, tty, started_at))
}

/// The account a live process spends now, and how that was determined.
pub fn attribute(
    timeline: &[Event],
    now: i64,
    binding: Option<&Binding>,
) -> (Option<String>, Attribution) {
    match history::account_at(timeline, now) {
        Some(Some(account)) => (Some(account), Attribution::Timeline),
        Some(None) => (None, Attribution::Unmanaged),
        None => match binding {
            Some(binding) => (Some(binding.account.clone()), Attribution::SessionStart),
            None => (None, Attribution::Unknown),
        },
    }
}

/// Per-account counts, `~` marking accounts known only from a binding;
/// `None` is a census that failed.
pub fn summary(sessions: Option<&[Session]>) -> String {
    let Some(sessions) = sessions else {
        return "unknown (process census failed)".into();
    };
    if sessions.is_empty() {
        return "none detected".into();
    }
    let mut counts = BTreeMap::<String, usize>::new();
    for session in sessions {
        let prefix = if session.source == Attribution::SessionStart {
            "~"
        } else {
            ""
        };
        let name = session.account.as_deref().unwrap_or("unknown");
        *counts.entry(format!("{prefix}{name}")).or_default() += 1;
    }
    counts
        .into_iter()
        .map(|(name, count)| format!("{name}×{count}"))
        .collect::<Vec<_>>()
        .join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::history::{EventKind, Fingerprint};
    use std::path::PathBuf;

    fn offer(at: i64, account: &str) -> Event {
        Event {
            at,
            account: Some(account.into()),
            fingerprint: Some(Fingerprint::of(account)),
            expires_at: Some(100_000),
            kind: EventKind::Switch,
        }
    }

    fn bound(account: &str) -> Binding {
        Binding {
            session_id: "s".into(),
            account: account.into(),
            transcript_path: PathBuf::from("/nonexistent"),
        }
    }

    #[test]
    fn recognizes_only_real_claude_process_rows() {
        let session = parse_ps_session("40102 ttys016 Sun Jul 26 18:23:11 2026 claude").unwrap();
        assert_eq!(session.0, 40102);
        assert_eq!(session.1.as_deref(), Some("ttys016"));
        let expected = parse_lstart("Sun Jul 26 18:23:11 2026").unwrap();
        assert_eq!(session.2, expected.with_timezone(&Utc));
        let daemon = parse_ps_session("40103 ?? Sun Jul 26 18:23:11 2026 /opt/bin/claude").unwrap();
        assert_eq!(daemon.1, None);
        assert!(
            parse_ps_session("5407 ?? Sun Jul 26 18:23:11 2026 /usr/bin/claude_resident").is_none()
        );
        assert!(parse_ps_session("garbage").is_none());
    }

    #[test]
    fn a_live_session_on_a_drained_account_is_not_reported_as_stranded() {
        // Started on `drained`, then the default moved to `fresh`: the process
        // spends `fresh` from its next request.
        let timeline = vec![offer(1_000, "drained"), offer(5_000, "fresh")];
        let (account, source) = attribute(&timeline, 6_000, Some(&bound("drained")));
        assert_eq!(account.as_deref(), Some("fresh"));
        assert_eq!(source, Attribution::Timeline);
        let paused = vec![offer(1_000, "drained"), Event::pause(5_000)];
        let (account, source) = attribute(&paused, 6_000, Some(&bound("drained")));
        assert_eq!(account, None);
        assert_eq!(source, Attribution::Unmanaged);
    }

    #[test]
    fn the_session_start_record_stands_in_only_without_history() {
        let (account, source) = attribute(&[], 6_000, Some(&bound("launch")));
        assert_eq!(account.as_deref(), Some("launch"));
        assert_eq!(source, Attribution::SessionStart);
        let (account, source) = attribute(&[], 6_000, None);
        assert_eq!(account, None);
        assert_eq!(source, Attribution::Unknown);
    }

    #[test]
    fn summaries_count_per_account_and_mark_bindings() {
        let session = |account: Option<&str>, source| Session {
            pid: 1,
            tty: None,
            started_at: Utc::now(),
            session_id: None,
            account: account.map(str::to_owned),
            source,
        };
        assert_eq!(summary(None), "unknown (process census failed)");
        assert_eq!(summary(Some(&[])), "none detected");
        let sessions = [
            session(Some("a"), Attribution::Timeline),
            session(Some("a"), Attribution::Timeline),
            session(Some("b"), Attribution::SessionStart),
            session(None, Attribution::Unknown),
        ];
        assert_eq!(summary(Some(&sessions)), "a×2, unknown×1, ~b×1");
        assert_eq!(
            serde_json::to_value(&sessions[2]).unwrap()["credential_source"],
            "SessionStart binding"
        );
    }
}
