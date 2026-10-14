//! What the daemon remembers between cycles: when it last probed, the fleet
//! mode and next interval, which refused credentials are parked, and the
//! last target-model sample per account. A cache: losing it costs one cycle.

use std::collections::BTreeMap;

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::account::Account;
use crate::fs::write_atomic;
use crate::history::Fingerprint;
use crate::paths;
use crate::policy::Mode;
use crate::quota::{Bucket, Family, ModelKey, ProbeError, Reading, Source, Window};
use crate::settings::PolicySettings;

/// A refused credential and when to ask again. The refusal is an answer
/// about that credential only: once the account offers another one the
/// entry no longer applies.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Backoff {
    pub failures: u32,
    pub next_attempt_epoch: f64,
    pub last_error: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub credential: Option<Fingerprint>,
}

/// 5m, 10m, 20m, then every 30m.
fn backoff_secs(failures: u32) -> f64 {
    (300.0 * 2f64.powi(failures.saturating_sub(1).min(8) as i32)).min(1800.0)
}

/// The last probe made with the startup model for one account.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Sample {
    pub sampled_at: f64,
    /// Whether the sample got quota headers at all.
    #[serde(default)]
    pub answered: bool,
    /// The model's own weekly window (the Fable limit), when the response carried it.
    #[serde(default)]
    pub window: Option<Window>,
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct Cadence {
    #[serde(default)]
    pub last_probe_epoch: f64,
    #[serde(default)]
    pub mode: Option<Mode>,
    /// The next interval; faster than the mode's default while coverage is
    /// degraded or the default is near a floor.
    #[serde(default)]
    pub probe_interval_secs: Option<u64>,
    #[serde(default)]
    pub consecutive_degraded_probes: u32,
    #[serde(default, rename = "auth_backoff")]
    pub backoff: BTreeMap<String, Backoff>,
    #[serde(default, rename = "header_samples")]
    pub samples: BTreeMap<String, Sample>,
}

fn fingerprint(account: &Account, now: f64) -> Option<Fingerprint> {
    account
        .credential((now * 1000.0) as i64)
        .map(|credential| Fingerprint::of(credential.value()))
}

impl Cadence {
    pub fn load() -> Self {
        std::fs::read(paths::cadence_state())
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default()
    }

    pub fn save(&self) -> Result<()> {
        let mut bytes = serde_json::to_vec_pretty(self)?;
        bytes.push(b'\n');
        write_atomic(&paths::cadence_state(), &bytes, 0o600)
    }

    /// Forget accounts that left the fleet.
    pub fn retain(&mut self, accounts: &[Account]) {
        let configured = |name: &String| accounts.iter().any(|account| account.name == *name);
        self.backoff.retain(|name, _| configured(name));
        self.samples.retain(|name, _| configured(name));
    }

    /// Parked: the recheck is not due and the account still offers the
    /// credential that was refused.
    pub fn is_parked(&self, account: &Account, now: f64) -> bool {
        self.backoff.get(&account.name).is_some_and(|entry| {
            now < entry.next_attempt_epoch && entry.credential == fingerprint(account, now)
        })
    }

