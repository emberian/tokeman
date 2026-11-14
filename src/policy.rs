//! Which account the default should be. Pure: no clock, no files, no network.
//!
//! The default stays until it reaches its own normal floors, and leaves only
//! for an account above the landing floors. With nowhere worth landing it
//! drains in place to the sip floors, then moves to an account that is above
//! them. Every switch cold-starts the prompt cache of every live session, so
//! switching rarely matters more than switching early.
//!
//! Where a switch lands spends expiring room first: weekly quota left at its
//! reset is lost, so a candidate whose weekly window resets within
//! [`EXPIRING_WITHIN_SECS`] is preferred, soonest first. Beyond that horizon
//! the longest dwell lands, because room days from its reset can still be
//! spent after a later switch. That changes where a switch lands, never
//! whether one happens.

use serde::{Deserialize, Serialize};
use std::cmp::Ordering;

use crate::quota::{ModelKey, Reading, Span, Window};
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

/// Minimum fractions remaining in the session and weekly windows. A model's
/// limit is a weekly window and uses the weekly floor, as does an
/// account-wide window of any length but five hours.
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

    /// Where a running Codex leaves its account.
    pub fn codex_floors(&self) -> Floors {
        Floors {
            session: self.codex_min_5h_remaining,
            weekly: self.codex_min_7d_remaining,
        }
    }

    /// The room an account must offer a running Codex that moves to it.
    pub fn codex_landing_floors(&self) -> Floors {
        Floors {
            session: self.codex_land_min_5h_remaining,
            weekly: self.codex_land_min_7d_remaining,
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

/// The account above `floors` to land on: a weekly reset within
/// [`EXPIRING_WITHIN_SECS`] first (soonest first), then the longest balanced dwell, then the
/// soonest reset of its limiting window, then raw use, then name. An account
/// without a 5h window is ranked on its weekly room alone.
pub fn choose_above<'a>(
    readings: &'a [Reading],
    floors: Floors,
    model: Option<&ModelKey>,
) -> Option<&'a Reading> {
    readings
        .iter()
        .filter(|reading| above(reading, floors, model))
        .min_by(|a, b| landing_order(a, b, floors, model))
}

/// Every account above `floors`, in the order [`choose_above`] prefers them.
pub fn rank_above<'a>(
    readings: &'a [Reading],
    floors: Floors,
    model: Option<&ModelKey>,
) -> Vec<&'a Reading> {
    let mut ranked: Vec<&Reading> = readings
        .iter()
        .filter(|reading| above(reading, floors, model))
        .collect();
    ranked.sort_by(|a, b| landing_order(a, b, floors, model));
    ranked
}

/// What work running on one account should do, for a tool that can be moved
/// between turns at no cost to the others.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Relocation<'a> {
    /// The seat is above its floors.
    Stay,
    /// Below its floors, and nowhere offers the landing room: it drains in
    /// place.
    Drain,
    Move(&'a Reading),
}

/// Whether work seated on `seat` should move to one of `others`. It leaves
/// at `leave` for the best account above `land` ([`choose_above`]); a seat
/// that takes no work at all leaves for any account that does.
pub fn relocate<'a>(
    seat: &Reading,
    others: &'a [Reading],
    leave: Floors,
    land: Floors,
) -> Relocation<'a> {
    const ANY_ROOM: Floors = Floors {
        session: 0.0,
        weekly: 0.0,
    };
    if above(seat, leave, None) {
        return Relocation::Stay;
    }
    let landing = choose_above(others, land, None).or_else(|| {
        (!above(seat, ANY_ROOM, None))
            .then(|| choose_above(others, ANY_ROOM, None))
            .flatten()
    });
    landing.map_or(Relocation::Drain, Relocation::Move)
}

/// The better landing first.
fn landing_order(a: &Reading, b: &Reading, floors: Floors, model: Option<&ModelKey>) -> Ordering {
    compare_reset(expiring_reset(a), expiring_reset(b))
        .then_with(|| compare_f64(dwell_score(b, floors, model), dwell_score(a, floors, model)))
        .then_with(|| compare_reset(reset_delay(a, floors, model), reset_delay(b, floors, model)))
        .then_with(|| compare_f64(used(a).0, used(b).0))
        .then_with(|| compare_f64(used(a).1, used(b).1))
        .then_with(|| a.account.cmp(&b.account))
}

/// How close a weekly reset must be, measured from the reading, for the room
/// before it to count as expiring. Room further out can still be spent after
/// a later switch, so it does not justify landing on a tighter account.
pub const EXPIRING_WITHIN_SECS: i64 = 24 * 3600;

