//! The next fire of a schedule in a time zone, for a backend whose service manager cannot tell it
//! (launchd takes calendar intervals and reports no next fire).
//!
//! The model: a minute fires when its reading in the zone (minute, hour, day, month, weekday) is
//! one the schedule names. A local time the zone skips (a spring-forward gap) never happens, so it
//! never fires; one it repeats (a fall-back fold) happens twice and fires at both passes.

use std::path::Path;
use std::time::SystemTime;

use jiff::Timestamp;
use jiff::civil::Date;
use jiff::tz::{AmbiguousOffset, TimeZone};

use super::{CronField, DayRule, Schedule};
use crate::error::{Error, Result};

/// Days in the Gregorian cycle: dates, and the weekdays on them, repeat exactly after it.
const GREGORIAN_CYCLE_DAYS: u32 = 146_097;

/// The first instant strictly after `after`, on a whole minute, whose reading in `zone` the
/// schedule names.
///
/// `None` when no day of the Gregorian cycle names it (`0 0 30 2 *`), or when `after` or the answer
/// lies outside the years jiff represents (to 9999). A caller treats `None` as "no next run known",
/// not as "never".
pub(crate) fn next_fire(schedule: &Schedule, after: SystemTime, zone: &TimeZone) -> Option<SystemTime> {
    let after = Timestamp::try_from(after).ok()?;
    // From the day before `after`'s own date: a fall-back across midnight reads that date again.
    let mut date = after.to_zoned(zone.clone()).date().yesterday().ok()?;
    // Then `after`'s own date and a cycle's worth after it, whose first date a fire earlier that day
    // may not satisfy.
    for _ in 0..=GREGORIAN_CYCLE_DAYS + 1 {
        if let Some(fire) = fire_on(schedule, zone, date, after) {
            // The next date can read an earlier instant than this one (a fall-back across midnight),
            // and no date after it can.
            let tomorrow = date
                .tomorrow()
                .ok()
                .and_then(|date| fire_on(schedule, zone, date, after));
            return Some(tomorrow.map_or(fire, |tomorrow| fire.min(tomorrow)).into());
        }
        date = date.tomorrow().ok()?;
    }
    None
}

/// The first instant after `after` at which `zone` reads a minute the schedule names on `date`.
fn fire_on(schedule: &Schedule, zone: &TimeZone, date: Date, after: Timestamp) -> Option<Timestamp> {
    let [minutes, hours, days, months, weekdays] = schedule.fields();
    if !names_date(schedule, date, [days, months, weekdays]) {
        return None;
    }
    minutes
        .values
        .iter()
        .flat_map(|&minute| hours.values.iter().map(move |&hour| (hour, minute)))
        .flat_map(|(hour, minute)| instants(zone, date, hour, minute))
        .filter(|&instant| instant > after)
        .min()
}

/// Whether the schedule's day-of-month, month and day-of-week fields name `date`. Not shared with
/// `testing::cron_matches`: that is the oracle the tests check this walk against, and it must stay
/// independent of the code it checks.
fn names_date(schedule: &Schedule, date: Date, [days, months, weekdays]: [&CronField; 3]) -> bool {
    let has = |field: &CronField, value: i8| u8::try_from(value).is_ok_and(|value| field.values.contains(&value));
    let day = has(days, date.day());
    let weekday = has(weekdays, date.weekday().to_sunday_zero_offset());
    let day_ok = match schedule.day_rule() {
        DayRule::And => day && weekday,
        DayRule::Or => day || weekday,
    };
    day_ok && has(months, date.month())
}

/// The instants at which `zone` reads `hour:minute` on `date`: none in a gap, two in a fold.
fn instants(zone: &TimeZone, date: Date, hour: u8, minute: u8) -> impl Iterator<Item = Timestamp> {
    let civil = date.at(hour as i8, minute as i8, 0, 0);
    let offsets = match zone.to_ambiguous_timestamp(civil).offset() {
        AmbiguousOffset::Unambiguous { offset } => [Some(offset), None],
        AmbiguousOffset::Fold { before, after } => [Some(before), Some(after)],
        AmbiguousOffset::Gap { .. } => [None, None],
    };
    offsets
        .into_iter()
        .flatten()
        .filter_map(move |offset| offset.to_timestamp(civil).ok())
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
