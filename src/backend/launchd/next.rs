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

use std::path::Path;
use std::time::SystemTime;

use jiff::SignedDuration;
use jiff::Timestamp;
use jiff::civil::Date;
use jiff::tz::{AmbiguousOffset, Offset, TimeZone};

use super::calendar::Entry;
use crate::error::{Error, Result};

/// The first instant strictly after `after`, on a whole minute, that one of `entries` fires at in `zone`.
///
/// `None` when `entries` is empty, or when `after` or the answer lies outside the years jiff represents
/// (to 9999). A caller treats `None` as "no next run known", not as "never".
pub(crate) fn next_fire(entries: &[Entry], after: SystemTime, zone: &TimeZone) -> Option<SystemTime> {
    if entries.is_empty() {
        return None;
    }
    let after = Timestamp::try_from(after).ok()?;
    // A date's readings lie within an offset's span (`Offset::MAX`, 25:59:59, either way) of its UTC day.
    // The walk starts at the first date that can still read an instant after `after`, and goes on past a
    // fire while a later date can still read an earlier one: dates are not read in order where an offset
    // falls (a fall-back across midnight, or by more than a day), so nothing narrower is exact.
    let span = SignedDuration::from_secs(i64::from(Offset::MAX.seconds()));
    let mut date = after.checked_sub(span).ok()?.to_zoned(TimeZone::UTC).date();
    let mut best: Option<Timestamp> = None;
    loop {
        if let Some(best) = best {
            // The earliest instant `date` can be read at is its midnight at the largest offset.
            if Offset::MAX
                .to_timestamp(date.at(0, 0, 0, 0))
                .is_ok_and(|earliest| earliest >= best)
            {
                break;
            }
        }
        if let Some(fire) = fire_on(entries, zone, date, after) {
            best = Some(best.map_or(fire, |best| best.min(fire)));
        }
        let Ok(tomorrow) = date.tomorrow() else { break };
        date = tomorrow;
    }
    best.map(Into::into)
}

/// The first instant after `after` at which an entry that names `date` fires on it.
fn fire_on(entries: &[Entry], zone: &TimeZone, date: Date, after: Timestamp) -> Option<Timestamp> {
    entries
        .iter()
        .filter(|entry| names_date(entry, date))
        .flat_map(|entry| instants(zone, date, entry))
        .filter(|&instant| instant > after)
        .min()
}

/// Whether the entry's date keys (the day of the month, the month, the weekday; an omitted key is a
/// wildcard) name `date`.
fn names_date(entry: &Entry, date: Date) -> bool {
    let is = |key: Option<u8>, value: i8| key.is_none_or(|key| i8::try_from(key).is_ok_and(|key| key == value));
    is(entry.day, date.day())
        && is(entry.month, date.month())
        // 0 and 7 are both Sunday.
        && entry
            .weekday
            .is_none_or(|weekday| i8::try_from(weekday % 7).is_ok_and(|weekday| weekday == date.weekday().to_sunday_zero_offset()))
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
pub(crate) fn system_zone() -> Result<TimeZone> {
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
