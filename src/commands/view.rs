//! Reading and ranking the whole fleet: the shared probe, the fleet view
//! (stored or read now), the one ranking behind every `best`, history and
//! stats.

use anyhow::{Result, bail};
use serde::Serialize;
use std::path::PathBuf;

use super::now;
use crate::account::Fleet;
use crate::anthropic;
use crate::claude::settings as claude_settings;
use crate::codex;
use crate::daemon::notify::{self, Delivery};
use crate::policy::{self, Floors};
use crate::providers::{self, ServiceAccount};
use crate::quota::{ModelKey, Reading, Service};
use crate::store::{Stats, Store};
use crate::ui::overview::{Facts, ForecastView, Monitor, Overview};

/// No floor: any counted window with something left can take work.
pub const ANY_ROOM: Floors = Floors {
    session: 0.0,
    weekly: 0.0,
};

/// Read every account of `only` (every service when `None`) now, record the
/// reads in the history, and return them: Claude, then Codex, then the
/// services, each in configured order. The three sweeps run concurrently and
/// each is bounded by its own timeouts. A read that failed is a `Reading`
/// with `error` set.
pub async fn probe(fleet: &Fleet, only: Option<Service>) -> Vec<Reading> {
    let wants = |service: Service| only.is_none_or(|only| only == service);
    let at = now();
    let claude = async {
        if !wants(Service::Claude) || fleet.accounts.is_empty() {
            return Vec::new();
        }
        let mut readings = anthropic::probe::probe_all(
            anthropic::Client::shared(),
            fleet.accounts.iter(),
            anthropic::probe::PROBE_MODEL,
            anthropic::probe::PROBE_TIMEOUT,
            at,
        )
        .await;
        let _ = crate::observe::scan_transcripts(&fleet.accounts);
        crate::observe::apply(&mut readings);
        readings
    };
    let codex = async {
        if !wants(Service::Codex) || fleet.codex.is_empty() {
            return Vec::new();
        }
        match codex::Client::new() {
            Ok(client) => {
                let mut probes = codex::probe::probe_all(&client, &fleet.codex).await;
                codex::resets::with_expiry(&client, &fleet.codex, &mut probes).await;
                probes
            }
            Err(error) => fleet
                .codex
                .iter()
                .map(|account| {
                    Reading::failed(
                        Service::Codex,
                        &account.name,
                        at,
                        crate::quota::ReadError::Other {
                            message: format!("{error:#}"),
                        },
                    )
                })
                .collect(),
        }
    };
    let services = async {
        let accounts: Vec<ServiceAccount> = fleet
            .services
            .iter()
            .filter(|account| wants(account.service))
            .cloned()
            .collect();
        providers::read_all(providers::client(), &accounts, at).await
    };
    let (claude, codex, services) = tokio::join!(claude, codex, services);
    let readings: Vec<Reading> = claude.into_iter().chain(codex).chain(services).collect();
    record(&readings);
    readings
}

/// Keep readings in the history, for the surfaces that show stored reads.
pub fn record(readings: &[Reading]) {
    let Ok(store) = Store::open() else {
        return;
    };
    for reading in readings {
        let _ = store.insert(reading);
    }
}

/// The fleet view from what is stored: no request and no write. Each
/// reading is as old as its `at`.
pub async fn stored(fleet: &Fleet) -> Overview {
    let at = now();
    let mut facts = Facts::on_disk(fleet, at);
    facts.agy_account = super::services::seated(&fleet.services).await;
    Overview::build(fleet, facts, None, at)
}

/// The fleet view of readings made just now, over what is stored about the
/// rest; of one service's accounts with `only`.
pub async fn shown(fleet: &Fleet, readings: Vec<Reading>, only: Option<Service>) -> Overview {
    let at = now();
    let mut facts = Facts::on_disk(fleet, at).with_fresh(readings);
    let claude: Vec<Reading> = facts
        .readings
        .iter()
        .filter(|reading| reading.service == Service::Claude)
        .cloned()
        .collect();
    if only.is_none_or(|only| only == Service::Claude) && !claude.is_empty() {
        let model = facts.target_model.as_deref().map(ModelKey::new);
        facts.mode = policy::safe_mode_for(
            &claude,
            fleet.accounts.len(),
            &fleet.policy,
            facts.mode,
            model.as_ref(),
        );
        facts.probe_interval_secs = None;
    }
    if only.is_none_or(|only| only == Service::Antigravity) {
        facts.agy_account = super::services::seated(&fleet.services).await;
    }
    Overview::build(fleet, facts, only, at)
}