/// When the weekly window resets, epoch seconds, if that is within
/// [`EXPIRING_WITHIN_SECS`] of the reading; `None` when further out or
/// unreported.
fn expiring_reset(reading: &Reading) -> Option<i64> {
    reading
        .weekly()
        .map(|window| window.resets_at)
        .filter(|&reset| reset > 0 && reset - reading.at <= EXPIRING_WITHIN_SECS)
}

/// Sooner first; unknown last.
fn compare_reset(a: Option<i64>, b: Option<i64>) -> Ordering {
    match (a, b) {
        (Some(a), Some(b)) => a.cmp(&b),
        (Some(_), None) => Ordering::Less,
        (None, Some(_)) => Ordering::Greater,
        (None, None) => Ordering::Equal,
    }
}

/// Use of the shared windows; an unreported window has none.
fn used(reading: &Reading) -> (f64, f64) {
    (
        reading.session().map_or(0.0, |w| w.used),
        reading.weekly().map_or(0.0, |w| w.used),
    )
}

/// Every reported window with its floor: the account-wide ones (the session
/// floor for a 5h window, the weekly floor for any other), then the limits
/// on `model`.
fn limiting_windows<'a>(
    reading: &'a Reading,
    floors: Floors,
    model: Option<&'a ModelKey>,
) -> impl Iterator<Item = (f64, i64)> + 'a {
    let weekly = reading.weekly();
    reading
        .shared()
        .map(move |limit| {
            let floor = match limit.span {
                Span::FiveHour => floors.session,
                _ => floors.weekly,
            };
            (margin(&limit.window, floor), limit.window.resets_at)
        })
        .chain(reading.limits_for(model).map(move |limit| {
            (
                room_margin(limit.room(weekly.as_ref()), floors.weekly),
                limit.window.resets_at,
            )
        }))
}

/// Room above the floor, as a fraction of the room the floor leaves.
fn margin(window: &Window, floor: f64) -> f64 {
    room_margin(window.remaining(), floor)
}

fn room_margin(remaining: f64, floor: f64) -> f64 {
    (remaining - floor) / (1.0 - floor)
}

/// The smallest normalized margin across every window that limits `model`.
fn dwell_score(reading: &Reading, floors: Floors, model: Option<&ModelKey>) -> f64 {
    if !reading.has_quota() {
        return f64::NEG_INFINITY;
    }
    limiting_windows(reading, floors, model)
        .map(|(margin, _)| margin)
        .fold(f64::INFINITY, f64::min)
}

