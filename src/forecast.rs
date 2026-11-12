//! When a service's accounts run dry at the rate they were used lately.
//! Pure: no IO, no clock.
//!
//! Each account counts through its longest account-wide window (Claude's
//! and Codex's weekly). Its room is what that window has left for the target
//! model above the floor; its burn is the use that window gained over the
//! last [`BURN_HOURS`] hours, each interval between two reads of one reset
//! window weighted by its length. The fleet's rate is the sum of the
//! accounts' rates, and the fleet is walked forward at that rate, spending
//! the account whose window resets soonest first (as the rotation lands) and
//! refilling each window when its reset arrives.
//!
//! Rooms and rates of different accounts are added as fractions of one
//! window, which is exact when the accounts are the same size and an
//! approximation otherwise. Accounts whose longest windows differ in length
//! are not added at all: there is no forecast then.

use serde::Serialize;

use crate::quota::{Limit, ModelKey, Reading, Scope, Service, Span};

/// The rate is taken over this many hours.
pub const BURN_HOURS: i64 = 6;
/// A fleet that lasts this long is not given a time.
pub const HORIZON_SECS: i64 = 7 * 86_400;
/// An account's history must cover this much of one reset window before its
/// rate counts.
pub const MIN_COVER_SECS: i64 = 3_600;
/// Use per hour below this is no use.
const IDLE_RATE: f64 = 1e-5;

/// One account of the service.
#[derive(Debug, Clone, Copy)]
pub struct Input<'a> {
    pub latest: &'a Reading,
    /// The account's reads of the last [`BURN_HOURS`] hours, oldest first.
    pub recent: &'a [Reading],
    /// Unused resets on offer that would refill the account's window.
    pub refills: u32,
}