/// The fleet view, read now: every account of `only` (every service when
/// `None`) through [`probe`].
pub async fn fresh(fleet: &Fleet, only: Option<Service>) -> Overview {
    let readings = probe(fleet, only).await;
    shown(fleet, readings, only).await
}

/// The fleet view a surface shows. While the background service runs it is
/// the only reader, and this is the stored view: no request is sent.
/// Without it, or with `force`, every account is read now.
pub async fn overview(fleet: &Fleet, force: bool) -> Overview {
    if !force && Monitor::now().keeps_fresh() {
        stored(fleet).await
    } else {
        fresh(fleet, None).await
    }
}

/// What part of a reading an account is ranked on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RankOn<'a> {
    /// The account-wide windows, and the limits on the policy's model.
    Account,
    /// The scoped limits matching this text, or every group's when `None`
    /// ([`Reading::scoped_to`]): they are metered apart from the account.
    Limits(Option<&'a str>),
}

/// The one ranking: the readings above `floors`, best landing first
/// ([`policy::rank_above`]), each with the fraction left on its tightest
/// counted limit. A failed read, a refused account and one with nothing to
/// count are left out.
pub fn ranked<'a>(
    readings: &'a [Reading],
    floors: Floors,
    model: Option<&ModelKey>,
    on: RankOn<'_>,
) -> Vec<(&'a Reading, f64)> {
    let counted: Vec<Reading> = readings
        .iter()
        .filter_map(|reading| match on {
            RankOn::Account => Some(reading.clone()),
            RankOn::Limits(needle) => reading.scoped_to(needle),
        })
        .collect();
    policy::rank_above(&counted, floors, model)
        .into_iter()
        .filter_map(|best| {
            let original = readings.iter().find(|reading| {
                reading.service == best.service && reading.account == best.account
            })?;
            Some((original, best.tightest()?.window.remaining()))
        })
        .collect()
}

/// The accounts of one service, read now and ranked.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Best {
    pub service: Service,
    /// Every read, in configured order, failures included.
    pub readings: Vec<Reading>,
    /// Account names, best first, with the fraction left on the tightest
    /// counted limit.
    pub ranked: Vec<(String, f64)>,
    /// The account the tool holds now: Claude's default, agy's login.
    pub current: Option<String>,
    /// Codex: the winner's CODEX_HOME.
    pub home: Option<PathBuf>,
    /// What `--limit` / `--group` asked to rank on.
    pub scope: Option<String>,
}

impl Best {
    pub fn winner(&self) -> Option<&str> {
        self.ranked.first().map(|(name, _)| name.as_str())
    }

    pub fn reading(&self, account: &str) -> Option<&Reading> {
        self.readings
            .iter()
            .find(|reading| reading.account == account)
    }
}

/// Read `service`'s accounts now and rank them: `tokeman <service> best`.
/// Claude is ranked as the rotation would land (the landing floors, the
/// selection model); Codex on its account-wide windows or on `scope`'s limit,
/// parked profiles left out; the others on `scope`'s group or on every group.
pub async fn best(fleet: &Fleet, service: Service, scope: Option<&str>) -> Result<Best> {
    let configured = match service {
        Service::Claude => fleet.accounts.len(),
        Service::Codex => fleet.codex.len(),
        other => fleet
            .services
            .iter()
            .filter(|account| account.service == other)
            .count(),
    };
    if configured == 0 {
        bail!("no {} accounts are registered", service.label());
    }
    let readings = probe(fleet, Some(service)).await;
    let (ranked, current, home) = match service {
        Service::Claude => {
            let model = target_model();
            let ranked = names(ranked(
                &readings,
                fleet.policy.landing_floors(),
                model.as_ref(),
                RankOn::Account,
            ));
            let current = crate::daemon::default_account(fleet).ok().flatten();
            (ranked, current, None)
        }
        Service::Codex => {
            let ranked = names(codex_ranked(&readings, fleet, scope));
            let home = ranked
                .first()
                .and_then(|(name, _)| fleet.codex_account(name))
                .map(|account| account.home());
            (ranked, None, home)
        }
        _ => {
            let ranked = names(ranked(&readings, ANY_ROOM, None, RankOn::Limits(scope)));
            let current = super::services::seated(&fleet.services).await;
            (ranked, current, None)
        }
    };
    Ok(Best {
        service,
        readings,
        ranked,
        current,
        home,
        scope: scope.map(str::to_owned),
    })
}

