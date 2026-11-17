//! Compiles a cron schedule into launchd `StartCalendarInterval` entries, or refuses it.
//!
//! launchd reads the dicts of the array as a union. Within a dict a key omitted above the most
//! significant key it gives is a wildcard, and one omitted below or between is its smallest value.
//! Significance is month, then day or weekday, then hour, then minute. So a field is left out only
//! when it and every field above it is full, and a full field below a given one is written out:
//! `* 12 * * *` is sixty entries, not one. The empty dict fires every minute.
//!
//! Three more facts shape the result, each measured: a day a month lacks is registered as another
//! date; a dict with a weekday ignores its month; and launchd has no year. A day of the month that
//! some months lack is therefore written with a month that has it, a weekday under a restricted
//! month is refused, and so is a schedule that has to name 29 February.

use crate::backend::refuse_day_fields_and;
use crate::error::Error;
use crate::spec::{CronField, DaySelection, Id, Schedule, invalid};

/// One `StartCalendarInterval` dict; a `None` field is a key the dict omits. The fields are in the
/// order the dict renders its keys.
///
/// Of every entry [`entries`] returns, the omitted fields are a prefix of the significance order
/// `month`, `day` or `weekday`, `hour`, `minute`, and an entry with a `weekday` has no `month` and
/// no `day`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Entry {
    pub minute: Option<u8>,
    pub hour: Option<u8>,
    pub day: Option<u8>,
    pub weekday: Option<u8>,
    pub month: Option<u8>,
}

/// The days each month has in every year; February's 29th is what launchd cannot hold.
const MONTH_DAYS: [u8; 12] = [31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31];

const FEBRUARY: u8 = 2;

/// The entries that fire exactly when `schedule` does: all day entries (no month first, then by
/// month), then all weekday entries; within each, by hour, then minute. An empty `Vec` is a schedule
/// with no date (`0 0 30 2 *`): its timer has no `StartCalendarInterval`, which is not an empty dict.
///
/// Refused with [`Error::Invalid`] for `id`: a day of the month and a day of the week that must both
/// match, with neither full (launchd only ORs them); a weekday under a restricted month; and a
/// schedule that must name 29 February. The number of entries has no limit.
pub(crate) fn entries(schedule: &Schedule, id: &Id) -> Result<Vec<Entry>, Error> {
    let [minutes, hours, days, months, weekdays] = schedule.fields();
    let text = schedule.text();
    let months_full = months.values.len() == 12;
    let refuse = |message: String| invalid(id, &message);

    let (use_days, use_weekdays, every_day) = match schedule.days() {
        DaySelection::Every => (false, false, true),
        DaySelection::OfWeek => (false, true, false),
        DaySelection::OfMonth => (true, false, false),
        DaySelection::Either => (true, true, false),
        DaySelection::Both => return Err(refuse_day_fields_and(id, "launchd", text)),
    };

    if use_weekdays && !months_full {
        return Err(refuse(format!(
            "schedule day-of-week field: launchd ignores the month in an entry that names a weekday, so it cannot \
             limit a weekday schedule to some months, and `{text}` does"
        )));
    }
    let names_february = months.values.contains(&FEBRUARY);
    let february_29 = |field: &str| {
        refuse(format!(
            "schedule {field} field: launchd has no year, so it cannot run on 29 February only in leap years, and \
             `{text}` includes it"
        ))
    };
    if use_days && days.values.contains(&29) && names_february {
        return Err(february_29("day-of-month"));
    }
    if every_day && !months_full && names_february {
        return Err(february_29("month"));
    }

    let mut picks: Vec<Entry> = Vec::new();
    if every_day {
        if months_full {
            picks.push(Entry::default());
        } else {
            for &month in &months.values {
                picks.extend((1..=MONTH_DAYS[usize::from(month) - 1]).map(|day| pair(day, month)));
            }
        }
    }
    if use_days {
        for &day in &days.values {
            if day <= 28 && months_full {
                picks.push(Entry {
                    day: Some(day),
                    ..Entry::default()
                });
            } else {
                let exists = |month: &u8| day <= MONTH_DAYS[usize::from(*month) - 1];
                picks.extend(
                    months
                        .values
                        .iter()
                        .filter(|month| exists(month))
                        .map(|&month| pair(day, month)),
                );
            }
        }
        picks.sort_by_key(|pick| (pick.month, pick.day));
    }
    if use_weekdays {
        picks.extend(weekdays.values.iter().map(|&weekday| Entry {
            weekday: Some(weekday),
            ..Entry::default()
        }));
    }

    let mut compiled = Vec::new();
    for pick in picks {
        // Only a pick that names no date leaves the time keys omittable.
        let free = pick == Entry::default();
        let hour_omitted = free && is_full(hours, 24);
        let minute_omitted = hour_omitted && is_full(minutes, 60);
        let hour_keys = keys(hours, hour_omitted);
        for hour in hour_keys {
            for minute in keys(minutes, minute_omitted) {
                compiled.push(Entry {
                    minute,
                    hour,
                    ..pick.clone()
                });
            }
        }
    }
    Ok(compiled)
}

fn is_full(field: &CronField, count: usize) -> bool {
    field.values.len() == count
}

fn pair(day: u8, month: u8) -> Entry {
    Entry {
        day: Some(day),
        month: Some(month),
        ..Entry::default()
    }
}

/// The values of `field` as keys: one omitted key, or every value.
fn keys(field: &CronField, omitted: bool) -> Vec<Option<u8>> {
    if omitted {
        vec![None]
    } else {
        field.values.iter().map(|&value| Some(value)).collect()
    }
}

#[cfg(test)]
#[path = "calendar_tests.rs"]
mod calendar_tests;
