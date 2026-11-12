//! The daemon's look at every running Codex: every [`mover::INTERVAL`], in a
//! task of its own bounded by [`mover::SWEEP_TIMEOUT`], it asks each control
//! socket for its limits and moves a Codex below the `[rotation]` Codex
//! floors onto a profile with room. The rotation cycle never waits on it.
//! Off when `codex_auto_move` is false, while rotation is paused, and with
//! fewer than two unparked profiles.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Instant;

use tokio::task::JoinHandle;

use super::lock::is_paused;
use crate::account::Fleet;
use crate::codex::Client;
use crate::codex::mover::{self, By, Look, MoveFloors, Outcome};
use crate::paths;
use crate::store::Store;

pub struct Mover {
    store: PathBuf,
    moves: PathBuf,
    next: Option<Instant>,
    running: Option<JoinHandle<Result<Vec<Look>, String>>>,
    /// The outcome last seen per home, so only changes are logged.
    last: BTreeMap<PathBuf, &'static str>,
}

impl Default for Mover {
    fn default() -> Self {
        Self::recording_to(paths::snapshots_db(), paths::codex_moves())
    }
}

/// Whether the daemon would look at all, and why not.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Standing {
    On,
    /// `codex_auto_move = false`.
    Off,
    /// Rotation is paused; nothing is managed.
    Paused,
    /// Fewer than two unparked profiles: nowhere to move to.
    FewProfiles {
        unparked: usize,
    },
}

impl Standing {
    pub fn of(fleet: &Fleet) -> Self {
        Self::with_pause(fleet, is_paused())
    }

    fn with_pause(fleet: &Fleet, paused: bool) -> Self {
        let unparked = mover::landings(fleet).len();
        if !fleet.policy.codex_auto_move {
            Self::Off
        } else if paused {
            Self::Paused
        } else if unparked < 2 {
            Self::FewProfiles { unparked }
        } else {
            Self::On
        }
    }
}

impl Mover {
    pub fn recording_to(store: PathBuf, moves: PathBuf) -> Self {
        Self {
            store,
            moves,
            next: None,
            running: None,
            last: BTreeMap::new(),
        }
    }

    /// Take in a finished look and start the next when it is due. Returns
    /// the lines worth logging.
    pub async fn tick(&mut self, fleet: &Fleet) -> Vec<String> {
        self.tick_with(fleet, Standing::of(fleet), None).await
    }

    async fn tick_with(
        &mut self,
        fleet: &Fleet,
        standing: Standing,
        client: Option<Client>,
    ) -> Vec<String> {
        let mut lines = Vec::new();
        if let Some(look) = self.running.take_if(|look| look.is_finished()) {
            match look.await {
                Ok(Ok(looks)) => lines = self.take_in(&looks),
                Ok(Err(message)) => lines.push(format!("ERROR codex mover: {message}")),
                Err(error) => lines.push(format!("ERROR codex mover task failed: {error}")),
            }
        }
        let due = self.next.is_none_or(|next| Instant::now() >= next);
        if self.running.is_none() && due && standing == Standing::On {
            self.next = Some(Instant::now() + mover::INTERVAL);
            let fleet = fleet.clone();
            self.running = Some(tokio::spawn(async move {
                let client = match client {
                    Some(client) => client,
                    None => Client::new().map_err(|error| format!("{error:#}"))?,
                };
                let homes = mover::homes(&fleet);
                let floors = MoveFloors::of(&fleet);
                tokio::time::timeout(
                    mover::SWEEP_TIMEOUT,
                    mover::sweep(&client, &fleet, &homes, floors, false),
                )
                .await
                .map_err(|_| {
                    format!(
                        "a look did not finish in {}s",
                        mover::SWEEP_TIMEOUT.as_secs()
                    )
                })
            }));
        }
        lines
    }

