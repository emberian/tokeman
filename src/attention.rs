//! What needs the user, found in the fleet view. Pure: no IO, no clock.
//!
//! Each [`Attention`] has a stable [`key`](Attention::key). The background
//! service notifies a key once (`daemon::notify`), so anything that should
//! be said again when it gets closer carries its threshold ([`Within`]) in
//! the key.

use serde::Serialize;

use crate::codex::keep::RENEW_WITHIN_SECS;
use crate::codex::mover;
use crate::codex::seats::Cause;
use crate::forecast::Outlook;
use crate::quota::{Level, Service};
use crate::ui::overview::{Monitor, Overview};

const DAY_SECS: i64 = 86_400;
/// A fleet forecast to run dry sooner than this needs the user.
pub const DRY_WITHIN_SECS: i64 = 12 * 3_600;

/// How close an end is. A date inside the nearer threshold is reported at
/// that one alone.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
pub enum Within {
    #[serde(rename = "1d")]
    OneDay,
    #[serde(rename = "3d")]
    ThreeDays,
}

impl Within {
    /// The threshold `at` falls in, seen from `now`; `None` when it is more
    /// than three days out. A date already passed is within a day.
    pub fn of(at: i64, now: i64) -> Option<Self> {
        match at - now {
            left if left <= DAY_SECS => Some(Self::OneDay),
            left if left <= 3 * DAY_SECS => Some(Self::ThreeDays),
            _ => None,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::OneDay => "1d",
            Self::ThreeDays => "3d",
        }
    }
}

/// Why a pushed Codex seat was not renewed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Unkept {
    /// The background service tried and could not.
    Tried(Cause),
    /// Rotation is paused: the service renews nothing.
    Paused,
    /// No background service runs: nothing renews it.
    NoDaemon,
    /// The service runs and has not renewed it yet.
    Overdue,
}

impl Unkept {
    pub fn name(self) -> &'static str {
        match self {
            Self::Tried(cause) => cause.name(),
            Self::Paused => "paused",
            Self::NoDaemon => "no-daemon",
            Self::Overdue => "overdue",
        }
    }
}

/// One thing that needs the user.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum Attention {
    /// The background service is installed and no daemon runs: nothing
    /// reads, rotates or notifies.
    DaemonStopped,
    /// No account of the service can take work: at least one was read below
    /// its floor and none was read above.
    BelowFloor { service: Service },
    /// The service's accounts are forecast to run dry within
    /// [`DRY_WITHIN_SECS`].
    RunsDry { service: Service, at: i64 },
    /// The refresh token of a Claude login was refused for good.
    LoginRefused { account: String },
    /// A Claude login's refresh token dies at `ends_at` (epoch seconds).
    LoginEnds {
        account: String,
        ends_at: i64,
        within: Within,
    },
    /// A Claude login with no reported end is old enough to end soon.
    LoginAgeing { account: String },
    /// A reset on offer (a Claude grant, a Codex credit) stops being usable
    /// at `expires_at`.
    ResetExpires {
        service: Service,
        account: String,
        expires_at: i64,
        within: Within,
    },
    /// The Codex running on `home` holds a token tokeman pushed, which it
    /// cannot renew itself, and nothing has renewed it: its requests fail
    /// from `expires_at` (now, when the backend already rejects the token).
    CodexSeatExpires {
        home: String,
        account: String,
        /// Where the account's profile logs in, when it is registered.
        profile_home: Option<String>,
        expires_at: Option<i64>,
        why: Unkept,
    },
}

impl Attention {
    /// What this is, the same for as long as it stands and different from
    /// every other.
    pub fn key(&self) -> String {
        match self {
            Self::DaemonStopped => "daemon-stopped".to_owned(),
            Self::BelowFloor { service } => format!("below-floor:{}", service.name()),
            Self::RunsDry { service, .. } => format!("runs-dry:{}", service.name()),
            Self::LoginRefused { account } => format!("login-refused:{account}"),
            Self::LoginEnds {
                account, within, ..
            } => format!("login-ends:{account}:{}", within.name()),
            Self::LoginAgeing { account } => format!("login-ageing:{account}"),
            Self::ResetExpires {
                service,
                account,
                expires_at,
                within,
            } => format!(
                "reset-expires:{}:{account}:{expires_at}:{}",
                service.name(),
                within.name()
            ),
            Self::CodexSeatExpires {
                home,
                expires_at,
                why,
                ..
            } => format!(
                "codex-seat:{home}:{}:{}",
                expires_at.unwrap_or(0),
                why.name()
            ),
        }
    }

