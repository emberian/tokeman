//! Which account the default should be. Pure: no clock, no files, no network.
//!
//! The default stays until it reaches its own normal floors, and leaves only
//! for an account above the landing floors. With nowhere worth landing it
//! drains in place to the sip floors, then moves to the roomiest account that
//! is above them. Every switch cold-starts the prompt cache of every live
//! session, so switching rarely matters more than switching early.

use serde::{Deserialize, Serialize};
use std::cmp::Ordering;

use crate::quota::{ModelKey, Reading, Window};
use crate::settings::PolicySettings;

/// How the fleet is doing as a whole: `Sip` once no account is above the
/// normal floors, which also makes the daemon sample faster.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Mode {
    Normal,
    #[serde(rename = "sip-and-drain")]
    Sip,
}

impl Mode {
    pub fn label(self) -> &'static str {
        match self {
            Self::Normal => "normal",
            Self::Sip => "sip-and-drain",
        }
    }
}

/// Minimum fractions remaining in the session and weekly windows. Model
/// buckets are weekly windows and use the weekly floor.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Floors {
    pub session: f64,
    pub weekly: f64,
}

impl PolicySettings {
    pub fn floors(&self, mode: Mode) -> Floors {
        match mode {
            Mode::Normal => Floors {
                session: self.normal_min_5h_remaining,
                weekly: self.normal_min_7d_remaining,
            },
            Mode::Sip => Floors {
                session: self.sip_min_5h_remaining,
                weekly: self.sip_min_7d_remaining,
            },
        }
    }

    /// The room a replacement must offer.
    pub fn landing_floors(&self) -> Floors {
        Floors {
            session: self.land_min_5h_remaining,
            weekly: self.land_min_7d_remaining,
        }
    }

    pub fn interval_secs(&self, mode: Mode) -> u64 {
        match mode {
            Mode::Normal => self.normal_probe_interval_secs,
            Mode::Sip => self.sip_probe_interval_secs,
        }
    }

    fn fast_interval_secs(&self) -> u64 {
        self.sip_probe_interval_secs
            .min(self.normal_probe_interval_secs)
    }
}

pub fn above(reading: &Reading, floors: Floors, model: Option<&ModelKey>) -> bool {
    reading.above(floors.session, floors.weekly, model)
}

/// Whether the account can take new sessions on `model` in `mode`.
pub fn viable(
    reading: &Reading,
    mode: Mode,
    policy: &PolicySettings,
    model: Option<&ModelKey>,
) -> bool {
    above(reading, policy.floors(mode), model)
}

/// Every configured account answered with a reading or a definitive refusal.
pub fn complete(readings: &[Reading], expected: usize) -> bool {
    readings.len() == expected
        && readings
            .iter()
            .all(|reading| reading.has_quota() || reading.is_refused())
}

/// Normal while any account is above the normal floors; otherwise sip, even
/// for an exhausted fleet, which is still an emergency worth sampling fast.
pub fn mode_for(readings: &[Reading], policy: &PolicySettings, model: Option<&ModelKey>) -> Mode {
    if readings
        .iter()
        .any(|reading| viable(reading, Mode::Normal, policy, model))
    {
        Mode::Normal
    } else {
        Mode::Sip
    }
}

/// `mode_for`, unless the view is partial and shows no normal candidate: an
/// omitted account may be the roomy one, so the previous mode stands.
pub fn safe_mode_for(
    readings: &[Reading],
    expected: usize,
    policy: &PolicySettings,
    previous: Mode,
    model: Option<&ModelKey>,
) -> Mode {
    let normal_candidate = readings
        .iter()
        .any(|reading| viable(reading, Mode::Normal, policy, model));
    if complete(readings, expected) || normal_candidate {
        mode_for(readings, policy, model)
    } else {
        previous
    }
}

pub fn landing_exists(
    readings: &[Reading],
    policy: &PolicySettings,
    model: Option<&ModelKey>,
) -> bool {
    readings
        .iter()
        .any(|reading| above(reading, policy.landing_floors(), model))
}

/// Whether the default should stay. The fleet's mode plays no part: judging
/// the default by the fleet's floors evicted it the moment another account
/// reset. It stays above its normal floors; with nowhere to land it also
/// stays above its sip floors.
pub fn should_stay(
    current: &Reading,
    landing_exists: bool,
    policy: &PolicySettings,
    model: Option<&ModelKey>,
) -> bool {
    above(current, policy.floors(Mode::Normal), model)
        || (!landing_exists && above(current, policy.floors(Mode::Sip), model))
}