    /// Keep what the looks read, record the moves, and say what changed.
    fn take_in(&mut self, looks: &[Look]) -> Vec<String> {
        let mut lines = Vec::new();
        let store = Store::open_at(&self.store).ok();
        for look in looks {
            if let Some(store) = &store {
                for reading in &look.probed {
                    let _ = store.insert(reading);
                }
            }
            if let Some(moved) = look.moved(By::Daemon, chrono::Utc::now().timestamp()) {
                lines.push(moved.to_string());
                if let Err(error) = mover::record_at(&self.moves, &moved) {
                    lines.push(format!("ERROR codex move was not recorded: {error:#}"));
                }
            }
            let kind = look.kind();
            let before = self.last.insert(look.home.clone(), kind);
            let said = match &look.outcome {
                Outcome::Moved { .. } | Outcome::Failed { .. } => true,
                // Trouble is said when it starts, and its end once.
                Outcome::Draining | Outcome::Unreachable { .. } => before != Some(kind),
                _ => matches!(before, Some("draining" | "unreachable" | "failed")),
            };
            if said {
                lines.push(look.to_string());
            }
        }
        lines
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codex::appserver::tests::{FakeAppServer, limits_answer};
    use crate::codex::auth::CodexHome;
    use crate::codex::probe::tests::{Backend, ok, profile, usage_body};
    use crate::quota::Service;

    #[test]
    fn the_daemon_looks_only_with_the_setting_on_unpaused_and_somewhere_to_move() {
        let dir = tempfile::tempdir().unwrap();
        let mut fleet = Fleet {
            codex: vec![profile(dir.path(), "a", "r")],
            ..Fleet::default()
        };
        assert_eq!(
            Standing::with_pause(&fleet, false),
            Standing::FewProfiles { unparked: 1 }
        );
        fleet.codex.push(profile(dir.path(), "b", "r"));
        assert_eq!(Standing::with_pause(&fleet, false), Standing::On);
        assert_eq!(Standing::with_pause(&fleet, true), Standing::Paused);
        fleet.codex[1].parked = true;
        assert_eq!(
            Standing::with_pause(&fleet, false),
            Standing::FewProfiles { unparked: 1 }
        );
        fleet.codex[1].parked = false;
        fleet.policy.codex_auto_move = false;
        assert_eq!(Standing::with_pause(&fleet, false), Standing::Off);
    }

    async fn finished(mover: &Mover) {
        while !mover.running.as_ref().unwrap().is_finished() {
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    }

    #[tokio::test]
    async fn a_low_codex_is_moved_in_the_background_recorded_and_logged_once() {
        let dir = tempfile::tempdir().unwrap();
        let fake = FakeAppServer::start(vec![
            ("acct-a", "a@x.y", limits_answer(90.0, 10.0)),
            ("acct-b", "b@x.y", limits_answer(10.0, 10.0)),
        ]);
        // Profile `a` is the home the Codex runs on.
        let home = CodexHome::new(fake.home.path());
        std::fs::copy(
            CodexHome::new(profile(dir.path(), "a", "r-a").home()).auth_path(),
            home.auth_path(),
        )
        .unwrap();
        let fleet = Fleet {
            codex: vec![
                crate::testing::codex_account("a", &fake.home.path().display().to_string()),
                profile(dir.path(), "b", "r-b"),
            ],
            ..Fleet::default()
        };
        let backend = Backend::default().usage_answers(vec![ok(usage_body(20.0, 20.0))]);
        let client = backend.client().await;
        let db = dir.path().join("s.db");
        let moves = dir.path().join("moves.jsonl");
        let mut mover = Mover::recording_to(db.clone(), moves.clone());

        // Off, paused or with one profile, no task starts.
        for standing in [
            Standing::Off,
            Standing::Paused,
            Standing::FewProfiles { unparked: 1 },
        ] {
            assert!(
                mover
                    .tick_with(&fleet, standing, Some(client.clone()))
                    .await
                    .is_empty()
            );
            assert!(mover.running.is_none(), "{standing:?}");
        }

        // The tick that starts the look returns at once.
        assert!(
            mover
                .tick_with(&fleet, Standing::On, Some(client.clone()))
                .await
                .is_empty()
        );
        assert!(fake.logins().is_empty() || mover.running.is_some());
        finished(&mover).await;
        let lines = mover
            .tick_with(&fleet, Standing::On, Some(client.clone()))
            .await;
        assert_eq!(fake.account_id(), "acct-b");
        assert!(
            lines[0].starts_with("codex: moved ")
                && lines[0].ends_with("from a to b (10% left in its 5h window) [daemon]"),
            "{lines:?}"
        );
        assert!(
            lines[1].contains("moved to b (a@x.y (plus) -> b@x.y (plus))"),
            "{lines:?}"
        );
        assert_eq!(lines.len(), 2);
        assert!(
            mover.running.is_none(),
            "the next look waits for its interval"
        );
        let recorded = mover::moves_at(&moves);
        assert_eq!(recorded.len(), 1);
        assert_eq!(
            (
                recorded[0].from.as_deref(),
                recorded[0].to.as_str(),
                recorded[0].by
            ),
            (Some("a"), "b", By::Daemon)
        );
        // The landing's probe is in the history.
        let stored = Store::open_at(&db)
            .unwrap()
            .latest(&[(Service::Codex, "b")])
            .unwrap();
        assert_eq!(stored.len(), 1);

        // Due again: the Codex is on b, healthy, and that is not news.
        mover.next = None;
        mover
            .tick_with(&fleet, Standing::On, Some(client.clone()))
            .await;
        finished(&mover).await;
        let lines = mover.tick_with(&fleet, Standing::On, Some(client)).await;
        assert!(lines.is_empty(), "{lines:?}");
        assert_eq!(fake.logins().len(), 1);
    }

    #[test]
    fn only_changes_are_logged_and_quiet_states_only_after_trouble() {
        let mut mover = Mover::recording_to("/nonexistent/s.db".into(), "/nonexistent/m".into());
        let look = |outcome: Outcome| Look {
            home: "/h".into(),
            seated: Some("a".into()),
            tightest: None,
            probed: Vec::new(),
            outcome,
        };
        assert!(mover.take_in(&[look(Outcome::NoSocket)]).is_empty());
        assert!(mover.take_in(&[look(Outcome::Healthy)]).is_empty());
        assert_eq!(mover.take_in(&[look(Outcome::Draining)]).len(), 1);
        assert!(mover.take_in(&[look(Outcome::Draining)]).is_empty());
        let recovered = mover.take_in(&[look(Outcome::Healthy)]);
        assert!(recovered[0].ends_with("staying"), "{recovered:?}");
        assert!(mover.take_in(&[look(Outcome::Healthy)]).is_empty());
        let failed = || {
            look(Outcome::Failed {
                to: "b".into(),
                message: "no".into(),
            })
        };
        assert_eq!(mover.take_in(&[failed()]).len(), 1);
        assert_eq!(
            mover.take_in(&[failed()]).len(),
            1,
            "every failed move is said"
        );
    }
}
