//! The daemon's sweep over the Codex profiles: probe each, refresh what is
//! due or rejected, and report standing login failures once.

use chrono::Utc;
use std::collections::BTreeSet;
use std::fmt;
use std::path::PathBuf;
use std::time::Duration;

use super::Client;
use super::auth::CodexHome;
use super::probe;
use super::refresh::{self, DEFAULT_MAX_AGE_DAYS, Failure, RefreshError};
use crate::account::{CodexAccount, Fleet};
use crate::quota::{ReadError, Reading};

/// Codex accounts need attention on the order of days; a half hour keeps
/// the request rate far below anything the edge treats as abusive.
pub const SWEEP_INTERVAL: Duration = Duration::from_secs(30 * 60);

/// What one maintenance sweep did, for the daemon's log.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MaintenanceEvent {
    Refreshed {
        account: String,
    },
    /// A standing login failure cleared.
    UsableAgain {
        account: String,
    },
    /// A permanent failure began; reported once until it clears.
    NeedsLogin {
        account: String,
        home: PathBuf,
        failure: Failure,
    },
    RefreshFailed {
        account: String,
        error: RefreshError,
    },
    /// No HTTP client could be built; the sweep did nothing.
    Skipped {
        message: String,
    },
}

impl fmt::Display for MaintenanceEvent {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Refreshed { account } => write!(f, "codex: refreshed {account}"),
            Self::UsableAgain { account } => write!(f, "codex: {account} is usable again"),
            Self::NeedsLogin {
                account,
                home,
                failure,
            } => write!(
                f,
                "codex: {account} needs an interactive `CODEX_HOME={} codex login` ({})",
                home.display(),
                failure.label()
            ),
            Self::RefreshFailed { account, error } => {
                write!(f, "codex: refresh failed for {account}: {error}")
            }
            Self::Skipped { message } => write!(f, "codex: sweep skipped: {message}"),
        }
    }
}

/// What the daemon carries between sweeps: the client, so the cookie jar
/// persists, and the accounts whose login failure has already been reported.
#[derive(Debug, Default)]
pub struct MaintenanceState {
    client: Option<Client>,
    needs_login: BTreeSet<String>,
}

impl MaintenanceState {
    /// The accounts waiting on an interactive `codex login`.
    pub fn needs_login(&self) -> &BTreeSet<String> {
        &self.needs_login
    }
}

/// The daemon's sweep over the fleet's Codex accounts; the events are for
/// its log.
pub async fn maintenance(fleet: &Fleet, state: &mut MaintenanceState) -> Vec<MaintenanceEvent> {
    if fleet.codex.is_empty() {
        return Vec::new();
    }
    let MaintenanceState {
        client,
        needs_login,
    } = state;
    let client = match client {
        Some(client) => client,
        None => match Client::new() {
            Ok(built) => client.insert(built),
            Err(error) => {
                return vec![MaintenanceEvent::Skipped {
                    message: format!("{error:#}"),
                }];
            }
        },
    };
    let (probes, events) = sweep_probed(client, &fleet.codex, needs_login).await;
    crate::daemon::log::log_drift(&probes);
    record(&probes);
    events
}

/// Keep probes in the history, for the surfaces that show stored reads.
fn record(probes: &[Reading]) {
    let Ok(store) = crate::store::Store::open() else {
        return;
    };
    for probe in probes {
        let _ = store.insert(probe);
    }
}

/// Keep every profile refreshed. The backend is asked rather than the clock:
/// a session revoked elsewhere leaves `last_refresh` recent while the token
/// is already rejected, and that rejection forces a refresh. `needs_login`
/// carries the standing failures between sweeps so each is reported when it
/// starts and when it clears, not every half hour.
pub async fn sweep(
    client: &Client,
    accounts: &[CodexAccount],
    needs_login: &mut BTreeSet<String>,
) -> Vec<MaintenanceEvent> {
    sweep_probed(client, accounts, needs_login).await.1
}

