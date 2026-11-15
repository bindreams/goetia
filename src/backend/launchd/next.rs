//! The next fire of a task's calendar entries in a time zone, for the launchd backend: launchd takes
//! `StartCalendarInterval` entries and reports no next fire.
//!
//! The model is launchd's own, measured on macOS 26.7.2 in `America/New_York`. A local time the zone skips
//! (a spring-forward gap) and one it repeats (a fall-back fold) are read per entry:
//!
//! - **gap:** an entry that names its hour fires once, at the gap's end, whatever minute it names. An entry
//!   with no hour (`{Minute m}`, `{}`) has only real local times to fire at, so the missing hour gives it
//!   none: `{Minute 30}` goes from 01:30 to 03:30. See the knowledge-base note "A StartCalendarInterval entry
//!   in a spring-forward gap fires once, at the end of the gap".
//! - **fold:** an entry with no date or a weekday fires at both passes; an entry with a day, or a month and a
//!   day, only at the first. See "In a fall-back fold, StartCalendarInterval entries with no date or a
//!   Weekday fire at both passes, and entries with a Day or a Month and Day at the first only", and, for the
//!   entry with no hour, "A StartCalendarInterval entry with no Hour skips the gap hour and fires at the
//!   next hour's minute, and fires at both passes of a fold".
//!
//! The next fire of a task is the earliest over its entries.

use std::collections::HashMap;
use std::path::Path;
use std::time::SystemTime;

use jiff::SignedDuration;
use jiff::Timestamp;
use jiff::civil::Date;
use jiff::tz::{AmbiguousOffset, Offset, TimeZone};

use super::calendar::Entry;
use crate::error::{Error, Result};

/// The days of the Gregorian calendar's cycle: 400 years, 97 of them leap, a whole number of weeks. Dates,
/// weekdays and leap years all repeat after it, and a zone's rules beyond its listed transitions are yearly
/// rules over the same calendar. So an entry that has not fired within one cycle of the walk's start never
/// will.
const CALENDAR_CYCLE_DAYS: u32 = 146_097;

/// An entry's date keys: month, day of the month, weekday (0 for Sunday), each `None` for a wildcard.
type DateKey = (Option<u8>, Option<u8>, Option<u8>);

/// The first instant strictly after `after`, on a whole minute, that one of `entries` fires at in `zone`.
///
/// Precondition: the keys an entry omits are a prefix of month, day or weekday, hour, minute, as
/// [`calendar::entries`] returns them and as launchd reads them (an omitted key above the one it names is a
/// wildcard; one below it is not), and an entry with a weekday has no month and no day. A `debug_assert!`
/// checks it. An entry that names a date no year has
/// (30 February) never fires: it does not make the walk longer than one calendar cycle.
///
/// `None` when no entry fires within a calendar cycle (none, or none that names a real date), or when `after`
/// or the answer lies outside the years jiff represents (to 9999). A caller treats `None` as "no next run
/// known", not as "never".
///
/// [`calendar::entries`]: super::calendar::entries
pub(crate) fn next_fire(entries: &[Entry], after: SystemTime, zone: &TimeZone) -> Option<SystemTime> {
    debug_assert!(
        entries.iter().all(omits_a_prefix),
        "the keys an entry omits are a prefix of month, day or weekday, hour, minute: {entries:?}"
    );
    if entries.is_empty() {
        return None;
    }
    let by_date = index_by_date(entries);
    let after = Timestamp::try_from(after).ok()?;
    // A date's readings lie within an offset's span (`Offset::MAX`, 25:59:59, either way) of its UTC day.
    // The walk starts at the first date that can still read an instant after `after`, and goes on past a
    // fire while a later date can still read an earlier one: dates are not read in order where an offset
    // falls (a fall-back across midnight, or by more than a day), so nothing narrower is exact.
    let span = SignedDuration::from_secs(i64::from(Offset::MAX.seconds()));
    let mut date = after.checked_sub(span).ok()?.to_zoned(TimeZone::UTC).date();
    let mut best: Option<Timestamp> = None;
    let mut walked = 0;
    loop {
        match best {
            // The earliest instant `date` can be read at is its midnight at the largest offset.
            Some(best)
                if Offset::MAX
                    .to_timestamp(date.at(0, 0, 0, 0))
                    .is_ok_and(|earliest| earliest >= best) =>
            {
                break;
            }
            None if walked == CALENDAR_CYCLE_DAYS => break,
            _ => {}
        }
        if let Some(fire) = fire_on(&by_date, zone, date, after) {
            best = Some(best.map_or(fire, |best| best.min(fire)));
        }
        let Ok(tomorrow) = date.tomorrow() else { break };
        date = tomorrow;
        walked += 1;
    }
    best.map(Into::into)
}