/// Where to go when leaving: the roomiest account above the landing floors,
/// or, failing that, above the sip floors.
pub fn choose_landing<'a>(
    readings: &'a [Reading],
    policy: &PolicySettings,
    model: Option<&ModelKey>,
) -> Option<&'a Reading> {
    choose_above(readings, policy.landing_floors(), model)
        .or_else(|| choose_above(readings, policy.floors(Mode::Sip), model))
}

/// The roomiest account above `floors`: longest balanced dwell first, then
/// the soonest reset of its limiting window, then raw use, then name.
pub fn choose_above<'a>(
    readings: &'a [Reading],
    floors: Floors,
    model: Option<&ModelKey>,
) -> Option<&'a Reading> {
    readings
        .iter()
        .filter(|reading| above(reading, floors, model))
        .min_by(|a, b| {
            compare_f64(dwell_score(b, floors, model), dwell_score(a, floors, model))
                .then_with(|| {
                    compare_delay(reset_delay(a, floors, model), reset_delay(b, floors, model))
                })
                .then_with(|| compare_f64(used(a).0, used(b).0))
                .then_with(|| compare_f64(used(a).1, used(b).1))
                .then_with(|| a.account.cmp(&b.account))
        })
}

fn used(reading: &Reading) -> (f64, f64) {
    reading
        .windows()
        .map_or((1.0, 1.0), |(session, weekly)| (session.used, weekly.used))
}

/// Room above the floor, as a fraction of the room the floor leaves.
fn margin(window: &Window, floor: f64) -> f64 {
    (window.remaining() - floor) / (1.0 - floor)
}

/// The smallest normalized margin across every window that limits `model`.
fn dwell_score(reading: &Reading, floors: Floors, model: Option<&ModelKey>) -> f64 {
    let Some((session, weekly)) = reading.windows() else {
        return f64::NEG_INFINITY;
    };
    reading
        .windows_limiting(model)
        .map(|window| margin(window, floors.weekly))
        .fold(
            margin(&session, floors.session).min(margin(&weekly, floors.weekly)),
            f64::min,
        )
}

/// Seconds until the reset of the window with the least room above its
/// floor. `None` when the server sent no reset for it.
fn reset_delay(reading: &Reading, floors: Floors, model: Option<&ModelKey>) -> Option<i64> {
    let (session, weekly) = reading.windows()?;
    let mut windows = vec![
        (margin(&session, floors.session), session.resets_at),
        (margin(&weekly, floors.weekly), weekly.resets_at),
    ];
    windows.extend(
        reading
            .windows_limiting(model)
            .map(|window| (margin(window, floors.weekly), window.resets_at)),
    );
    windows.sort_by(|(a_margin, a_reset), (b_margin, b_reset)| {
        compare_f64(*a_margin, *b_margin).then_with(|| match (*a_reset > 0, *b_reset > 0) {
            (true, true) => a_reset.cmp(b_reset),
            (true, false) => Ordering::Less,
            (false, true) => Ordering::Greater,
            (false, false) => Ordering::Equal,
        })
    });
    let reset = windows.first().map(|(_, reset)| *reset).unwrap_or(0);
    (reset > 0).then(|| reset.saturating_sub(reading.at))
}

fn compare_delay(a: Option<i64>, b: Option<i64>) -> Ordering {
    match (a, b) {
        (Some(a), Some(b)) => a.cmp(&b),
        (Some(_), None) => Ordering::Less,
        (None, Some(_)) => Ordering::Greater,
        (None, None) => Ordering::Equal,
    }
}

fn compare_f64(a: f64, b: f64) -> Ordering {
    a.partial_cmp(&b).unwrap_or(Ordering::Equal)
}

/// How soon to probe again: fast in sip mode, after a partial probe, when
/// the default's quota is unknown, or when it is within twice a normal floor.
pub fn next_interval_secs(
    mode: Mode,
    current: Option<&Reading>,
    complete: bool,
    policy: &PolicySettings,
) -> u64 {
    let near_floor = |reading: &Reading| {
        reading.remaining().is_none_or(|(session, weekly)| {
            session <= policy.normal_min_5h_remaining * 2.0
                || weekly <= policy.normal_min_7d_remaining * 2.0
        })
    };
    if mode == Mode::Sip
        || !complete
        || current.is_none_or(|reading| !reading.has_quota())
        || current.is_some_and(near_floor)
    {
        policy.fast_interval_secs()
    } else {
        policy.normal_probe_interval_secs
    }
}

