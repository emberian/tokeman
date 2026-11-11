//! The daemon's service poll: every `services_interval_secs` it reads the
//! `[[services]]` accounts in a task of its own and records the reads. The
//! rotation cycle never waits on it, and a failed read is logged, not acted on.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Instant;

use tokio::task::JoinHandle;

use crate::account::Fleet;
use crate::paths;
use crate::providers::{self, Service, Usage};
use crate::store::Store;

pub struct Poller {
    store: PathBuf,
    next: Option<Instant>,
    running: Option<JoinHandle<Vec<Usage>>>,
    /// The last error of each account that is failing, so only changes are logged.
    failing: BTreeMap<(Service, String), String>,
}

impl Default for Poller {
    fn default() -> Self {
        Self::recording_to(paths::snapshots_db())
    }
}

impl Poller {
    pub fn recording_to(store: PathBuf) -> Self {
        Self {
            store,
            next: None,
            running: None,
            failing: BTreeMap::new(),
        }
    }

    /// Record a finished poll and start the next when it is due. Returns the
    /// lines worth logging.
    pub async fn tick(&mut self, fleet: &Fleet) -> Vec<String> {
        let mut lines = Vec::new();
        if let Some(poll) = self.running.take_if(|poll| poll.is_finished()) {
            match poll.await {
                Ok(usages) => {
                    self.record(&usages);
                    lines = self.transitions(&usages);
                }
                Err(error) => lines.push(format!("ERROR service poll task failed: {error}")),
            }
        }
        let due = self.next.is_none_or(|next| Instant::now() >= next);
        if self.running.is_none() && due && !fleet.services.is_empty() {
            self.next = Some(Instant::now() + fleet.launch.services_interval());
            let accounts = fleet.services.clone();
            self.running = Some(tokio::spawn(async move {
                providers::read_all(
                    providers::client(),
                    &accounts,
                    chrono::Utc::now().timestamp(),
                )
                .await
            }));
        }
        lines
    }

    fn record(&self, usages: &[Usage]) {
        let Ok(store) = Store::open_at(&self.store) else {
            return;
        };
        for usage in usages {
            let _ = store.insert_usage(usage);
        }
    }

    /// One line per account that started failing, changed its error, or recovered.
    fn transitions(&mut self, usages: &[Usage]) -> Vec<String> {
        let mut lines = Vec::new();
        for usage in usages {
            let key = (usage.service, usage.account.clone());
            match &usage.error {
                Some(error) => {
                    if self.failing.get(&key) != Some(error) {
                        lines.push(format!(
                            "service read failed for {} ({}): {error}",
                            usage.account,
                            usage.service.name()
                        ));
                        self.failing.insert(key, error.clone());
                    }
                }
                None => {
                    if self.failing.remove(&key).is_some() {
                        lines.push(format!(
                            "service read recovered for {} ({})",
                            usage.account,
                            usage.service.name()
                        ));
                    }
                }
            }
        }
        lines
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_poll_runs_in_the_background_and_is_recorded_once_finished() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("s.db");
        let mut poller = Poller::recording_to(db.clone());
        let mut fleet = Fleet::default();
        assert!(poller.tick(&fleet).await.is_empty());
        assert!(poller.running.is_none(), "no services, no poll");
        fleet
            .services
            .push(providers::ServiceAccount::with_key_file(
                Service::Zai,
                "glm",
                "/nonexistent/zai-key",
            ));
        assert!(poller.tick(&fleet).await.is_empty());
        let started = poller.next.unwrap();
        while !poller.running.as_ref().unwrap().is_finished() {
            tokio::task::yield_now().await;
        }
        let lines = poller.tick(&fleet).await;
        assert_eq!(lines.len(), 1, "{lines:?}");
        assert!(lines[0].contains("glm"), "{lines:?}");
        assert!(
            poller.running.is_none(),
            "the next poll waits for its interval"
        );
        assert_eq!(poller.next, Some(started));
        let stored = Store::open_at(&db).unwrap().latest_usage().unwrap();
        assert_eq!(stored.len(), 1);
        assert!(
            stored[0]
                .error
                .as_deref()
                .unwrap()
                .contains("/nonexistent/zai-key")
        );
    }

    #[test]
    fn only_changes_in_an_accounts_error_are_logged() {
        let mut poller = Poller::default();
        let failed = |error: &str| vec![Usage::failed(Service::Kimi, "kimi", 0, error)];
        let ok = Usage {
            error: None,
            ..Usage::failed(Service::Kimi, "kimi", 0, "")
        };
        assert_eq!(poller.transitions(&failed("HTTP 401")).len(), 1);
        assert!(poller.transitions(&failed("HTTP 401")).is_empty());
        let changed = poller.transitions(&failed("HTTP 500"));
        assert!(changed[0].contains("HTTP 500"), "{changed:?}");
        let recovered = poller.transitions(std::slice::from_ref(&ok));
        assert!(recovered[0].contains("recovered"), "{recovered:?}");
        assert!(poller.transitions(&[ok]).is_empty());
    }
}