/// [`sweep`], with the probes it decided on. They predate any refresh the
/// sweep then made.
async fn sweep_probed(
    client: &Client,
    accounts: &[CodexAccount],
    needs_login: &mut BTreeSet<String>,
) -> (Vec<Reading>, Vec<MaintenanceEvent>) {
    let mut events = Vec::new();
    if accounts.is_empty() {
        return (Vec::new(), events);
    }
    let probes = probe::probe_all(client, accounts).await;
    let rejected: BTreeSet<&str> = probes
        .iter()
        .filter(|probe| probe.error.as_ref().is_some_and(ReadError::is_unauthorized))
        .map(|probe| probe.account.as_str())
        .collect();
    for account in accounts {
        // No refresh material: its eventual 401 is expected, not a fault.
        if account.access_only {
            continue;
        }
        let home = CodexHome::new(account.home());
        let force = rejected.contains(account.name.as_str());
        let outcome =
            refresh::refresh(client, &home, force, DEFAULT_MAX_AGE_DAYS, Utc::now()).await;
        let permanent = outcome
            .as_ref()
            .err()
            .map(|error| error.failure)
            .filter(|failure| failure.is_permanent());
        if permanent.is_none() && needs_login.remove(&account.name) {
            events.push(MaintenanceEvent::UsableAgain {
                account: account.name.clone(),
            });
        }
        match (outcome, permanent) {
            (Ok(Some(_)), _) => events.push(MaintenanceEvent::Refreshed {
                account: account.name.clone(),
            }),
            (Ok(None), _) => {}
            (Err(_), Some(failure)) => {
                if needs_login.insert(account.name.clone()) {
                    events.push(MaintenanceEvent::NeedsLogin {
                        account: account.name.clone(),
                        home: home.path().to_path_buf(),
                        failure,
                    });
                }
            }
            (Err(error), None) => events.push(MaintenanceEvent::RefreshFailed {
                account: account.name.clone(),
                error,
            }),
        }
    }
    drop(rejected);
    (probes, events)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codex::auth::tests::{bundle, id_token};
    use crate::codex::probe::tests::{Backend, challenge, ok, profile, status, usage_body};
    use axum::http::StatusCode;
    use serde_json::json;

    /// Push an account's `last_refresh` back by `days`.
    fn age(account: &CodexAccount, days: i64) {
        let path = CodexHome::new(account.home()).auth_path();
        let mut raw: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        raw["last_refresh"] = json!(Utc::now() - chrono::Duration::days(days));
        std::fs::write(&path, raw.to_string()).unwrap();
    }

    #[tokio::test]
    async fn maintenance_refreshes_rejected_accounts_and_reports_login_needs_once() {
        let dir = tempfile::tempdir().unwrap();
        let rejected = profile(dir.path(), "rejected", "r-old");
        let fine = profile(dir.path(), "fine", "r-fine");
        let orphan = profile(dir.path(), "orphan", "");
        age(&orphan, 8);
        let access_only = CodexAccount {
            access_only: true,
            ..profile(dir.path(), "import", "")
        };
        let accounts = vec![rejected.clone(), fine, orphan.clone(), access_only];
        let new_id = id_token("rejected@x.y", "acct-rejected", "pro");
        let backend = Backend::default()
            .usage_answers(vec![
                status(StatusCode::UNAUTHORIZED, "{}"),
                ok(usage_body(0.0, 0.0)),
            ])
            .token_answers(vec![(
                StatusCode::OK,
                json!({"access_token": "access-new", "refresh_token": "r-new", "id_token": new_id})
                    .to_string(),
            )]);
        let client = backend.client().await;
        let mut needs_login = BTreeSet::new();

        let events = sweep(&client, &accounts, &mut needs_login).await;
        assert_eq!(
            events,
            vec![
                MaintenanceEvent::Refreshed {
                    account: "rejected".into()
                },
                MaintenanceEvent::NeedsLogin {
                    account: "orphan".into(),
                    home: orphan.home(),
                    failure: Failure::NoRefreshToken,
                },
            ]
        );
        assert_eq!(needs_login.iter().collect::<Vec<_>>(), vec!["orphan"]);
        let auth = CodexHome::new(rejected.home()).read().unwrap();
        assert_eq!(auth.access_token(), Some("access-new"));
        assert_eq!(auth.refresh_token(), Some("r-new"));
        assert_eq!(
            auth.account_id(),
            Some("acct-rejected"),
            "the account id survives a refresh"
        );
        let token_requests = backend.requests("/token");
        assert_eq!(token_requests.len(), 1);
        let sent: serde_json::Value = serde_json::from_str(&token_requests[0].body).unwrap();
        assert_eq!(
            sent,
            json!({"client_id": "test-client", "grant_type": "refresh_token", "refresh_token": "r-old"})
        );
        assert_eq!(
            backend.requests("/usage").len(),
            4,
            "access-only accounts are still probed"
        );

        // The standing failure is not repeated; once fixed, it clears once.
        let events = sweep(&client, &accounts, &mut needs_login).await;
        assert!(events.is_empty(), "{events:?}");
        CodexHome::new(orphan.home())
            .write_tokens(&bundle("a", "r"))
            .unwrap();
        let events = sweep(&client, &accounts, &mut needs_login).await;
        assert_eq!(
            events,
            vec![MaintenanceEvent::UsableAgain {
                account: "orphan".into()
            }]
        );
        assert!(needs_login.is_empty());
        assert!(sweep(&client, &[], &mut needs_login).await.is_empty());
    }

    #[tokio::test]
    async fn maintenance_reports_transient_failures_every_sweep_and_a_reused_token_once() {
        let dir = tempfile::tempdir().unwrap();
        let account = profile(dir.path(), "one", "r1");
        let backend = Backend::default()
            .usage_answers(vec![status(StatusCode::UNAUTHORIZED, "{}")])
            .token_answers(vec![
                (StatusCode::BAD_GATEWAY, "upstream".into()),
                (
                    StatusCode::BAD_REQUEST,
                    r#"{"error":"refresh_token_reused"}"#.into(),
                ),
            ]);
        let client = backend.client().await;
        let mut needs_login = BTreeSet::new();
        let accounts = vec![account];
        let events = sweep(&client, &accounts, &mut needs_login).await;
        assert!(matches!(
            &events[..],
            [MaintenanceEvent::RefreshFailed { account, error }]
                if account == "one" && error.failure == Failure::Transient
        ));
        assert!(needs_login.is_empty());
        let events = sweep(&client, &accounts, &mut needs_login).await;
        assert!(matches!(
            &events[..],
            [MaintenanceEvent::NeedsLogin {
                failure: Failure::Reused,
                ..
            }]
        ));
        assert!(sweep(&client, &accounts, &mut needs_login).await.is_empty());
        assert_eq!(
            events[0].to_string(),
            format!(
                "codex: one needs an interactive `CODEX_HOME={} codex login` ({})",
                accounts[0].home().display(),
                Failure::Reused.label()
            )
        );
    }

    #[tokio::test]
    async fn a_challenged_probe_does_not_force_a_refresh() {
        let dir = tempfile::tempdir().unwrap();
        let accounts = vec![profile(dir.path(), "one", "r1")];
        let backend = Backend::default().usage_answers(vec![challenge()]);
        let client = backend.client().await;
        let events = sweep(&client, &accounts, &mut BTreeSet::new()).await;
        assert!(events.is_empty());
        assert!(backend.requests("/token").is_empty());
    }
}