    /// How urgent it is: `Critical` when work stops or something is lost
    /// within a day, `Low` otherwise.
    pub fn level(&self) -> Level {
        match self {
            Self::DaemonStopped
            | Self::BelowFloor { .. }
            | Self::RunsDry { .. }
            | Self::CodexSeatExpires { .. }
            | Self::LoginRefused { .. } => Level::Critical,
            Self::LoginEnds { within, .. } | Self::ResetExpires { within, .. } => match within {
                Within::OneDay => Level::Critical,
                Within::ThreeDays => Level::Low,
            },
            Self::LoginAgeing { .. } => Level::Low,
        }
    }
}

/// Everything in `overview` that needs the user at `now`, the critical
/// items first.
pub fn find(overview: &Overview, now: i64) -> Vec<Attention> {
    let mut found = Vec::new();
    if overview.monitor == Monitor::Stopped {
        found.push(Attention::DaemonStopped);
    }
    for service in overview.services() {
        let verdicts: Vec<bool> = overview
            .of(service)
            .filter(|account| !account.parked)
            .filter(|account| {
                account
                    .reading
                    .as_ref()
                    .is_some_and(|reading| reading.error.is_none())
            })
            .filter_map(|account| account.viable)
            .collect();
        if !verdicts.is_empty() && verdicts.iter().all(|viable| !viable) {
            found.push(Attention::BelowFloor { service });
        }
    }
    for forecast in &overview.forecasts {
        if let Outlook::Dry { at } = forecast.forecast.outlook
            && at - now < DRY_WITHIN_SECS
        {
            found.push(Attention::RunsDry {
                service: forecast.forecast.service,
                at,
            });
        }
    }
    for account in &overview.accounts {
        let Some(login) = &account.login else {
            continue;
        };
        let name = || account.name.clone();
        if login.refused {
            found.push(Attention::LoginRefused { account: name() });
        } else if let Some(ends_at) = login.ends_at {
            if let Some(within) = Within::of(ends_at, now) {
                found.push(Attention::LoginEnds {
                    account: name(),
                    ends_at,
                    within,
                });
            }
        } else if login.warning {
            found.push(Attention::LoginAgeing { account: name() });
        }
    }
    for offer in &overview.resets_on_offer {
        if let Some(expires_at) = offer.expires_at.filter(|at| *at > now)
            && let Some(within) = Within::of(expires_at, now)
        {
            found.push(Attention::ResetExpires {
                service: Service::Claude,
                account: offer.account.clone(),
                expires_at,
                within,
            });
        }
    }
    for account in overview.of(Service::Codex) {
        let credits = account
            .reading
            .as_ref()
            .and_then(|reading| reading.codex())
            .and_then(|codex| codex.reset_credits)
            .filter(|credits| credits.available > 0);
        if let Some(expires_at) = credits
            .and_then(|credits| credits.expires_at)
            .filter(|at| *at > now)
            && let Some(within) = Within::of(expires_at, now)
        {
            found.push(Attention::ResetExpires {
                service: Service::Codex,
                account: account.name.clone(),
                expires_at,
                within,
            });
        }
    }
    for view in &overview.codex_seats {
        let seat = &view.seat;
        let left = seat.expires_at.map(|at| at - now);
        // Long past its end, the session has failed already and the entry
        // only waits for a service to forget it.
        if left.is_some_and(|left| left < -DAY_SECS) {
            continue;
        }
        let due = left.is_some_and(|left| left <= RENEW_WITHIN_SECS);
        let why = match (&seat.trouble, overview.monitor) {
            (Some(trouble), _) => Unkept::Tried(trouble.cause),
            (None, _) if !due => continue,
            (None, Monitor::Paused) => Unkept::Paused,
            (None, Monitor::Stopped | Monitor::NotInstalled) => Unkept::NoDaemon,
            // The service renews at its next look; a few looks later it
            // should have.
            (None, Monitor::Running) => {
                let grace = 3 * mover::INTERVAL.as_secs() as i64;
                if left.is_some_and(|left| left > RENEW_WITHIN_SECS - grace) {
                    continue;
                }
                Unkept::Overdue
            }
        };
        found.push(Attention::CodexSeatExpires {
            home: seat.home.clone(),
            account: seat.account.clone(),
            profile_home: view.profile_home.clone(),
            expires_at: seat.expires_at,
            why,
        });
    }
    // Stable: within a level the order above stands.
    found.sort_by_key(|item| item.level() != Level::Critical);
    found
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::account::Fleet;
    use crate::quota::{CodexDetail, Detail, Reading, ResetCreditCount, Window};
    use crate::testing::{codex_account, login_account, setup_account};
    use crate::ui::overview::{Facts, ResetOffer};

    const NOW: i64 = 1_794_000_000;
    const HOUR: i64 = 3_600;

    fn claude(account: &str, weekly_used: f64) -> Reading {
        let mut reading = crate::testing::reading(account, 0.0, weekly_used);
        reading.at = NOW - 60;
        reading.set_weekly(Some(Window::new(weekly_used, NOW + 5 * DAY_SECS)));
        reading
    }

    /// A login account whose refresh token ends `secs` from now.
    fn ending(name: &str, secs: i64) -> crate::account::Account {
        let mut account = login_account(name, NOW * 1000 + 3_600_000);
        account.login.as_mut().unwrap().refresh_expires_at = Some((NOW + secs) * 1000);
        account
    }

    fn view(fleet: &Fleet, facts: Facts) -> Vec<Attention> {
        find(&Overview::build(fleet, facts, None, NOW), NOW)
    }

    fn keys(found: &[Attention]) -> Vec<String> {
        found.iter().map(Attention::key).collect()
    }

    #[test]
    fn a_pushed_codex_seat_nothing_renewed_needs_the_user_before_it_ends() {
        use crate::codex::seats::Trouble;
        use crate::codex::seats::tests::pushed;
        let fleet = Fleet {
            codex: vec![codex_account("b", "~/.codex-homes/b")],
            ..Fleet::default()
        };
        let found = |seat: crate::codex::seats::Pushed, monitor: Monitor| {
            let facts = Facts {
                codex_seats: vec![seat],
                monitor,
                ..Facts::default()
            };
            view(&fleet, facts)
        };
        let seat = |left: i64| pushed("~/.codex", "b", Some(NOW + left));
        let why = |found: &[Attention]| match found {
            [Attention::CodexSeatExpires { why, .. }] => Some(*why),
            [] => None,
            other => panic!("{other:?}"),
        };

        // Days left: nothing, whoever runs.
        assert_eq!(found(seat(5 * DAY_SECS), Monitor::NotInstalled), []);
        // Inside the renewal margin the running service gets a few looks.
        assert_eq!(found(seat(RENEW_WITHIN_SECS - 60), Monitor::Running), []);
        assert_eq!(
            why(&found(seat(RENEW_WITHIN_SECS - HOUR), Monitor::Running)),
            Some(Unkept::Overdue)
        );
        // Nobody to renew it is said as soon as it is due.
        assert_eq!(
            why(&found(seat(RENEW_WITHIN_SECS - 60), Monitor::Paused)),
            Some(Unkept::Paused)
        );
        for monitor in [Monitor::Stopped, Monitor::NotInstalled] {
            let found = found(seat(3 * HOUR), monitor);
            let seats: Vec<_> = found
                .iter()
                .filter(|item| matches!(item, Attention::CodexSeatExpires { .. }))
                .cloned()
                .collect();
            assert_eq!(why(&seats), Some(Unkept::NoDaemon), "{monitor:?}");
        }

        // A renewal that failed is said with its cause, even days before
        // the expiry: the token may be dead already.
        let mut refused = seat(5 * DAY_SECS);
        refused.trouble = Some(Trouble {
            cause: Cause::Refused,
            at: NOW,
            message: "revoked".into(),
        });
        let items = found(refused.clone(), Monitor::Running);
        assert_eq!(
            items,
            [Attention::CodexSeatExpires {
                home: "~/.codex".into(),
                account: "b".into(),
                profile_home: Some("~/.codex-homes/b".into()),
                expires_at: Some(NOW + 5 * DAY_SECS),
                why: Unkept::Tried(Cause::Refused),
            }]
        );
        assert_eq!(items[0].level(), Level::Critical);
        assert_eq!(
            items[0].key(),
            format!("codex-seat:~/.codex:{}:refused", NOW + 5 * DAY_SECS)
        );
        // Another cause is another key, so it is notified again.
        refused.trouble.as_mut().unwrap().cause = Cause::Unanswered;
        assert_ne!(found(refused, Monitor::Running)[0].key(), items[0].key());
        // More than a day past its end the session is over.
        assert_eq!(found(seat(-2 * DAY_SECS), Monitor::NotInstalled), []);
        assert_eq!(
            why(&found(seat(-HOUR), Monitor::Paused)),
            Some(Unkept::Paused)
        );
    }

    #[test]
    fn a_healthy_fleet_needs_nothing() {
        let fleet = Fleet {
            accounts: vec![ending("a", 20 * DAY_SECS), setup_account("b")],
            ..Fleet::default()
        };
        let facts = Facts {
            readings: vec![claude("a", 0.2), claude("b", 0.99)],
            monitor: Monitor::Running,
            ..Facts::default()
        };
        assert_eq!(view(&fleet, facts), []);
        // Nothing read yet is nothing known, and no service is not a fault.
        assert_eq!(view(&fleet, Facts::default()), []);
    }

    #[test]
    fn a_login_is_reported_at_three_days_and_again_at_one() {
        let fleet = |secs| Fleet {
            accounts: vec![ending("a", secs)],
            ..Fleet::default()
        };
        assert_eq!(
            view(&fleet(3 * DAY_SECS + HOUR), Facts::default()),
            [],
            "more than three days out"
        );
        let three = view(&fleet(2 * DAY_SECS), Facts::default());
        assert_eq!(
            three,
            [Attention::LoginEnds {
                account: "a".into(),
                ends_at: NOW + 2 * DAY_SECS,
                within: Within::ThreeDays,
            }]
        );
        assert_eq!(three[0].level(), Level::Low);
        let one = view(&fleet(5 * HOUR), Facts::default());
        assert_eq!(keys(&one), ["login-ends:a:1d"]);
        assert_eq!(one[0].level(), Level::Critical);
        assert_ne!(
            three[0].key(),
            one[0].key(),
            "each threshold is its own key"
        );
        // An ended login keeps the nearer threshold's key.
        assert_eq!(
            keys(&view(&fleet(-HOUR), Facts::default())),
            ["login-ends:a:1d"]
        );
    }

    #[test]
    fn a_refused_login_and_an_old_one_without_a_date() {
        let mut refused = ending("refused", DAY_SECS / 2);
        refused.login.as_mut().unwrap().error = Some("invalid_grant".into());
        let mut old = login_account("old", NOW * 1000 + 3_600_000);
        old.login.as_mut().unwrap().logged_in_at = Some((NOW - 25 * DAY_SECS) * 1000);
        let young = login_account("young", NOW * 1000 + 3_600_000);
        let fleet = Fleet {
            accounts: vec![old, refused, young],
            ..Fleet::default()
        };
        let found = view(&fleet, Facts::default());
        // The refusal says everything about that login, and comes first.
        assert_eq!(keys(&found), ["login-refused:refused", "login-ageing:old"]);
    }

    #[test]
    fn a_service_is_below_its_floor_only_when_no_account_read_above_it() {
        let fleet = Fleet {
            accounts: vec![setup_account("a"), setup_account("b")],
            codex: vec![codex_account("work", "~/w"), {
                let mut parked = codex_account("spare", "~/s");
                parked.parked = true;
                parked
            }],
            ..Fleet::default()
        };
        let codex = |name: &str, allowed: bool| Reading {
            detail: Detail::Codex(CodexDetail {
                allowed: Some(allowed),
                ..Default::default()
            }),
            ..Reading::empty(Service::Codex, name, NOW)
        };
        let below = Facts {
            readings: vec![
                claude("a", 0.99),
                claude("b", 0.97),
                codex("work", false),
                // A parked profile takes no work either way.
                codex("spare", true),
            ],
            ..Facts::default()
        };
        assert_eq!(
            keys(&view(&fleet, below)),
            ["below-floor:claude", "below-floor:codex"]
        );
        let one_left = Facts {
            readings: vec![claude("a", 0.99), claude("b", 0.5), codex("work", true)],
            ..Facts::default()
        };
        assert_eq!(view(&fleet, one_left), []);
        // Reads that failed say nothing about room: an outage is not a floor.
        let outage = Facts {
            readings: vec![
                crate::testing::refused("a"),
                crate::testing::refused("b"),
                crate::providers::failed(Service::Codex, "work", NOW, "network down"),
            ],
            ..Facts::default()
        };
        assert_eq!(view(&fleet, outage), []);
        // One account known to be below and the rest unreadable is no room.
        let mixed = Facts {
            readings: vec![claude("a", 0.99), crate::testing::refused("b")],
            ..Facts::default()
        };
        assert_eq!(keys(&view(&fleet, mixed)), ["below-floor:claude"]);
    }

    #[test]
    fn resets_on_offer_are_reported_as_they_near_their_end() {
        let fleet = Fleet {
            accounts: vec![setup_account("a")],
            codex: vec![codex_account("work", "~/w")],
            ..Fleet::default()
        };
        let offer = |expires_at| ResetOffer {
            account: "a".into(),
            offer: "launch reset".into(),
            read_at: NOW - 60,
            expires_at,
            weekly_refills: 1,
        };
        let credits = |available, expires_at| Reading {
            detail: Detail::Codex(CodexDetail {
                allowed: Some(true),
                reset_credits: Some(ResetCreditCount {
                    available,
                    applicable: Some(0),
                    expires_at,
                }),
                ..Default::default()
            }),
            ..Reading::empty(Service::Codex, "work", NOW)
        };
        let found = view(
            &fleet,
            Facts {
                offers: vec![
                    offer(Some(NOW + 2 * DAY_SECS)),
                    offer(Some(NOW + 9 * DAY_SECS)),
                    offer(None),
                    // Already over: nothing is left to use.
                    offer(Some(NOW - HOUR)),
                ],
                readings: vec![credits(2, Some(NOW + 6 * HOUR))],
                ..Facts::default()
            },
        );
        assert_eq!(
            keys(&found),
            [
                format!("reset-expires:codex:work:{}:1d", NOW + 6 * HOUR),
                format!("reset-expires:claude:a:{}:3d", NOW + 2 * DAY_SECS),
            ],
            "the one within a day first"
        );
        // No credit left, or no known end, is nothing to say.
        for reading in [
            credits(0, Some(NOW + HOUR)),
            credits(2, None),
            credits(2, Some(NOW + 30 * DAY_SECS)),
        ] {
            let facts = Facts {
                readings: vec![reading],
                ..Facts::default()
            };
            assert_eq!(view(&fleet, facts), []);
        }
    }

    #[test]
    fn a_fleet_forecast_under_twelve_hours_and_a_stopped_service() {
        let fleet = Fleet {
            accounts: vec![setup_account("a")],
            ..Fleet::default()
        };
        // 4%/h with `left` of the weekly remaining above the sip floor.
        let facts = |left: f64| {
            let floor = fleet.policy.sip_min_7d_remaining;
            let used = 1.0 - floor - left;
            let recent: Vec<Reading> = (0..=6)
                .map(|hour| {
                    let mut reading = claude("a", used - 0.04 * (6 - hour) as f64);
                    reading.at = NOW - (6 - hour) * HOUR;
                    reading
                })
                .collect();
            Facts {
                readings: vec![recent.last().unwrap().clone()],
                recent,
                monitor: Monitor::Stopped,
                ..Facts::default()
            }
        };
        let found = view(&fleet, facts(0.40));
        assert_eq!(keys(&found), ["daemon-stopped", "runs-dry:claude"]);
        let Attention::RunsDry { at, .. } = found[1] else {
            panic!("{found:?}");
        };
        assert!((at - NOW - 10 * HOUR).abs() <= 1, "{}", at - NOW);
        // Thirteen hours of room is not yet the user's problem.
        assert_eq!(keys(&view(&fleet, facts(0.52))), ["daemon-stopped"]);
    }

    #[test]
    fn items_serialize_with_their_kind() {
        let item = Attention::ResetExpires {
            service: Service::Codex,
            account: "work".into(),
            expires_at: 7,
            within: Within::OneDay,
        };
        assert_eq!(
            serde_json::to_value(&item).unwrap(),
            serde_json::json!({
                "kind": "reset-expires", "service": "codex", "account": "work",
                "expires_at": 7, "within": "1d",
            })
        );
        assert_eq!(
            serde_json::to_value(Attention::DaemonStopped).unwrap(),
            serde_json::json!({"kind": "daemon-stopped"})
        );
    }
}