/// What the walk found.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(tag = "state", rename_all = "kebab-case")]
pub enum Outlook {
    /// No account has room left at `at` (epoch seconds).
    Dry { at: i64 },
    /// Room lasts past the horizon.
    Beyond,
    /// Nothing was used over the hours the rate is taken from.
    Idle,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Forecast {
    pub service: Service,
    pub outlook: Outlook,
    /// The hours the rate is taken over.
    pub burn_hours: i64,
    /// Use per hour across the fleet, in fractions of one account's window.
    pub rate: f64,
    /// Room left now across the fleet, in the same unit.
    pub room: f64,
    /// The window the accounts are counted through.
    pub span: Span,
    /// Accounts with a window to count, and how many of them have a rate.
    pub accounts: usize,
    pub measured: usize,
    /// Unused resets on offer across the counted accounts.
    pub refills: u32,
    /// The outlook if every one of those resets were spent as its account
    /// ran out. `None` without resets on offer or without a dry time.
    pub with_resets: Option<Outlook>,
}

/// The account-wide limit of the longest known length.
fn governing(reading: &Reading) -> Option<&Limit> {
    reading
        .shared()
        .filter(|limit| limit.span.seconds().is_some())
        .max_by_key(|limit| limit.span.seconds())
}

/// `(covered seconds, use gained)` of the `span` window over `recent`: the
/// intervals between consecutive reads of one reset window that did not go
/// down.
fn burn(recent: &[Reading], span: Span) -> (i64, f64) {
    recent
        .windows(2)
        .filter_map(|pair| {
            let secs = pair[1].at - pair[0].at;
            let (a, b) = (
                pair[0].window(span, &Scope::All)?,
                pair[1].window(span, &Scope::All)?,
            );
            (secs > 0 && a.resets_at == b.resets_at && b.used >= a.used)
                .then_some((secs, b.used - a.used))
        })
        .fold((0, 0.0), |(secs, used), (more_secs, more_used)| {
            (secs + more_secs, used + more_used)
        })
}

/// One account in the walk.
#[derive(Debug, Clone, Copy)]
struct Pool {
    room: f64,
    /// Room right after a reset.
    full: f64,
    /// The pending reset; `None` while the window is not running.
    reset: Option<i64>,
    refills: u32,
}

/// The pool to act on next among those `wanted`: the one whose window
/// resets soonest, a window that is not running last.
fn soonest(pools: &[Pool], wanted: impl Fn(&Pool) -> bool) -> Option<usize> {
    pools
        .iter()
        .enumerate()
        .filter(|(_, pool)| wanted(pool))
        .min_by_key(|(_, pool)| pool.reset.unwrap_or(i64::MAX))
        .map(|(index, _)| index)
}

/// When no pool has room, walking from `now` at `rate` per hour; `None`
/// when room lasts to the horizon. With `use_refills`, a pool's resets on
/// offer are spent when nothing else has room.
fn walk(
    mut pools: Vec<Pool>,
    rate: f64,
    span_secs: i64,
    now: i64,
    use_refills: bool,
) -> Option<i64> {
    let end = (now + HORIZON_SECS) as f64;
    let mut t = now as f64;
    // Each step ends at a reset or empties a pool, so the walk is finite;
    // the bound is for a rate so small that rounding stalls it.
    for _ in 0..10_000 {
        for pool in &mut pools {
            if pool.reset.is_some_and(|reset| reset as f64 <= t) {
                pool.room = pool.full;
                pool.reset = None;
            }
        }
        let Some(index) = soonest(&pools, |pool| pool.room > 0.0) else {
            let refill = use_refills
                .then(|| soonest(&pools, |pool| pool.refills > 0 && pool.full > 0.0))
                .flatten();
            match refill {
                Some(index) => {
                    pools[index].refills -= 1;
                    pools[index].room = pools[index].full;
                    continue;
                }
                None => return Some(t.round() as i64),
            }
        };
        // A window that is not running starts with its first use.
        pools[index]
            .reset
            .get_or_insert(t.round() as i64 + span_secs);
        let next = pools
            .iter()
            .filter_map(|pool| pool.reset)
            .map(|reset| reset as f64)
            .filter(|reset| *reset > t)
            .fold(end, f64::min);
        let empties = t + pools[index].room / rate * 3_600.0;
        if empties <= next {
            pools[index].room = 0.0;
            t = empties;
        } else {
            pools[index].room -= rate * (next - t) / 3_600.0;
            t = next;
        }
        if t >= end {
            return None;
        }
    }
    None
}

/// The forecast for one service's accounts, for sessions on `model`, with
/// `floor` of each window kept back. `None` when the data cannot support
/// one: no account has an account-wide window of known length, the accounts'
/// windows differ in length, or no account's history covers
/// [`MIN_COVER_SECS`] of one reset window.
pub fn forecast(
    service: Service,
    accounts: &[Input<'_>],
    model: Option<&ModelKey>,
    floor: f64,
    now: i64,
) -> Option<Forecast> {
    let counted: Vec<(&Input<'_>, &Limit)> = accounts
        .iter()
        .filter(|account| account.latest.error.is_none())
        .filter_map(|account| Some((account, governing(account.latest)?)))
        .collect();
    let span = counted.first()?.1.span;
    if counted.iter().any(|(_, limit)| limit.span != span) {
        return None;
    }
    let span_secs = span.seconds()?;
    let mut pools = Vec::with_capacity(counted.len());
    let (mut rate, mut measured) = (0.0, 0);
    for (account, limit) in &counted {
        let reading = account.latest;
        // A model's limit that is a share of the weekly caps the room a
        // full window gives it.
        let (room, full) = if span == Span::Weekly {
            (
                reading
                    .model_room(model)
                    .unwrap_or_else(|| limit.window.remaining()),
                reading
                    .limits_for(model)
                    .filter_map(|limit| limit.share)
                    .fold(1.0, f64::min),
            )
        } else {
            (limit.window.remaining(), 1.0)
        };
        pools.push(Pool {
            room: (room - floor).max(0.0),
            full: (full - floor).max(0.0),
            reset: Some(limit.window.resets_at).filter(|reset| *reset > now),
            refills: account.refills,
        });
        let (covered, used) = burn(account.recent, span);
        if covered >= MIN_COVER_SECS {
            measured += 1;
            rate += used / (covered as f64 / 3_600.0);
        }
    }
    if measured == 0 {
        return None;
    }
    let refills: u32 = pools.iter().map(|pool| pool.refills).sum();
    let outlook = |use_refills: bool| {
        if rate < IDLE_RATE {
            Outlook::Idle
        } else {
            match walk(pools.clone(), rate, span_secs, now, use_refills) {
                Some(at) => Outlook::Dry { at },
                None => Outlook::Beyond,
            }
        }
    };
    let plain = outlook(false);
    Some(Forecast {
        service,
        outlook: plain,
        burn_hours: BURN_HOURS,
        rate,
        room: pools.iter().map(|pool| pool.room).sum(),
        span,
        accounts: pools.len(),
        measured,
        refills,
        with_resets: (refills > 0 && matches!(plain, Outlook::Dry { .. })).then(|| outlook(true)),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::quota::{Source, Window};
    use crate::testing::model_limit;

    const NOW: i64 = 1_794_000_000;
    const HOUR: i64 = 3_600;
    const DAY: i64 = 86_400;

    /// A Claude reading at `at` whose weekly window is `used` and resets at `reset`.
    fn weekly(account: &str, at: i64, used: f64, reset: i64) -> Reading {
        let mut reading = crate::testing::reading(account, 0.0, used);
        reading.at = at;
        reading.set_session(None);
        reading.set_weekly(Some(Window::new(used, reset)));
        reading
    }

    /// Six hours of reads, one an hour, rising evenly from `from` to `to`.
    fn history(account: &str, from: f64, to: f64, reset: i64) -> Vec<Reading> {
        (0..=6)
            .map(|hour| {
                let used = from + (to - from) * hour as f64 / 6.0;
                weekly(account, NOW - (6 - hour) * HOUR, used, reset)
            })
            .collect()
    }

    fn run(histories: &[Vec<Reading>], refills: &[u32], floor: f64) -> Option<Forecast> {
        let inputs: Vec<Input<'_>> = histories
            .iter()
            .enumerate()
            .map(|(index, recent)| Input {
                latest: recent.last().unwrap(),
                recent,
                refills: refills.get(index).copied().unwrap_or(0),
            })
            .collect();
        forecast(Service::Claude, &inputs, None, floor, NOW)
    }

    fn dry_in_hours(forecast: &Forecast) -> f64 {
        match forecast.outlook {
            Outlook::Dry { at } => (at - NOW) as f64 / HOUR as f64,
            other => panic!("not dry: {other:?}"),
        }
    }

    #[test]
    fn one_account_runs_dry_when_its_room_is_spent_at_the_measured_rate() {
        // 6% of the weekly in six hours is 1%/h; 40% is left; the reset is
        // past the dry time.
        let only = history("a", 0.54, 0.60, NOW + 5 * DAY);
        let forecast = run(&[only], &[], 0.0).unwrap();
        assert!((forecast.rate - 0.01).abs() < 1e-9, "{}", forecast.rate);
        assert!((forecast.room - 0.40).abs() < 1e-9);
        assert!((dry_in_hours(&forecast) - 40.0).abs() < 0.01);
        assert_eq!(
            (forecast.accounts, forecast.measured, forecast.burn_hours),
            (1, 1, 6)
        );
        assert_eq!(forecast.span, Span::Weekly);
        assert_eq!(forecast.with_resets, None);
    }

    #[test]
    fn the_floor_is_kept_back_from_every_account() {
        let only = history("a", 0.54, 0.60, NOW + 5 * DAY);
        let forecast = run(&[only], &[], 0.05).unwrap();
        assert!((dry_in_hours(&forecast) - 35.0).abs() < 0.01);
        // And from the window a reset gives back: 5% lasts past the reset
        // in 4 hours, then 95% lasts 95 more.
        let resetting = history("a", 0.84, 0.90, NOW + 4 * HOUR);
        let forecast = run(&[resetting], &[], 0.05).unwrap();
        assert!(
            (dry_in_hours(&forecast) - 99.0).abs() < 0.01,
            "{}",
            dry_in_hours(&forecast)
        );
    }

    #[test]
    fn the_fleet_spends_every_account_at_the_sum_of_the_rates() {
        // One account in use at 2%/h, one idle with 50% left: 30% + 50% at
        // 2%/h is 40 hours.
        let busy = history("busy", 0.58, 0.70, NOW + 6 * DAY);
        let spare = history("spare", 0.50, 0.50, NOW + 6 * DAY);
        let forecast = run(&[busy, spare], &[], 0.0).unwrap();
        assert!((forecast.rate - 0.02).abs() < 1e-9);
        assert!((dry_in_hours(&forecast) - 40.0).abs() < 0.01);
        assert_eq!((forecast.accounts, forecast.measured), (2, 2));
    }

    #[test]
    fn a_reset_inside_the_horizon_refills_its_account() {
        // 10% left at 1%/h would be dry in 10 hours, but the window resets
        // in 4: the 6% unspent then is lost, a full window follows, and
        // 100% lasts 100 more hours.
        let only = history("a", 0.84, 0.90, NOW + 4 * HOUR);
        let forecast = run(&[only], &[], 0.0).unwrap();
        assert!((dry_in_hours(&forecast) - 104.0).abs() < 0.01);
    }

    #[test]
    fn the_account_that_resets_soonest_is_spent_first() {
        // Both have 20% left and the rate is 1%/h; `soon` resets in 30
        // hours. Spending `late` first would leave half of `soon` unspent
        // at its reset and end at 130 hours.
        let soon = history("soon", 0.74, 0.80, NOW + 30 * HOUR);
        let late = history("late", 0.80, 0.80, NOW + 6 * DAY + 12 * HOUR);
        let forecast = run(&[late, soon], &[], 0.0).unwrap();
        // 20 h on soon, 20 h on late (soon refills meanwhile, at 30 h),
        // then 100 h on the refilled soon: 140 h.
        assert!(
            (dry_in_hours(&forecast) - 140.0).abs() < 0.01,
            "{}",
            dry_in_hours(&forecast)
        );
    }

    #[test]
    fn room_that_outlasts_the_week_has_no_time_and_no_use_is_idle() {
        let slow = history("a", 0.100, 0.106, NOW + 6 * DAY);
        assert_eq!(
            run(std::slice::from_ref(&slow), &[], 0.0).unwrap().outlook,
            Outlook::Beyond
        );
        // Resets on offer add nothing to say when nothing runs dry.
        let offered = run(&[slow], &[2], 0.0).unwrap();
        assert_eq!(
            (offered.outlook, offered.refills, offered.with_resets),
            (Outlook::Beyond, 2, None)
        );
        let idle = history("a", 0.3, 0.3, NOW + 6 * DAY);
        let forecast = run(&[idle], &[], 0.0).unwrap();
        assert_eq!(forecast.outlook, Outlook::Idle);
        assert_eq!(forecast.rate, 0.0);
    }

    #[test]
    fn resets_on_offer_are_reported_apart_and_never_counted_as_room() {
        let only = history("a", 0.54, 0.60, NOW + 5 * DAY);
        let forecast = run(&[only], &[1], 0.0).unwrap();
        // Without the reset: 40 hours, as when none is on offer.
        assert!((dry_in_hours(&forecast) - 40.0).abs() < 0.01);
        assert_eq!(forecast.refills, 1);
        // With it: the 40%, then a full window until the reset at 120 h
        // refills it again; that lasts past the week.
        assert_eq!(forecast.with_resets, Some(Outlook::Beyond));
        // A faster fleet: 5%/h, 40% left, then one refill of 100%.
        let fast = history("a", 0.30, 0.60, NOW + 6 * DAY);
        let forecast = run(&[fast], &[1], 0.0).unwrap();
        assert!((dry_in_hours(&forecast) - 8.0).abs() < 0.01);
        assert_eq!(
            forecast.with_resets,
            Some(Outlook::Dry {
                at: NOW + 28 * HOUR
            })
        );
    }

    #[test]
    fn a_model_limit_that_is_a_share_of_the_weekly_caps_the_room() {
        let fable = ModelKey::new("claude-fable-5-1");
        let mut recent = history("a", 0.14, 0.20, NOW + 6 * DAY);
        for reading in &mut recent {
            let mut limit = model_limit("fable", "Fable", 0.4, Source::Profile);
            limit.share = Some(0.5);
            reading.limits.push(limit);
        }
        let input = Input {
            latest: recent.last().unwrap(),
            recent: &recent,
            refills: 0,
        };
        // 60% of Fable's limit at half the weekly is 30% of the weekly,
        // less than the 80% the weekly has left.
        let on_fable = forecast(Service::Claude, &[input], Some(&fable), 0.0, NOW).unwrap();
        assert!((on_fable.room - 0.30).abs() < 1e-9, "{}", on_fable.room);
        let on_any = forecast(Service::Claude, &[input], None, 0.0, NOW).unwrap();
        assert!((on_any.room - 0.80).abs() < 1e-9);
        // A reset gives Fable back its share of the weekly, not a whole
        // weekly: 30% at 1%/h lasts past the reset in 10 hours, then 50%
        // lasts 50 more.
        let mut resetting = history("a", 0.14, 0.20, NOW + 10 * HOUR);
        for reading in &mut resetting {
            let mut limit = model_limit("fable", "Fable", 0.4, Source::Profile);
            limit.share = Some(0.5);
            reading.limits.push(limit);
        }
        let input = Input {
            latest: resetting.last().unwrap(),
            recent: &resetting,
            refills: 0,
        };
        let on_fable = forecast(Service::Claude, &[input], Some(&fable), 0.0, NOW).unwrap();
        assert!(
            (dry_in_hours(&on_fable) - 60.0).abs() < 0.01,
            "{}",
            dry_in_hours(&on_fable)
        );
    }

    #[test]
    fn no_forecast_without_a_measured_rate() {
        // No history at all.
        let latest = weekly("a", NOW, 0.5, NOW + DAY);
        let unmeasured = Input {
            latest: &latest,
            recent: std::slice::from_ref(&latest),
            refills: 0,
        };
        assert_eq!(
            forecast(Service::Claude, &[unmeasured], None, 0.0, NOW),
            None
        );
        // Half an hour of history is too little to call a rate.
        let short = vec![
            weekly("a", NOW - 1_800, 0.50, NOW + DAY),
            weekly("a", NOW, 0.52, NOW + DAY),
        ];
        assert_eq!(run(&[short], &[], 0.0), None);
        // Reads on both sides of a reset share no window: nothing is covered.
        let across = vec![
            weekly("a", NOW - 3 * HOUR, 0.90, NOW - HOUR),
            weekly("a", NOW, 0.02, NOW + 7 * DAY),
        ];
        assert_eq!(run(&[across], &[], 0.0), None);
        // A failed read has no window.
        let failed = crate::testing::refused("a");
        let input = Input {
            latest: &failed,
            recent: &[],
            refills: 0,
        };
        assert_eq!(forecast(Service::Claude, &[input], None, 0.0, NOW), None);
        assert_eq!(forecast(Service::Claude, &[], None, 0.0, NOW), None);
    }

    #[test]
    fn the_rate_is_the_use_gained_inside_one_reset_window_over_the_time_it_covers() {
        let rate = |reads: &[(i64, f64, i64)]| {
            let recent: Vec<Reading> = reads
                .iter()
                .map(|(hours_ago, used, reset)| weekly("a", NOW - hours_ago * HOUR, *used, *reset))
                .collect();
            run(&[recent], &[], 0.0).map(|forecast| forecast.rate)
        };
        let close = |rate: Option<f64>, want: f64| {
            let rate = rate.unwrap();
            assert!((rate - want).abs() < 1e-9, "{rate} is not {want}");
        };
        let (old, new) = (NOW - HOUR - 1, NOW + 7 * DAY);
        // Two hours of reads, not the six the rate is named for: 10%/h.
        close(
            rate(&[(2, 0.10, new), (1, 0.20, new), (0, 0.30, new)]),
            0.10,
        );
        // The window reset between the second and third read, and the new
        // one reads higher: that jump is not use.
        close(
            rate(&[
                (3, 0.10, old),
                (2, 0.20, old),
                (1, 0.50, new),
                (0, 0.60, new),
            ]),
            0.10,
        );
        // A value the server took back is not negative use.
        close(
            rate(&[
                (3, 0.30, new),
                (2, 0.20, new),
                (1, 0.25, new),
                (0, 0.35, new),
            ]),
            0.075,
        );
        // One hour of one window is enough to call a rate; less is not.
        close(rate(&[(1, 0.10, new), (0, 0.12, new)]), 0.02);
        assert_eq!(
            rate(&[(2, 0.10, old), (1, 0.50, new), (0, 0.52, new)]).map(|rate| rate > 0.0),
            Some(true),
            "the hour after the reset counts"
        );
    }

    #[test]
    fn an_account_counts_through_its_longest_window() {
        // A 5h window at its wall beside a weekly with room: the weekly is
        // what the fleet runs dry on.
        let mut recent = history("a", 0.54, 0.60, NOW + 5 * DAY);
        for reading in &mut recent {
            reading.set_session(Some(Window::new(0.99, NOW + HOUR)));
        }
        let forecast = run(&[recent], &[], 0.0).unwrap();
        assert_eq!(forecast.span, Span::Weekly);
        assert!((forecast.room - 0.40).abs() < 1e-9, "{}", forecast.room);
        assert!((dry_in_hours(&forecast) - 40.0).abs() < 0.01);
    }

    #[test]
    fn accounts_counted_through_windows_of_different_lengths_are_not_added() {
        let week = history("a", 0.54, 0.60, NOW + 5 * DAY);
        let mut month = history("b", 0.54, 0.60, NOW + 20 * DAY);
        for reading in &mut month {
            reading.limits = vec![Limit::new(
                Span::Monthly,
                Scope::All,
                reading.weekly().unwrap(),
                Source::Poll,
            )];
        }
        assert_eq!(run(&[week, month.clone()], &[], 0.0), None);
        // Alone, the monthly accounts are a fleet of their own.
        let forecast = run(&[month], &[], 0.0).unwrap();
        assert_eq!(forecast.span, Span::Monthly);
        assert!((dry_in_hours(&forecast) - 40.0).abs() < 0.01);
    }

    #[test]
    fn a_window_that_is_not_running_starts_when_it_is_first_spent() {
        // `spare` has never been used: no reset is pending. `busy` runs out
        // in 10 hours; `spare` then lasts 100 hours, its week starting then,
        // so nothing refills before the fleet is dry at 110 hours.
        let busy = history("busy", 0.84, 0.90, NOW + 6 * DAY + 20 * HOUR);
        let spare = history("spare", 0.0, 0.0, 0);
        let forecast = run(&[busy, spare], &[], 0.0).unwrap();
        assert!(
            (dry_in_hours(&forecast) - 110.0).abs() < 0.01,
            "{}",
            dry_in_hours(&forecast)
        );
    }
}