fn names(ranked: Vec<(&Reading, f64)>) -> Vec<(String, f64)> {
    ranked
        .into_iter()
        .map(|(reading, room)| (reading.account.clone(), room))
        .collect()
}

/// Codex accounts work can move to, best first: unparked, accepted by the
/// backend, with room in every account-wide window, or, with `limit`, ranked
/// on that per-model or per-feature limit alone. An accepted account that
/// reports no window at all has nothing to rank on and comes last.
pub fn codex_ranked<'a>(
    readings: &'a [Reading],
    fleet: &Fleet,
    limit: Option<&str>,
) -> Vec<(&'a Reading, f64)> {
    let candidates: Vec<&Reading> = readings
        .iter()
        .filter(|reading| reading.service == Service::Codex)
        .filter(|reading| {
            fleet
                .codex_account(&reading.account)
                .is_none_or(|account| !account.parked)
        })
        .collect();
    let on = match limit {
        Some(limit) => RankOn::Limits(Some(limit)),
        None => RankOn::Account,
    };
    let owned: Vec<Reading> = candidates
        .iter()
        .map(|reading| (*reading).clone())
        .collect();
    let mut order: Vec<(String, f64)> = names(ranked(&owned, ANY_ROOM, None, on));
    if limit.is_none() {
        order.extend(
            candidates
                .iter()
                .filter(|reading| reading.accepts_work() && reading.limits.is_empty())
                .map(|reading| (reading.account.clone(), 1.0)),
        );
    }
    order
        .into_iter()
        .filter_map(|(name, room)| {
            let reading = candidates.iter().find(|reading| reading.account == name)?;
            Some((*reading, room))
        })
        .collect()
}

/// The model Claude Code starts with, as its settings name it.
pub fn target_model() -> Option<ModelKey> {
    let settings = claude_settings::load().ok()?;
    claude_settings::target_model(&settings).map(ModelKey::new)
}

/// Stored reads of one service, newest first: the last `last`, or everything
/// in the last `since_hours`. Writes nothing; no history yet reads as empty.
pub fn history(
    service: Service,
    account: Option<&str>,
    last: usize,
    since_hours: Option<f64>,
) -> Result<Vec<Reading>> {
    let Ok(store) = Store::open_read_only() else {
        return Ok(Vec::new());
    };
    match since_hours {
        Some(hours) => store.since(service, account, now() - (hours * 3_600.0) as i64),
        None => store.recent(service, account, last),
    }
}

/// Burn rates of each account of `service` over the last week, in
/// configured order. Writes nothing; no history yet is no stats.
pub fn stats(fleet: &Fleet, service: Service) -> Vec<Stats> {
    let Ok(store) = Store::open_read_only() else {
        return Vec::new();
    };
    let since = now() - 7 * 86_400;
    fleet
        .account_keys()
        .into_iter()
        .filter(|(of, _)| *of == service)
        .filter_map(|(_, account)| {
            let readings = store.since(service, Some(account), since).ok()?;
            Some(Stats::compute(account, &readings))
        })
        .collect()
}

/// When `service`'s accounts run dry at the recent rate, from what is
/// stored; `None` when the history cannot support a forecast.
pub fn forecast(fleet: &Fleet, service: Service) -> Option<ForecastView> {
    let at = now();
    Overview::build(fleet, Facts::on_disk(fleet, at), Some(service), at)
        .forecasts
        .pop()
}