    /// Fold this cycle's readings into the backoff table; returns the
    /// accounts parked for the first time.
    pub fn record_refusals<'a>(
        &mut self,
        accounts: &[Account],
        readings: impl IntoIterator<Item = &'a Reading>,
        now: f64,
    ) -> Vec<String> {
        let mut newly_parked = Vec::new();
        for reading in readings {
            if reading.is_refused() {
                let entry = self
                    .backoff
                    .entry(reading.account.clone())
                    .or_insert(Backoff {
                        failures: 0,
                        next_attempt_epoch: 0.0,
                        last_error: String::new(),
                        credential: None,
                    });
                entry.failures = entry.failures.saturating_add(1);
                entry.last_error = reading
                    .error
                    .as_ref()
                    .map(ToString::to_string)
                    .unwrap_or_default();
                entry.next_attempt_epoch = now + backoff_secs(entry.failures);
                entry.credential = accounts
                    .iter()
                    .find(|account| account.name == reading.account)
                    .and_then(|account| fingerprint(account, now));
                if entry.failures == 1 {
                    newly_parked.push(reading.account.clone());
                }
            } else {
                self.backoff.remove(&reading.account);
            }
        }
        newly_parked
    }

    /// A refused account that still offers the same credential answers the
    /// same way; synthesize its refusal instead of asking again.
    pub fn synthetic_refusal(&self, account: &Account, now: f64) -> Option<Reading> {
        let entry = self.backoff.get(&account.name)?;
        if !self.is_parked(account, now) {
            return None;
        }
        Some(Reading::failed(
            &account.name,
            now as i64,
            ProbeError::Http {
                status: 401,
                body: format!("authentication previously refused: {}", entry.last_error),
            },
        ))
    }

    /// Whether to spend a target-model probe on this account this cycle. An
    /// account with profile reads already gets the limit as a bucket.
    pub fn due_for_sample(&self, account: &Account, policy: &PolicySettings, now: f64) -> bool {
        let interval = policy.target_model_sample_secs as f64;
        interval > 0.0
            && account.profile_token((now * 1000.0) as i64).is_none()
            && self
                .samples
                .get(&account.name)
                .is_none_or(|sample| now - sample.sampled_at >= interval)
    }

    /// Remember a sample; returns the accounts whose sample got no headers
    /// this time but had before (worth one log line, not one per cycle).
    pub fn record_samples<'a>(
        &mut self,
        samples: impl IntoIterator<Item = &'a Reading>,
        now: f64,
    ) -> Vec<String> {
        let fable = ModelKey::family_wide(Family::Fable);
        let mut newly_unanswered = Vec::new();
        for sample in samples {
            let answered = sample.has_quota();
            if !answered
                && self
                    .samples
                    .get(&sample.account)
                    .is_none_or(|previous| previous.answered)
            {
                newly_unanswered.push(sample.account.clone());
            }
            self.samples.insert(
                sample.account.clone(),
                Sample {
                    sampled_at: now,
                    answered,
                    window: sample.bucket(&fable).map(|bucket| bucket.window),
                },
            );
        }
        newly_unanswered
    }

    /// Carry each account's last sampled Fable window into a reading that
    /// has none, until that window resets.
    pub fn apply_samples(&self, readings: &mut [Reading], now: f64) {
        let fable = ModelKey::family_wide(Family::Fable);
        for reading in readings {
            if reading.bucket(&fable).is_some() {
                continue;
            }
            let carried = self
                .samples
                .get(&reading.account)
                .and_then(|sample| sample.window)
                .filter(|window| window.resets_at == 0 || window.resets_at as f64 > now);
            if let Some(window) = carried {
                reading.set_bucket(Bucket {
                    key: fable.clone(),
                    label: Family::Fable.label().to_owned(),
                    window,
                    source: Source::Carried,
                });
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::account::Login;
    use crate::quota::Verdict;

    fn account(name: &str) -> Account {
        Account {
            name: name.into(),
            setup_key: Some(format!("sk-ant-oat01-setup-{name}")),
            login: Some(Login {
                access_token: Some(format!("sk-ant-oat01-access-{name}")),
                refresh_token: Some("r".into()),
                expires_at: Some(i64::MAX / 2),
                obtained_at: Some(0),
                scopes: vec!["user:inference".into()],
                error: None,
            }),
        }
    }

    fn refused(name: &str) -> Reading {
        Reading::failed(
            name,
            0,
            ProbeError::Http {
                status: 401,
                body: "nope".into(),
            },
        )
    }

    fn fine(name: &str) -> Reading {
        Reading {
            account: name.into(),
            at: 0,
            verdict: Verdict::Allowed,
            claim: None,
            session: Some(Window::new(0.1, 0)),
            weekly: Some(Window::new(0.1, 0)),
            buckets: Vec::new(),
            overage: None,
            error: None,
            profile_error: None,
        }
    }

    #[test]
    fn backoff_schedule_climbs_then_caps_at_thirty_minutes() {
        assert_eq!(backoff_secs(1), 300.0);
        assert_eq!(backoff_secs(2), 600.0);
        assert_eq!(backoff_secs(3), 1200.0);
        assert_eq!(backoff_secs(4), 1800.0);
        assert_eq!(backoff_secs(50), 1800.0);
    }

    #[test]
    fn refusals_park_and_successes_clear() {
        let accounts = [account("dead")];
        let mut cadence = Cadence::default();
        let parked = cadence.record_refusals(&accounts, [&refused("dead")], 1_000.0);
        assert_eq!(parked, ["dead"]);
        assert!(cadence.is_parked(&accounts[0], 1_100.0));
        assert!(!cadence.is_parked(&accounts[0], 1_400.0));
        assert!(
            cadence
                .synthetic_refusal(&accounts[0], 1_100.0)
                .unwrap()
                .is_refused()
        );
        // A second refusal in a streak is not "newly parked".
        assert!(
            cadence
                .record_refusals(&accounts, [&refused("dead")], 2_000.0)
                .is_empty()
        );
        assert_eq!(cadence.backoff["dead"].failures, 2);
        cadence.record_refusals(&accounts, [&fine("dead")], 3_000.0);
        assert!(cadence.backoff.is_empty());
    }

    #[test]
    fn a_changed_credential_ends_the_backoff_early() {
        let mut dead = account("dead");
        let mut cadence = Cadence::default();
        cadence.record_refusals(std::slice::from_ref(&dead), [&refused("dead")], 1_000.0);
        assert!(cadence.is_parked(&dead, 1_100.0));
        dead.login.as_mut().unwrap().access_token = Some("sk-ant-oat01-access-dead-2".into());
        assert!(!cadence.is_parked(&dead, 1_100.0));
        cadence.backoff.get_mut("dead").unwrap().credential = None;
        assert!(!cadence.is_parked(&dead, 1_100.0));
    }

    #[test]
    fn transient_failures_never_park() {
        let accounts = [account("flaky")];
        let mut cadence = Cadence::default();
        let timeout = Reading::failed(
            "flaky",
            0,
            ProbeError::Network {
                message: "timed out".into(),
            },
        );
        assert!(
            cadence
                .record_refusals(&accounts, [&timeout], 1_000.0)
                .is_empty()
        );
        assert!(cadence.backoff.is_empty());
    }

    #[test]
    fn state_forgets_accounts_that_left_the_fleet() {
        let mut cadence = Cadence::default();
        for name in ["kept", "removed"] {
            cadence.record_refusals(&[account(name)], [&refused(name)], 1.0);
            cadence.samples.insert(
                name.into(),
                Sample {
                    sampled_at: 0.0,
                    answered: true,
                    window: None,
                },
            );
        }
        cadence.retain(&[account("kept")]);
        assert_eq!(cadence.backoff.keys().collect::<Vec<_>>(), ["kept"]);
        assert_eq!(cadence.samples.keys().collect::<Vec<_>>(), ["kept"]);
    }

    #[test]
    fn samples_are_recorded_then_carried_until_reset() {
        let policy = PolicySettings::default();
        let mut cadence = Cadence::default();
        let mut setup_only = account("s");
        setup_only.login = None;
        assert!(cadence.due_for_sample(&setup_only, &policy, 10.0));
        assert!(!cadence.due_for_sample(&account("with-profile"), &policy, 10.0));

        let mut sampled = fine("s");
        sampled.set_bucket(Bucket {
            key: ModelKey::family_wide(Family::Fable),
            label: "Fable".into(),
            window: Window::new(0.6, 5_000),
            source: Source::Headers,
        });
        assert!(cadence.record_samples([&sampled], 10.0).is_empty());
        assert!(!cadence.due_for_sample(&setup_only, &policy, 20.0));
        assert!(cadence.due_for_sample(
            &setup_only,
            &policy,
            10.0 + policy.target_model_sample_secs as f64
        ));

        let mut readings = [fine("s")];
        cadence.apply_samples(&mut readings, 100.0);
        let carried = readings[0]
            .bucket(&ModelKey::family_wide(Family::Fable))
            .unwrap();
        assert_eq!(carried.source, Source::Carried);
        assert_eq!(carried.window, Window::new(0.6, 5_000));
        let mut after_reset = [fine("s")];
        cadence.apply_samples(&mut after_reset, 6_000.0);
        assert!(after_reset[0].buckets.is_empty());

        // An unanswered sample is reported once, not every cycle.
        let unanswered = Reading::failed(
            "s",
            0,
            ProbeError::Http {
                status: 429,
                body: String::new(),
            },
        );
        assert_eq!(cadence.record_samples([&unanswered], 200.0), ["s"]);
        assert!(cadence.record_samples([&unanswered], 300.0).is_empty());
    }

    #[test]
    fn the_state_file_keeps_its_spelling() {
        let mut cadence = Cadence::default();
        cadence.mode = Some(Mode::Sip);
        cadence.record_refusals(&[account("a")], [&refused("a")], 1.0);
        let json = serde_json::to_string(&cadence).unwrap();
        assert!(json.contains("\"mode\":\"sip-and-drain\""));
        assert!(json.contains("\"auth_backoff\":{\"a\":{\"failures\":1"));
        assert!(json.contains("\"header_samples\":{}"));
        assert_eq!(serde_json::from_str::<Cadence>(&json).unwrap(), cadence);
    }
}