/// A managed default whose probe is neither a reading nor a definitive
/// refusal: nothing can be concluded about it this cycle.
pub fn current_unreadable(current_name: Option<&str>, current: Option<&Reading>) -> bool {
    current_name.is_some()
        && current.is_none_or(|reading| !reading.has_quota() && !reading.is_refused())
}

/// Why the default moves.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Trigger {
    /// No managed account was the default.
    NoDefault,
    /// `--force` chose the best account over a healthy default.
    Forced,
    /// The default's credential is refused.
    Refused,
    /// The default reached its sip floors with nowhere better.
    SipFloor,
    /// The default reached its normal floors and a landing exists.
    NormalFloor,
}

/// Why nothing is decided this cycle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Hold {
    /// Not every account answered; an omitted one may be the roomy one.
    PartialProbe,
    /// The default's own quota could not be read.
    DefaultUnreadable,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Decision<'a> {
    Hold(Hold),
    /// The default stays; `healthy` is false when it is draining in place.
    Stay {
        healthy: bool,
    },
    Switch {
        to: &'a Reading,
        trigger: Trigger,
    },
    /// Nothing is above the sip floors.
    NoneViable,
}

pub struct Situation<'a> {
    pub readings: &'a [Reading],
    /// Number of configured accounts.
    pub expected: usize,
    /// The managed default, if any.
    pub current: Option<&'a str>,
    pub model: Option<&'a ModelKey>,
    pub force: bool,
}

impl Situation<'_> {
    pub fn current_reading(&self) -> Option<&Reading> {
        let name = self.current?;
        self.readings.iter().find(|reading| reading.account == name)
    }
}