/// Seconds until the reset of the window with the least room above its
/// floor. `None` when the server sent no reset for it.
fn reset_delay(reading: &Reading, floors: Floors, model: Option<&ModelKey>) -> Option<i64> {
    if !reading.has_quota() {
        return None;
    }
    let mut windows: Vec<(f64, i64)> = limiting_windows(reading, floors, model).collect();
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
    model: Option<&ModelKey>,
) -> u64 {
    let near_floor = |reading: &Reading| {
        !reading.above(
            policy.normal_min_5h_remaining * 2.0,
            policy.normal_min_7d_remaining * 2.0,
            model,
        )
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
    use crate::quota::{ReadError, Service, SessionLimit, Source};
    use crate::testing::{model_limit, reading, reading_with_model, reading_with_resets};

    fn key(model: &str) -> ModelKey {
        ModelKey::new(model)
    }

    fn opus() -> Option<ModelKey> {
        Some(key("opus"))
    }

    fn with_opus(name: &str, session_used: f64, weekly_used: f64, opus_used: f64) -> Reading {
        reading_with_model(name, session_used, weekly_used, "opus", "Opus", opus_used)
    }

    /// Viability under the default floors for an opus session.
    fn viable_in(r: &Reading, mode: Mode) -> bool {
        viable(r, mode, &PolicySettings::default(), opus().as_ref())
    }

    /// Viability under the default normal floors for a session on `model`.
    fn viable_for(r: &Reading, model: &str) -> bool {
        viable(
            r,
            Mode::Normal,
            &PolicySettings::default(),
            Some(&key(model)),
        )
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
        let at_floor = reading("at-floor", 1.0 - policy.normal_min_5h_remaining, 0.0);
        let weekly_floor = reading("weekly-floor", 0.0, 1.0 - policy.normal_min_7d_remaining);
        assert!(!viable_in(&at_floor, Mode::Normal));
        assert!(!viable_in(&weekly_floor, Mode::Normal));
    }

    #[test]
    fn known_opus_floor_is_part_of_shared_default_viability() {
        let floor = PolicySettings::default().normal_min_7d_remaining;
        let wall = with_opus("opus-wall", 0.0, 0.0, 1.0 - floor);
        let room = with_opus("opus-room", 0.0, 0.0, 1.0 - floor - 0.01);
        assert!(!viable_in(&wall, Mode::Normal));
        assert!(viable_in(&room, Mode::Normal));
    }

    #[test]
    fn exhausted_opus_48_bucket_does_not_poison_opus_5_or_fable() {
        let r = reading_with_model("separate-models", 0.0, 0.0, "opus48", "Opus 4.8", 1.0);
        assert!(viable_for(&r, "opus"));
        assert!(!viable_for(&r, "claude-opus-4-8"));
        assert!(viable_for(&r, "claude-fable-5-1"));
    }

    #[test]
    fn the_fable_limit_gates_fable_sessions_only() {
        let r = reading_with_model("profile", 0.0, 0.0, "fable", "Fable", 1.0);
        assert!(!viable_for(&r, "claude-fable-5-1"));
        assert!(!viable_for(&r, "fable"));
        assert!(viable_for(&r, "opus"));
    }

    #[test]
    fn the_opus_alias_obeys_the_opus_5_bucket() {
        let r = reading_with_model("opus-five", 0.0, 0.0, "claudeopus5", "Opus 5", 0.95);
        assert!(!viable_for(&r, "opus[1m]"));
    }

    #[test]
    fn unrelated_family_bucket_does_not_affect_selection() {
        let mut r = with_opus("family-split", 0.0, 0.0, 0.10);
        r.limits
            .push(model_limit("sonnet", "Sonnet", 1.0, Source::Profile));
        assert!(viable_for(&r, "opus"));
        assert!(!viable_for(&r, "sonnet"));
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
        // Sip mode samples fast even when the default itself has room.
        let roomy = reading("default", 0.20, 0.20);
        assert_eq!(
            next_interval_secs(Mode::Sip, Some(&roomy), true, &policy, None),
            policy.sip_probe_interval_secs
        );
        assert_eq!(
            next_interval_secs(Mode::Normal, Some(&roomy), true, &policy, None),
            policy.normal_probe_interval_secs
        );
    }

    #[test]
    fn sip_floors_are_hard_boundaries() {
        assert!(!viable_in(
            &reading("five-hour-floor", 0.98, 0.50),
            Mode::Sip
        ));
        assert!(!viable_in(&reading("weekly-floor", 0.50, 0.97), Mode::Sip));
        assert!(viable_in(&reading("scraps", 0.979, 0.969), Mode::Sip));
    }

    #[test]
    fn replacement_ranks_by_dwell_then_headroom_then_the_limiting_reset() {
        let cases: [(&str, Vec<Reading>, &str); 7] = [
            (
                "the landing score includes known opus capacity",
                vec![
                    with_opus("general-room-opus-tight", 0.0, 0.0, 0.80),
                    with_opus("balanced-for-opus", 0.10, 0.10, 0.10),
                ],
                "balanced-for-opus",
            ),
            (
                "a long balanced dwell beats a short one",
                vec![
                    reading("short-five", 0.70, 0.15),
                    reading("balanced", 0.14, 0.14),
                    reading("short-weekly", 0.05, 0.77),
                ],
                "balanced",
            ),
            (
                "the soonest limiting reset breaks a headroom tie",
                // The 5h window is the tighter one of both; neither weekly
                // expires within a day.
                vec![
                    reading_with_resets("a-same-dwell-later", 0.60, 0.20, 3_600, 5 * 86_400),
                    reading_with_resets("same-dwell-first", 0.60, 0.20, 600, 5 * 86_400),
                ],
                "same-dwell-first",
            ),
            (
                "the sooner weekly reset beats more room",
                vec![
                    reading_with_resets("roomy", 0.08, 0.01, 3_600, 86_400),
                    reading_with_resets("soon-but-tight", 0.06, 0.54, 600, 600),
                ],
                "soon-but-tight",
            ),
            (
                "only the limiting window's reset counts",
                // The roomy weekly of the first resets sooner; its tight 5h later.
                vec![
                    reading_with_resets("a-non-limiting-reset", 0.60, 0.20, 3_600, 5 * 86_400),
                    reading_with_resets("limiting-reset", 0.60, 0.20, 600, 6 * 86_400),
                ],
                "limiting-reset",
            ),
            (
                "equal dwell and reset: the less used 5h window",
                vec![
                    reading("a-more-used", 0.30, 0.50),
                    reading("z-less-used", 0.10, 0.50),
                ],
                "z-less-used",
            ),
            (
                "nothing to tell them apart: by name, whatever the order",
                vec![reading("b", 0.10, 0.50), reading("a", 0.10, 0.50)],
                "a",
            ),
        ];
        for (why, readings, want) in cases {
            assert_eq!(
                best(&readings, Mode::Normal, opus().as_ref()),
                Some(want),
                "{why}"
            );
        }
    }

    #[test]
    fn fable_landing_ranks_on_fable_room_in_weekly_terms() {
        let fable = key("claude-fable-5-2");
        let with_fable = |name: &str, weekly_used: f64, fable_used: f64| {
            let mut r = reading_with_model(name, 0.0, weekly_used, "fable", "Fable", fable_used);
            r.set_session(None);
            r.limits[1].share = Some(0.5);
            r
        };
        // a: Fable 80% of half the weekly = 40%, capped by the 30% weekly left.
        // b: Fable 50% of half = 25%, the weekly has 90%. Unshared, b would
        // look roomier (50% against 30%).
        let fleet = [with_fable("a", 0.70, 0.20), with_fable("b", 0.10, 0.50)];
        assert_eq!(best(&fleet, Mode::Normal, Some(&fable)), Some("a"));
        // For any other model only the weekly counts.
        assert_eq!(best(&fleet, Mode::Normal, opus().as_ref()), Some("b"));
    }

    #[test]
    fn landing_spends_expiring_weekly_room_first_and_otherwise_the_longest_dwell() {
        let policy = PolicySettings::default();
        let land = |readings: &[Reading]| {
            choose_landing(readings, &policy, opus().as_ref()).map(|r| r.account.clone())
        };
        // Weekly quota left at its reset is lost: spend the one resetting soonest.
        assert_eq!(
            land(&[
                reading_with_resets("fresh-next-week", 0.0, 0.0, 0, 6 * 86_400),
                reading_with_resets("resets-in-an-hour", 0.2, 0.6, 0, 3_600),
            ]),
            Some("resets-in-an-hour".into())
        );
        // Same reset: the roomier one.
        assert_eq!(
            land(&[
                reading_with_resets("tight", 0.2, 0.6, 0, 3_600),
                reading_with_resets("roomy", 0.2, 0.3, 0, 3_600),
            ]),
            Some("roomy".into())
        );
        // An unreported weekly reset ranks after a known one.
        // Neither weekly expires within a day: the longest dwell lands.
        assert_eq!(
            land(&[
                reading_with_resets("unknown-reset", 0.0, 0.0, 0, 0),
                reading_with_resets("known-reset", 0.3, 0.5, 0, 6 * 86_400),
            ]),
            Some("unknown-reset".into())
        );
        // A tight account days from its reset is not worth a switch that
        // ends soon after; its room can still be spent later.
        assert_eq!(
            land(&[
                reading_with_resets("tight-in-five-days", 0.0, 0.85, 0, 5 * 86_400),
                reading_with_resets("fresh-in-six-days", 0.0, 0.0, 0, 6 * 86_400),
            ]),
            Some("fresh-in-six-days".into())
        );
        // The horizon is measured from the reading, not from epoch zero.
        let mut late = reading_with_resets("expiring-later-read", 0.0, 0.6, 0, 0);
        late.at = 10 * 86_400;
        late.set_weekly(Some(Window::new(0.6, late.at + 3_600)));
        let mut fresh = reading_with_resets("fresh-read-late", 0.0, 0.0, 0, 0);
        fresh.at = late.at;
        fresh.set_weekly(Some(Window::new(0.0, late.at + 6 * 86_400)));
        assert_eq!(land(&[fresh, late]), Some("expiring-later-read".into()));
        // An unreported reset is not an expiring one.
        assert_eq!(
            land(&[
                reading_with_resets("unknown-tight", 0.0, 0.6, 0, 0),
                reading_with_resets("known-roomy", 0.0, 0.1, 0, 6 * 86_400),
            ]),
            Some("known-roomy".into())
        );
        // Expiring room below the landing floors does not beat real room.
        assert_eq!(
            land(&[
                reading_with_resets("scrap-expiring", 0.0, 0.93, 0, 3_600),
                reading_with_resets("roomy-next-week", 0.0, 0.2, 0, 6 * 86_400),
            ]),
            Some("roomy-next-week".into())
        );
        // Below the landing floors it still falls back to the sip floors.
        assert_eq!(
            land(&[
                reading_with_resets("scrap-late", 0.0, 0.94, 0, 6 * 86_400),
                reading_with_resets("scrap-soon", 0.0, 0.95, 0, 3_600),
            ]),
            Some("scrap-soon".into())
        );
    }

    #[test]
    fn a_weekly_only_account_is_ranked_on_its_weekly_room_alone() {
        let mut ultra = reading_with_resets("ultra", 0.0, 0.5, 0, 3_600);
        ultra.set_session(None);
        ultra.claude_mut().session_limit = SessionLimit::Absent;
        let pro = reading_with_resets("pro", 0.85, 0.5, 600, 3_600);
        assert_eq!(
            best(&[pro.clone(), ultra.clone()], Mode::Normal, opus().as_ref()),
            Some("ultra")
        );
        // Its weekly room is its only gate.
        ultra.set_weekly(Some(Window::new(0.96, 3_600)));
        assert!(!viable_in(&ultra, Mode::Normal));
    }

    #[test]
    fn weekly_first_moves_where_a_switch_lands_never_whether_it_happens() {
        let policy = PolicySettings::default();
        let model = opus();
        let fleet: &'static [Reading] = Box::leak(Box::new([
            reading_with_resets("cur", 0.30, 0.50, 0, 6 * 86_400),
            reading_with_resets("soon", 0.10, 0.20, 0, 3_600),
            reading_with_resets("later", 0.0, 0.0, 0, 5 * 86_400),
        ]));
        let situation = |current| Situation {
            readings: fleet,
            expected: fleet.len(),
            current,
            model: model.as_ref(),
            force: false,
        };
        // A healthy default stays, however soon another account resets.
        assert_eq!(
            decide(&situation(Some("cur")), &policy),
            Decision::Stay { healthy: true }
        );
        // With no default, the switch lands on the room expiring within a day.
        assert_eq!(
            decide(&situation(None), &policy),
            Decision::Switch {
                to: &fleet[1],
                trigger: Trigger::NoDefault
            }
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
            let current = reading_with_model("boto", 0.0, 0.0, "fable", "Fable", used);
            should_stay(&current, landing, &policy, Some(&fable))
        };
        assert!(stays(0.91));
        assert!(!stays(0.95));
    }

    #[test]
    fn cadence_is_fast_when_degraded_or_near_a_floor_and_normal_otherwise() {
        let policy = PolicySettings::default();
        let fast = policy.sip_probe_interval_secs;
        let interval = |session, weekly, healthy| {
            let current = reading("a", session, weekly);
            next_interval_secs(Mode::Normal, Some(&current), healthy, &policy, None)
        };
        assert_eq!(interval(0.10, 0.10, false), fast);
        assert_eq!(interval(0.88, 0.20, true), fast);
        assert_eq!(interval(0.20, 0.91, true), fast, "twice the weekly floor");
        assert_eq!(
            next_interval_secs(Mode::Normal, None, true, &policy, None),
            fast
        );
        let unread = crate::testing::refused("a");
        assert_eq!(
            next_interval_secs(Mode::Normal, Some(&unread), true, &policy, None),
            fast
        );
        assert_eq!(
            interval(0.20, 0.20, true),
            policy.normal_probe_interval_secs
        );
    }

    #[test]
    fn fable_room_drives_cadence_and_rotation_even_with_a_roomy_weekly() {
        let policy = PolicySettings {
            normal_min_7d_remaining: 0.02,
            sip_min_7d_remaining: 0.005,
            ..PolicySettings::default()
        };
        let fable = key("claude-fable-5-2");
        let account = |name, used| {
            let mut r = reading_with_model(name, 0.1, 0.4, "fable", "Fable", used);
            r.limits
                .iter_mut()
                .find(|l| l.key().is_some())
                .unwrap()
                .share = Some(0.5);
            r
        };
        // Six percent of Fable is three percent of the shared weekly:
        // above the leave floor, but already close enough to poll quickly.
        let nearing = account("current", 0.94);
        assert_eq!(
            next_interval_secs(Mode::Normal, Some(&nearing), true, &policy, Some(&fable)),
            policy.sip_probe_interval_secs
        );
        assert_eq!(
            next_interval_secs(Mode::Normal, Some(&nearing), true, &policy, opus().as_ref()),
            policy.normal_probe_interval_secs
        );
        let fleet = [account("current", 0.98), account("fresh", 0.1)];
        assert_eq!(
            decide(
                &Situation {
                    readings: &fleet,
                    expected: fleet.len(),
                    current: Some("current"),
                    model: Some(&fable),
                    force: false,
                },
                &policy
            ),
            Decision::Switch {
                to: &fleet[1],
                trigger: Trigger::NormalFloor
            }
        );
    }

    #[test]
    fn a_failed_probe_is_not_a_reading_but_a_refusal_is_definitive() {
        let failed = Reading::failed(
            Service::Claude,
            "failed",
            0,
            ReadError::Network {
                message: "transient".into(),
            },
        );
        assert!(!failed.has_quota());
        assert!(!complete(std::slice::from_ref(&failed), 1));
        let expired = Reading::failed(
            Service::Claude,
            "expired",
            0,
            ReadError::Http {
                status: 401,
                body: "expired".into(),
            },
        );
        assert!(expired.is_refused());
        assert!(complete(std::slice::from_ref(&expired), 1));
        assert!(!current_unreadable(Some("expired"), Some(&expired)));
        assert!(!viable_in(&expired, Mode::Normal));
    }

    #[test]
    fn an_account_reporting_only_its_weekly_window_is_a_complete_reading() {
        let policy = PolicySettings::default();
        let mut idle = reading("idle", 0.0, 0.26);
        idle.set_session(None);
        assert!(complete(std::slice::from_ref(&idle), 1));
        assert!(viable(&idle, Mode::Normal, &policy, opus().as_ref()));
        assert_eq!(
            best(
                &[idle.clone(), reading("busy", 0.5, 0.5)],
                Mode::Normal,
                opus().as_ref()
            ),
            Some("idle")
        );
        assert_eq!(
            next_interval_secs(Mode::Normal, Some(&idle), true, &policy, None),
            policy.normal_probe_interval_secs
        );
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
        // One roomy account is enough to know the fleet is not sipping.
        let roomy = [reading("only-response", 0.10, 0.10)];
        assert_eq!(
            safe_mode_for(&roomy, 2, &policy, Mode::Sip, opus().as_ref()),
            Mode::Normal
        );
        // Every account answered: what they say stands.
        assert_eq!(
            safe_mode_for(&partial, 1, &policy, Mode::Normal, opus().as_ref()),
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

    #[test]
    fn dwell_is_room_above_each_windows_own_floor_as_a_share_of_what_the_floor_leaves() {
        let floors = Floors {
            session: 0.25,
            weekly: 0.10,
        };
        let land =
            |readings: &[Reading]| choose_above(readings, floors, None).map(|r| r.account.clone());
        // 40% of the 5h is 15 points over its floor of 25 (0.20 of the 75
        // the floor leaves); 30% of the weekly is 20 over its floor of 10
        // (0.22 of 90). Held to each other's floors the first would win.
        assert_eq!(
            land(&[
                reading("five-hour-tight", 0.60, 0.10),
                reading("weekly-tight", 0.10, 0.70)
            ]),
            Some("weekly-tight".into())
        );
        // 45% of the 5h is 0.267 of what its floor leaves; 32% of the
        // weekly is 0.244. In raw points over the floor (20 against 22) the
        // weekly-tight one would win.
        assert_eq!(
            land(&[
                reading("five-hour-tight", 0.55, 0.10),
                reading("weekly-tight", 0.10, 0.68)
            ]),
            Some("five-hour-tight".into())
        );
    }

    #[test]
    fn a_landing_is_held_to_the_landing_floor_of_each_window() {
        let policy = PolicySettings::default();
        assert!(policy.land_min_5h_remaining > policy.land_min_7d_remaining + 0.02);
        let exists = |r: Reading| landing_exists(&[r], &policy, opus().as_ref());
        // Just under the 5h landing floor, which is over the weekly one.
        assert!(!exists(reading(
            "five-hour-short",
            1.0 - policy.land_min_5h_remaining + 0.01,
            0.0
        )));
        // Just over the weekly landing floor, which is under the 5h one.
        assert!(exists(reading(
            "weekly-enough",
            0.0,
            1.0 - policy.land_min_7d_remaining - 0.01
        )));
        assert!(!exists(reading(
            "weekly-short",
            0.0,
            1.0 - policy.land_min_7d_remaining + 0.01
        )));
    }

    #[test]
    fn force_decides_on_what_was_read_and_every_switch_says_why() {
        let policy = PolicySettings::default();
        let model = opus();
        let decide_on = |readings: &[Reading], expected, current, force| {
            let situation = Situation {
                readings,
                expected,
                current,
                model: model.as_ref(),
                force,
            };
            match decide(&situation, &policy) {
                Decision::Hold(hold) => format!("hold {hold:?}"),
                Decision::Stay { healthy } => format!("stay healthy={healthy}"),
                Decision::Switch { to, trigger } => format!("{} {trigger:?}", to.account),
                Decision::NoneViable => "none".to_owned(),
            }
        };
        let healthy = [reading("cur", 0.30, 0.50), reading("fresh", 0.0, 0.0)];
        // One of three accounts did not answer: nothing is decided, unless forced.
        assert_eq!(
            decide_on(&healthy, 3, Some("cur"), false),
            "hold PartialProbe"
        );
        assert_eq!(decide_on(&healthy, 3, Some("cur"), true), "fresh Forced");
        // Forced, a healthy default still stays when it is the best there is.
        assert_eq!(
            decide_on(&healthy, 2, Some("fresh"), true),
            "stay healthy=true"
        );
        assert_eq!(
            decide_on(&healthy, 2, Some("cur"), false),
            "stay healthy=true"
        );
        // The default is managed and has no reading among a complete set.
        assert_eq!(
            decide_on(&healthy, 2, Some("unread"), false),
            "hold DefaultUnreadable"
        );
        assert_eq!(
            decide_on(&healthy, 2, Some("unread"), true),
            "fresh NoDefault"
        );
        // A refused default leaves because it is refused, not for a floor.
        let refused = [crate::testing::refused("cur"), reading("fresh", 0.0, 0.0)];
        assert_eq!(decide_on(&refused, 2, Some("cur"), false), "fresh Refused");
        // Nowhere above the landing floors: the default drains to its sip
        // floor, then takes the scrap.
        let scraps = |session_used| {
            [
                reading("cur", session_used, 0.50),
                reading("scrap", 0.90, 0.24),
            ]
        };
        assert_eq!(
            decide_on(&scraps(0.95), 2, Some("cur"), false),
            "stay healthy=false"
        );
        assert_eq!(
            decide_on(&scraps(0.99), 2, Some("cur"), false),
            "scrap SipFloor"
        );
    }

    const NO_FLOOR: Floors = Floors {
        session: 0.0,
        weekly: 0.0,
    };

    fn names<'a>(ranked: &[&'a Reading]) -> Vec<&'a str> {
        ranked.iter().map(|r| r.account.as_str()).collect()
    }

    #[test]
    fn the_same_ranking_serves_codex_readings() {
        use crate::quota::{CodexDetail, Detail, Limit, Scope, Span};
        let codex = |name: &str, windows: &[(Span, f64, i64)], allowed: bool| Reading {
            limits: windows
                .iter()
                .map(|(span, used, reset)| {
                    Limit::new(*span, Scope::All, Window::new(*used, *reset), Source::Poll)
                })
                .collect(),
            detail: Detail::Codex(CodexDetail {
                allowed: Some(allowed),
                ..CodexDetail::default()
            }),
            ..Reading::empty(Service::Codex, name, 0)
        };
        let week = 7 * 86_400;
        let fleet = [
            // 5h nearly unused, but the weekly has 20% left.
            codex(
                "both",
                &[(Span::FiveHour, 0.05, 3_600), (Span::Weekly, 0.80, week)],
                true,
            ),
            // Weekly only, 58% left: the longest dwell.
            codex("weekly-only", &[(Span::Weekly, 0.42, week)], true),
            // Roomiest of all, but the backend refuses it.
            codex("refused", &[(Span::Weekly, 0.01, week)], false),
            codex("spent", &[(Span::Weekly, 1.0, week)], true),
            // A daily window is held to the weekly floor and ranks like one.
            codex(
                "daily-tight",
                &[(Span::Weekly, 0.10, week), (Span::Other(86_400), 0.90, 600)],
                true,
            ),
            Reading::failed(
                Service::Codex,
                "dead",
                0,
                ReadError::Http {
                    status: 401,
                    body: String::new(),
                },
            ),
        ];
        assert_eq!(
            names(&rank_above(&fleet, NO_FLOOR, None)),
            ["weekly-only", "both", "daily-tight"]
        );
        assert_eq!(
            choose_above(&fleet, NO_FLOOR, None).map(|r| r.account.as_str()),
            Some("weekly-only")
        );
        // A floor applies to them as to Claude: 20% weekly left is under 25%.
        let floors = Floors {
            session: 0.05,
            weekly: 0.25,
        };
        assert_eq!(names(&rank_above(&fleet, floors, None)), ["weekly-only"]);
        // Weekly room about to expire is spent first, here too.
        let mut expiring = fleet.to_vec();
        expiring[0].set_window(
            Span::Weekly,
            Scope::All,
            Window::new(0.80, 3_600),
            Source::Poll,
        );
        assert_eq!(
            names(&rank_above(&expiring, NO_FLOOR, None)),
            ["both", "weekly-only", "daily-tight"]
        );
    }

    #[test]
    fn a_seat_leaves_at_its_floor_for_real_room_and_a_spent_one_for_any() {
        use crate::quota::{CodexDetail, Detail, Limit, Scope, Span};
        let codex = |name: &str, session_used: f64, weekly_used: f64| Reading {
            limits: vec![
                Limit::new(
                    Span::FiveHour,
                    Scope::All,
                    Window::new(session_used, 0),
                    Source::Poll,
                ),
                Limit::new(
                    Span::Weekly,
                    Scope::All,
                    Window::new(weekly_used, 0),
                    Source::Poll,
                ),
            ],
            detail: Detail::Codex(CodexDetail {
                allowed: Some(true),
                ..CodexDetail::default()
            }),
            ..Reading::empty(Service::Codex, name, 0)
        };
        let leave = Floors {
            session: 0.20,
            weekly: 0.20,
        };
        let land = Floors {
            session: 0.30,
            weekly: 0.30,
        };
        let to = |seat: &Reading, others: &[Reading]| match relocate(seat, others, leave, land) {
            Relocation::Stay => "stay".to_owned(),
            Relocation::Drain => "drain".to_owned(),
            Relocation::Move(to) => to.account.clone(),
        };
        let roomy = codex("roomy", 0.1, 0.1);
        let scraps = codex("scraps", 0.75, 0.2);
        // Above both floors: nothing moves, however roomy the rest.
        assert_eq!(
            to(&codex("seat", 0.5, 0.79), std::slice::from_ref(&roomy)),
            "stay"
        );
        // Either window at its floor leaves, for the account with landing room.
        assert_eq!(
            to(&codex("seat", 0.81, 0.1), &[scraps.clone(), roomy.clone()]),
            "roomy"
        );
        assert_eq!(
            to(&codex("seat", 0.1, 0.80), std::slice::from_ref(&roomy)),
            "roomy"
        );
        // 25% left is over the leaving floor and under the landing floor: a
        // low seat drains in place rather than land there.
        assert_eq!(
            to(&codex("seat", 0.85, 0.1), std::slice::from_ref(&scraps)),
            "drain"
        );
        // A spent seat takes any room at all; nothing with room is a drain.
        assert_eq!(
            to(&codex("seat", 1.0, 0.1), std::slice::from_ref(&scraps)),
            "scraps"
        );
        assert_eq!(
            to(&codex("seat", 1.0, 0.1), &[codex("spent", 0.2, 1.0)]),
            "drain"
        );
        // A seat the backend refuses is spent whatever its windows say.
        let mut refused = codex("seat", 0.1, 0.1);
        refused.detail = Detail::Codex(CodexDetail {
            allowed: Some(false),
            ..CodexDetail::default()
        });
        assert_eq!(to(&refused, std::slice::from_ref(&scraps)), "scraps");
        assert_eq!(to(&refused, &[]), "drain");
    }

    #[test]
    fn the_same_ranking_serves_antigravity_readings() {
        use crate::quota::{Limit, Scope, Span};
        let agy = |name: &str, groups: &[(&str, f64, f64)]| Reading {
            limits: groups
                .iter()
                .flat_map(|(group, session, weekly)| {
                    [(Span::FiveHour, *session), (Span::Weekly, *weekly)].map(|(span, used)| {
                        Limit::new(
                            span,
                            Scope::Group((*group).into()),
                            Window::new(used, 0),
                            Source::Poll,
                        )
                    })
                })
                .collect(),
            ..Reading::empty(Service::Antigravity, name, 0)
        };
        let fleet = [
            agy(
                "a",
                &[
                    ("Gemini Models", 0.9, 0.2),
                    ("Claude and GPT models", 0.1, 0.1),
                ],
            ),
            agy(
                "b",
                &[
                    ("Gemini Models", 0.3, 0.3),
                    ("Claude and GPT models", 0.5, 0.6),
                ],
            ),
            agy("spent", &[("Gemini Models", 0.0, 1.0)]),
        ];
        // As read, no limit is account-wide, so nothing is a landing.
        assert_eq!(choose_above(&fleet, NO_FLOOR, None), None);
        let ranked_on = |group: Option<&str>| -> Vec<String> {
            let counted: Vec<Reading> = fleet
                .iter()
                .filter_map(|reading| reading.scoped_to(group))
                .collect();
            rank_above(&counted, NO_FLOOR, None)
                .iter()
                .map(|r| r.account.clone())
                .collect()
        };
        // Any group: a's Gemini 5h (10% left) is tighter than b's worst (40%).
        assert_eq!(ranked_on(None), ["b", "a"]);
        assert_eq!(ranked_on(Some("claude")), ["a", "b"]);
        assert_eq!(ranked_on(Some("gemini")), ["b", "a"]);
        assert!(ranked_on(Some("no such group")).is_empty());
    }
}