/// `tokeman notify --test`: one desktop notification, sent now.
pub async fn notify_test() -> Delivery {
    notify::deliver(
        notify::TITLE,
        "This is a test. What needs you will arrive like this.",
    )
    .await
}

/// The launch settings and the Codex mover's, as `key = value` pairs.
pub fn settings() -> Result<Vec<(&'static str, String)>> {
    let fleet = Fleet::load()?;
    let mut entries = fleet.launch.entries().to_vec();
    entries.extend(fleet.policy.entries());
    Ok(entries)
}

pub fn set_setting(key: &str, value: &str) -> Result<()> {
    use crate::settings::{LaunchSettings, PolicySettings};
    if !LaunchSettings::KEYS.contains(&key) && !PolicySettings::KEYS.contains(&key) {
        let known: Vec<&str> = LaunchSettings::KEYS
            .into_iter()
            .chain(PolicySettings::KEYS)
            .collect();
        bail!("unknown setting {key:?}; one of {}", known.join(", "));
    }
    Fleet::update(|fleet| {
        if PolicySettings::KEYS.contains(&key) {
            fleet.policy.set(key, value)
        } else {
            fleet.launch.set(key, value)
        }
    })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::account::CodexAccount;
    use crate::codex::probe::tests::{limited_body, usage_body, weekly_only_body};
    use crate::quota::ReadError;
    use crate::testing::codex_account;
    use chrono::{DateTime, Utc};
    use serde_json::json;

    fn fleet_of(accounts: Vec<CodexAccount>) -> Fleet {
        Fleet {
            codex: accounts,
            ..Fleet::default()
        }
    }

    /// The account `tokeman codex best` would name.
    fn winner(readings: &[Reading], fleet: &Fleet, limit: Option<&str>) -> Option<String> {
        codex_ranked(readings, fleet, limit)
            .first()
            .map(|(reading, _)| reading.account.clone())
    }

    fn probe_from(body: &serde_json::Value, name: &str, at: DateTime<Utc>) -> Reading {
        crate::codex::probe::decode_usage(name, &body.to_string(), at).unwrap()
    }

    fn probe_with(name: &str, remaining: f64, spark: Option<f64>) -> Reading {
        let at = DateTime::from_timestamp(0, 0).unwrap();
        let mut body = usage_body(100.0 * (1.0 - remaining), 0.0);
        body["email"] = json!(format!("{name}@x.y"));
        match spark {
            Some(left) => {
                body["additional_rate_limits"][0]["rate_limit"]["limit_reached"] = json!(false);
                body["additional_rate_limits"][0]["rate_limit"]["allowed"] = json!(true);
                body["additional_rate_limits"][0]["rate_limit"]["primary_window"]["used_percent"] =
                    json!(100.0 * (1.0 - left));
            }
            None => body["additional_rate_limits"] = json!([]),
        }
        probe_from(&body, name, at)
    }

    #[test]
    fn best_skips_parked_and_unviable_and_ranks_buckets_separately() {
        let mut probes = vec![
            probe_with("roomy", 0.9, None),
            probe_with("parked", 0.95, Some(0.5)),
            probe_with("spark", 0.3, Some(0.4)),
            probe_with("dead", 1.0, Some(1.0)),
        ];
        probes[3] = Reading::failed(
            crate::quota::Service::Codex,
            "dead",
            0,
            ReadError::Http {
                status: 401,
                body: String::new(),
            },
        );
        let fleet = fleet_of(vec![CodexAccount {
            parked: true,
            ..codex_account("parked", "/homes/parked")
        }]);
        assert_eq!(winner(&probes, &fleet, None).unwrap(), "roomy");
        assert_eq!(winner(&probes, &fleet, Some("spark")).unwrap(), "spark");
        assert_eq!(winner(&probes, &fleet, Some("nothing-like-it")), None);
        let mut body = usage_body(1.0, 0.0);
        body["rate_limit"]["limit_reached"] = json!(true);
        let limited = probe_from(&body, "limited", DateTime::from_timestamp(0, 0).unwrap());
        assert_eq!(winner(&[limited], &fleet, None), None);
        // A spent window is not a landing, whatever the backend's flag says.
        assert_eq!(
            winner(&[probe_with("spent", 0.0, None)], &fleet, None),
            None
        );
        // Ties fall to the policy's last tie-break, the name.
        let tied = vec![probe_with("b", 0.5, None), probe_with("a", 0.5, None)];
        assert_eq!(winner(&tied, &fleet, None).unwrap(), "a");
        // An accepted account that reports no window is the last resort.
        let bare = probe_from(&json!({"email": "bare@x.y"}), "bare", DateTime::UNIX_EPOCH);
        assert_eq!(
            winner(&[bare.clone(), tied[0].clone()], &fleet, None).unwrap(),
            "b"
        );
        assert_eq!(
            winner(std::slice::from_ref(&bare), &fleet, None).unwrap(),
            "bare"
        );
        assert_eq!(winner(&[bare], &fleet, Some("spark")), None);
    }

    #[test]
    fn a_weekly_only_account_is_ranked_on_its_weekly_window() {
        let at = DateTime::from_timestamp(0, 0).unwrap();
        let fleet = Fleet::default();
        // 5h nearly empty but weekly 80% used: 20% left.
        let both = probe_from(&usage_body(5.0, 80.0), "both", at);
        // Only a weekly window, 42% used: 58% left, in `primary_window`.
        let weekly = probe_from(&weekly_only_body(42.0), "weekly", at);
        assert!((weekly.least_remaining().unwrap() - 0.58).abs() < 1e-9);
        assert_eq!(
            winner(&[weekly.clone(), both.clone()], &fleet, None).unwrap(),
            "weekly"
        );
        let fuller = probe_from(&weekly_only_body(90.0), "fuller", at);
        assert_eq!(winner(&[fuller, both], &fleet, None).unwrap(), "both");
        let limited = probe_from(&limited_body(), "limited", at);
        assert_eq!(winner(&[limited], &fleet, None), None);
    }

    fn service_reading(service: Service, account: &str, groups: &[(&str, f64)]) -> Reading {
        Reading {
            limits: groups
                .iter()
                .map(|(group, used)| {
                    providers::limit(
                        Some((*group).to_owned()),
                        "weekly",
                        Some(7 * 86_400),
                        crate::quota::Window::new(*used, 0),
                        None,
                    )
                })
                .collect(),
            ..Reading::empty(service, account, 0)
        }
    }

    #[test]
    fn every_service_ranks_through_the_one_function() {
        // Antigravity: on one group, or on every group when none is named.
        let agy = vec![
            service_reading(
                Service::Antigravity,
                "a",
                &[("Gemini", 0.9), ("Claude", 0.1)],
            ),
            service_reading(
                Service::Antigravity,
                "b",
                &[("Gemini", 0.2), ("Claude", 0.6)],
            ),
            Reading::failed(
                Service::Antigravity,
                "dead",
                0,
                ReadError::Network {
                    message: "down".into(),
                },
            ),
        ];
        let order = |on| -> Vec<(String, i64)> {
            ranked(&agy, ANY_ROOM, None, on)
                .into_iter()
                .map(|(reading, room)| (reading.account.clone(), (room * 100.0).round() as i64))
                .collect()
        };
        assert_eq!(
            order(RankOn::Limits(Some("claude"))),
            [("a".to_owned(), 90), ("b".to_owned(), 40)]
        );
        assert_eq!(
            order(RankOn::Limits(Some("gemini"))),
            [("b".to_owned(), 80), ("a".to_owned(), 10)]
        );
        // Every group counts: each account is as roomy as its tightest one.
        assert_eq!(
            order(RankOn::Limits(None)),
            [("b".to_owned(), 40), ("a".to_owned(), 10)]
        );
        assert!(order(RankOn::Limits(Some("nothing-like-it"))).is_empty());
        // A group limit does not gate the account itself.
        assert!(order(RankOn::Account).is_empty());
    }
}