/// Whether the entry's omitted keys are a prefix of month, day or weekday, hour, minute, and it does not
/// name both a day and a weekday, or a weekday and a month.
fn omits_a_prefix(entry: &Entry) -> bool {
    let named = [
        entry.month.is_some(),
        entry.day.is_some() || entry.weekday.is_some(),
        entry.hour.is_some(),
        entry.minute.is_some(),
    ];
    named.windows(2).all(|pair| !pair[0] || pair[1])
        && (entry.weekday.is_none() || (entry.day.is_none() && entry.month.is_none()))
}

/// The entries, each under the date keys it names. Every date then looks up the few keys that can name it,
/// not every entry (`* * * 1 *` has 44,640).
fn index_by_date(entries: &[Entry]) -> HashMap<DateKey, Vec<&Entry>> {
    let mut by_date: HashMap<DateKey, Vec<&Entry>> = HashMap::new();
    for entry in entries {
        // 0 and 7 are both Sunday.
        by_date
            .entry((entry.month, entry.day, entry.weekday.map(|weekday| weekday % 7)))
            .or_default()
            .push(entry);
    }
    by_date
}

/// The first instant after `after` at which an entry that names `date` fires on it.
fn fire_on(
    by_date: &HashMap<DateKey, Vec<&Entry>>,
    zone: &TimeZone,
    date: Date,
    after: Timestamp,
) -> Option<Timestamp> {
    let (month, day) = (date.month().unsigned_abs(), date.day().unsigned_abs());
    let weekday = date.weekday().to_sunday_zero_offset().unsigned_abs();
    // Each key an entry can name is either a wildcard or this date's value.
    let keys = [None, Some(month)].into_iter().flat_map(|month| {
        [None, Some(day)]
            .into_iter()
            .flat_map(move |day| [None, Some(weekday)].map(move |weekday| (month, day, weekday)))
    });
    keys.filter_map(|key| by_date.get(&key))
        .flatten()
        .flat_map(|entry| instants(zone, date, entry))
        .filter(|&instant| instant > after)
        .min()
}

/// The instants at which the entry fires on `date` in `zone`: those of each local time it names, read as the
/// module doc's rules say.
fn instants(zone: &TimeZone, date: Date, entry: &Entry) -> impl Iterator<Item = Timestamp> {
    let hours = entry.hour.map_or(0..24, |hour| hour..hour + 1);
    let minutes = entry.minute.map_or(0..60, |minute| minute..minute + 1);
    let (names_hour, names_day) = (entry.hour.is_some(), entry.day.is_some());
    hours
        .flat_map(move |hour| minutes.clone().map(move |minute| (hour, minute)))
        .filter_map(move |(hour, minute)| Some(date.at(i8::try_from(hour).ok()?, i8::try_from(minute).ok()?, 0, 0)))
        .flat_map(move |civil| {
            let at = |offset: Offset| offset.to_timestamp(civil).ok();
            let [first, second] = match zone.to_ambiguous_timestamp(civil).offset() {
                AmbiguousOffset::Unambiguous { offset } => [at(offset), None],
                AmbiguousOffset::Fold { before, after } => [at(before), at(after).filter(|_| !names_day)],
                // The instant `civil` would be read at if the gap were not there, before the transition
                // that ends it.
                AmbiguousOffset::Gap { after, .. } => [
                    at(after)
                        .filter(|_| names_hour)
                        .and_then(|before_gap| zone.following(before_gap).next())
                        .map(|transition| transition.timestamp()),
                    None,
                ],
            };
            [first, second].into_iter().flatten()
        })
}

/// The machine's time zone, from the bytes of `/etc/localtime`. The `TZ` variable is not consulted:
/// launchd schedules by the machine's zone, and a task's next fire must follow it.
// Only macOS calls it; other hosts compile the module for its tests, which read a zone file of their own.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub fn system_zone() -> Result<TimeZone> {
    zone_from(Path::new("/etc/localtime"))
}

/// The zone the TZif file at `path` holds (read through a symlink).
fn zone_from(path: &Path) -> Result<TimeZone> {
    let bytes = std::fs::read(path).map_err(|e| Error::os_at("read the system time zone", path, e))?;
    TimeZone::tzif("localtime", &bytes).map_err(|e| Error::os_at("parse the system time zone", path, e))
}

#[cfg(test)]
#[path = "next_tests.rs"]
mod next_tests;