pub fn decide<'a>(situation: &Situation<'a>, policy: &PolicySettings) -> Decision<'a> {
    let Situation {
        readings,
        expected,
        current,
        model,
        force,
    } = *situation;
    let current_reading = situation.current_reading();
    if !complete(readings, expected) && !force {
        return Decision::Hold(Hold::PartialProbe);
    }
    if current_unreadable(current, current_reading) && !force {
        return Decision::Hold(Hold::DefaultUnreadable);
    }
    let healthy =
        current_reading.is_some_and(|reading| viable(reading, Mode::Normal, policy, model));
    let staying = current_reading.is_some_and(|reading| {
        should_stay(
            reading,
            landing_exists(readings, policy, model),
            policy,
            model,
        )
    });
    if staying && !force {
        return Decision::Stay { healthy };
    }
    let Some(best) = choose_landing(readings, policy, model) else {
        return Decision::NoneViable;
    };
    if current == Some(best.account.as_str()) {
        return Decision::Stay { healthy };
    }
    let trigger = match current_reading {
        None => Trigger::NoDefault,
        Some(_) if healthy && force => Trigger::Forced,
        Some(reading) if reading.is_refused() => Trigger::Refused,
        Some(reading) if !viable(reading, Mode::Sip, policy, model) => Trigger::SipFloor,
        Some(_) => Trigger::NormalFloor,
    };
    Decision::Switch { to: best, trigger }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::quota::{Bucket, ProbeError, Source, Verdict};

    fn key(model: &str) -> ModelKey {
        ModelKey::new(model)
    }

    fn reading(name: &str, session_used: f64, weekly_used: f64) -> Reading {
        with_resets(name, session_used, weekly_used, 0, 0)
    }

    fn with_resets(
        name: &str,
        session_used: f64,
        weekly_used: f64,
        session_reset: i64,
        weekly_reset: i64,
    ) -> Reading {
        Reading {
            account: name.into(),
            at: 0,
            verdict: Verdict::Allowed,
            claim: Some("five_hour".into()),
            session: Some(Window::new(session_used, session_reset)),
            weekly: Some(Window::new(weekly_used, weekly_reset)),
            buckets: Vec::new(),
            overage: None,
            error: None,
            profile_error: None,
        }
    }

    fn with_bucket(name: &str, bucket: &str, label: &str, used: f64) -> Reading {
        let mut r = reading(name, 0.0, 0.0);
        r.buckets.push(Bucket {
            key: key(bucket),
            label: label.into(),
            window: Window::new(used, 0),
            source: Source::Profile,
        });
        r
    }

    fn with_opus(name: &str, session_used: f64, weekly_used: f64, opus_used: f64) -> Reading {
        let mut r = reading(name, session_used, weekly_used);
        r.buckets.push(Bucket {
            key: key("opus"),
            label: "Opus".into(),
            window: Window::new(opus_used, 0),
            source: Source::Profile,
        });
        r
    }

    fn opus() -> Option<ModelKey> {
        Some(key("opus"))
    }

    fn best<'a>(readings: &'a [Reading], mode: Mode, model: Option<&ModelKey>) -> Option<&'a str> {
        choose_above(readings, PolicySettings::default().floors(mode), model)
            .map(|r| r.account.as_str())
    }

    fn stays(current: &Reading, fleet: &[Reading]) -> bool {
        let policy = PolicySettings::default();
        should_stay(
            current,
            landing_exists(fleet, &policy, opus().as_ref()),
            &policy,
            opus().as_ref(),
        )
    }

    #[test]
    fn exact_normal_floor_is_not_viable() {
        let policy = PolicySettings::default();
        let viable = |r: &Reading| super::viable(r, Mode::Normal, &policy, opus().as_ref());
        assert!(!viable(&reading(
            "at-floor",
            1.0 - policy.normal_min_5h_remaining,
            0.0
        )));
        assert!(!viable(&reading(
            "weekly-floor",
            0.0,
            1.0 - policy.normal_min_7d_remaining
        )));
    }

    #[test]
    fn known_opus_floor_is_part_of_shared_default_viability() {
        let policy = PolicySettings::default();
        let floor = policy.normal_min_7d_remaining;
        let viable = |r: &Reading| super::viable(r, Mode::Normal, &policy, opus().as_ref());
        assert!(!viable(&with_opus("opus-wall", 0.0, 0.0, 1.0 - floor)));
        assert!(viable(&with_opus(
            "opus-room",
            0.0,
            0.0,
            1.0 - floor - 0.01
        )));
    }

    #[test]
    fn exhausted_opus_48_bucket_does_not_poison_opus_5_or_fable() {
        let policy = PolicySettings::default();
        let r = with_bucket("separate-models", "opus48", "Opus 4.8", 1.0);
        let viable = |model: &str| super::viable(&r, Mode::Normal, &policy, Some(&key(model)));
        assert!(viable("opus"));
        assert!(!viable("claude-opus-4-8"));
        assert!(viable("claude-fable-5-1"));
    }

    #[test]
    fn the_fable_limit_gates_fable_sessions_only() {
        let policy = PolicySettings::default();
        let r = with_bucket("profile", "fable", "Fable", 1.0);
        let viable = |model: &str| super::viable(&r, Mode::Normal, &policy, Some(&key(model)));
        assert!(!viable("claude-fable-5-1"));
        assert!(!viable("fable"));
        assert!(viable("opus"));
    }

    #[test]
    fn the_opus_alias_obeys_the_opus_5_bucket() {
        let policy = PolicySettings::default();
        let r = with_bucket("opus-five", "claudeopus5", "Opus 5", 0.95);
        assert!(!viable(&r, Mode::Normal, &policy, Some(&key("opus[1m]"))));
    }

    #[test]
    fn unrelated_family_bucket_does_not_affect_selection() {
        let policy = PolicySettings::default();
        let mut r = with_opus("family-split", 0.0, 0.0, 0.10);
        r.buckets.push(Bucket {
            key: key("sonnet"),
            label: "Sonnet".into(),
            window: Window::new(1.0, 0),
            source: Source::Profile,
        });
        assert!(viable(&r, Mode::Normal, &policy, Some(&key("opus"))));
        assert!(!viable(&r, Mode::Normal, &policy, Some(&key("sonnet"))));
    }

    #[test]
    fn replacement_landing_score_includes_known_opus_capacity() {
        let readings = [
            with_opus("general-room-opus-tight", 0.0, 0.0, 0.80),
            with_opus("balanced-for-opus", 0.10, 0.10, 0.10),
        ];
        assert_eq!(
            best(&readings, Mode::Normal, opus().as_ref()),
            Some("balanced-for-opus")
        );
    }

    #[test]
    fn sip_mode_unlocks_the_reserve_band() {
        let policy = PolicySettings::default();
        let readings = [
            reading("five-hour-scrap", 0.95, 0.96),
            reading("weekly-scrap", 0.97, 0.96),
        ];
        assert_eq!(mode_for(&readings, &policy, opus().as_ref()), Mode::Sip);
        assert_eq!(
            best(&readings, Mode::Sip, opus().as_ref()),
            Some("five-hour-scrap")
        );
    }

    #[test]
    fn exhausted_fleet_stays_in_fast_sip_mode() {
        let policy = PolicySettings::default();
        let readings = [
            reading("weekly-empty", 0.20, 1.0),
            reading("five-hour-empty", 1.0, 0.20),
        ];
        assert_eq!(mode_for(&readings, &policy, opus().as_ref()), Mode::Sip);
        assert_eq!(best(&readings, Mode::Sip, opus().as_ref()), None);
        assert_eq!(
            policy.interval_secs(Mode::Sip),
            policy.sip_probe_interval_secs
        );
    }

    #[test]
    fn sip_floors_are_hard_boundaries() {
        let policy = PolicySettings::default();
        let viable = |r: &Reading| super::viable(r, Mode::Sip, &policy, opus().as_ref());
        assert!(!viable(&reading("five-hour-floor", 0.98, 0.50)));
        assert!(!viable(&reading("weekly-floor", 0.50, 0.97)));
        assert!(viable(&reading("scraps", 0.979, 0.969)));
    }

    #[test]
    fn replacement_prefers_long_balanced_dwell() {
        let readings = [
            reading("short-five", 0.70, 0.15),
            reading("balanced", 0.14, 0.14),
            reading("short-weekly", 0.05, 0.77),
        ];
        assert_eq!(
            best(&readings, Mode::Normal, opus().as_ref()),
            Some("balanced")
        );
    }

    #[test]
    fn replacement_uses_soonest_limiting_reset_as_a_headroom_tiebreaker() {
        let readings = [
            with_resets("same-dwell-later", 0.10, 0.20, 900, 3_600),
            with_resets("same-dwell-first", 0.10, 0.20, 900, 600),
        ];
        assert_eq!(
            best(&readings, Mode::Normal, opus().as_ref()),
            Some("same-dwell-first")
        );
    }

    #[test]
    fn replacement_headroom_beats_an_earlier_reset() {
        let readings = [
            with_resets("roomy", 0.08, 0.01, 3_600, 86_400),
            with_resets("soon-but-tight", 0.06, 0.54, 600, 600),
        ];
        assert_eq!(
            best(&readings, Mode::Normal, opus().as_ref()),
            Some("roomy")
        );
    }

    #[test]
    fn replacement_uses_the_limiting_windows_reset() {
        let readings = [
            with_resets("non-limiting-reset", 0.00, 0.50, 60, 7_200),
            with_resets("limiting-reset", 0.40, 0.10, 600, 9_000),
        ];
        assert_eq!(
            best(&readings, Mode::Normal, opus().as_ref()),
            Some("limiting-reset")
        );
    }

    #[test]
    fn the_default_stays_until_its_own_floor_however_roomy_the_rest() {
        let fresh = [reading("fresh", 0.0, 0.0)];
        assert!(stays(&reading("current", 0.92, 0.50), &fresh));
        assert!(stays(&reading("current", 0.20, 0.94), &fresh));
        assert!(!stays(&reading("current", 0.93, 0.50), &fresh));
        assert!(!stays(&reading("current", 0.20, 0.95), &fresh));
    }

    #[test]
    fn below_its_floor_the_default_drains_in_place_rather_than_land_on_scraps() {
        let scraps = [
            reading("boot", 0.90, 0.24),
            reading("boto", 0.90, 0.24),
            reading("tobo", 0.00, 0.94),
        ];
        assert!(stays(&reading("otto", 0.95, 0.87), &scraps));
        assert!(!stays(&reading("otto", 0.98, 0.87), &scraps));
    }

    #[test]
    fn landing_prefers_real_room_and_falls_back_to_the_roomiest_scrap() {
        let policy = PolicySettings::default();
        let land = |readings: &[Reading]| {
            choose_landing(readings, &policy, opus().as_ref()).map(|r| r.account.clone())
        };
        assert_eq!(
            land(&[reading("tobo", 0.00, 0.95), reading("otto", 0.50, 0.30)]),
            Some("otto".into())
        );
        assert_eq!(
            land(&[reading("tobo", 0.00, 0.95), reading("boto", 0.97, 0.26)]),
            Some("tobo".into())
        );
        assert_eq!(land(&[reading("dead", 0.99, 0.99)]), None);
    }

    #[test]
    fn a_model_bucket_is_a_weekly_floor_like_any_other() {
        let policy = PolicySettings::default();
        let fable = key("claude-fable-5-1");
        let fresh = [reading("otto", 0.0, 0.27)];
        let landing = landing_exists(&fresh, &policy, Some(&fable));
        let stays = |used| {
            should_stay(
                &with_bucket("boto", "fable", "Fable", used),
                landing,
                &policy,
                Some(&fable),
            )
        };
        assert!(stays(0.91));
        assert!(!stays(0.95));
    }

    #[test]
    fn cadence_is_fast_when_degraded_or_near_a_floor_and_normal_otherwise() {
        let policy = PolicySettings::default();
        let fast = policy.sip_probe_interval_secs;
        assert_eq!(
            next_interval_secs(
                Mode::Normal,
                Some(&reading("a", 0.10, 0.10)),
                false,
                &policy
            ),
            fast
        );
        assert_eq!(
            next_interval_secs(Mode::Normal, Some(&reading("a", 0.88, 0.20)), true, &policy),
            fast
        );
        assert_eq!(
            next_interval_secs(Mode::Normal, Some(&reading("a", 0.20, 0.20)), true, &policy),
            policy.normal_probe_interval_secs
        );
    }

    #[test]
    fn a_failed_probe_is_not_a_reading_but_a_refusal_is_definitive() {
        let failed = Reading::failed(
            "failed",
            0,
            ProbeError::Network {
                message: "transient".into(),
            },
        );
        assert!(!failed.has_quota());
        assert!(!complete(std::slice::from_ref(&failed), 1));
        let expired = Reading::failed(
            "expired",
            0,
            ProbeError::Http {
                status: 401,
                body: "expired".into(),
            },
        );
        assert!(expired.is_refused());
        assert!(complete(std::slice::from_ref(&expired), 1));
        assert!(!current_unreadable(Some("expired"), Some(&expired)));
        assert!(!viable(
            &expired,
            Mode::Normal,
            &PolicySettings::default(),
            opus().as_ref()
        ));
    }

    #[test]
    fn partial_fleet_view_cannot_enter_sip_and_drain() {
        let policy = PolicySettings::default();
        let partial = [reading("only-response", 0.95, 0.96)];
        assert_eq!(
            safe_mode_for(&partial, 2, &policy, Mode::Normal, opus().as_ref()),
            Mode::Normal
        );
        assert_eq!(
            safe_mode_for(&partial, 2, &policy, Mode::Sip, opus().as_ref()),
            Mode::Sip
        );
    }

    #[test]
    fn decisions_follow_the_floors() {
        let policy = PolicySettings::default();
        let model = opus();
        let situation = |readings: &'static [Reading], current, force| Situation {
            readings,
            expected: readings.len(),
            current,
            model: model.as_ref(),
            force,
        };
        let fleet: &'static [Reading] = Box::leak(Box::new([
            reading("cur", 0.95, 0.50),
            reading("fresh", 0.0, 0.0),
        ]));
        assert_eq!(
            decide(&situation(fleet, Some("cur"), false), &policy),
            Decision::Switch {
                to: &fleet[1],
                trigger: Trigger::NormalFloor
            }
        );
        assert_eq!(
            decide(&situation(fleet, None, false), &policy),
            Decision::Switch {
                to: &fleet[1],
                trigger: Trigger::NoDefault
            }
        );
        assert_eq!(
            decide(&situation(fleet, Some("fresh"), false), &policy),
            Decision::Stay { healthy: true }
        );
        let partial = Situation {
            readings: &fleet[..1],
            expected: 2,
            current: Some("cur"),
            model: model.as_ref(),
            force: false,
        };
        assert_eq!(
            decide(&partial, &policy),
            Decision::Hold(Hold::PartialProbe)
        );
        let dead: &'static [Reading] = Box::leak(Box::new([reading("cur", 0.99, 0.50)]));
        assert_eq!(
            decide(&situation(dead, Some("cur"), false), &policy),
            Decision::NoneViable
        );
        let draining: &'static [Reading] = Box::leak(Box::new([
            reading("cur", 0.95, 0.50),
            reading("scrap", 0.90, 0.24),
        ]));
        assert_eq!(
            decide(&situation(draining, Some("cur"), false), &policy),
            Decision::Stay { healthy: false }
        );
    }
}
