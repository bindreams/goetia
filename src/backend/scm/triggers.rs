//! Compiles a cron schedule into Task Scheduler triggers, or refuses it.
//!
//! The triggers fire at the minutes the schedule names, in local civil time, by Task Scheduler's rule
//! for a time that does not exist. No repetition crosses a local hour boundary (see "The time of day").
//!
//! # Clock changes
//!
//! Measured on Windows Server 2025 with the change on an hour boundary (the knowledge base's
//! "A repetition across a DST change runs twice a minute for part of its span ..." and "Task Scheduler
//! runs a repetition that stays inside one hour across a DST change ..."):
//!
//! - A repetition that spans the change runs on both the civil grid and elapsed time, so some minutes
//!   run twice, and under `IgnoreNew` a doubled run is dropped only while the first is still going.
//!   That is why none spans an hour boundary, where a change is; a change off the hour was not measured.
//! - A repetition inside one hour, or a lone trigger, has `IgnoreNew` run exactly what `Parallel` runs.
//!   An hour a change skips runs nothing that day. An hour it repeats runs in both passes.
//! - Except a trigger due at the instant of the change: a repetition starting on the hour that ends
//!   the skipped hour, or the second pass of a repeated hour, does not run at all (every tick) in that
//!   pass. A repetition at `:05` or a lone trigger at `:30` of the same hours ran. A lone trigger at
//!   `:00` was not measured.
//!
//! A trigger names days (every day, weekdays, days of the month, in some months or all), a start time
//! and optionally a repetition that restarts at its start each day. What the shapes rest on was
//! measured: a day a month lacks is skipped, never moved, but Task Scheduler refuses a trigger
//! whose days no month in its month set has (that schedule is [`Trigger::Never`]); a repetition is
//! one trigger, and a `Duration` includes its last tick; a task holds at most [`TRIGGER_LIMIT`]
//! triggers.
//!
//! # Either-or days
//!
//! A restricted day of the month OR a restricted weekday would be two sets of triggers, one for
//! each, and a date both name would have two triggers on the same minute (there is no AND-NOT).
//! `IgnoreNew` does not keep that to one run: measured, with 450 such tasks due in three minutes,
//! one started twice, the second trigger finding the first instance already over (the knowledge
//! base's "Two triggers at the same instant under IgnoreNew started a task twice in one of 450
//! SYSTEM samples and in none of 450 password-user samples"). So such a schedule is refused, naming
//! the limit. One whose days of the month no month of its month set has (`0 0 30 2 1`) is only its
//! weekdays, and compiles.
//!
//! # The time of day
//!
//! The minutes are split into progressions, longest first (a class like `*/15` is one), then single
//! minutes. Each group costs one trigger per hour the schedule names, none of which crosses an hour
//! boundary:
//!
//! - a progression is one repetition from its first minute to its last, inside the hour;
//! - a lone minute is one trigger without a repetition.
//!
//! So `*/15 * * * *` is 24 triggers, `5,10 * * * *` and `5,10,15 * * * *` too, and `0 9-17 * * *` is
//! nine lone triggers. A schedule of more than [`TRIGGER_LIMIT`] triggers is refused. A repetition ends
//! inside its day, so it never runs into a day the trigger does not name.

use std::collections::BTreeSet;
use std::fmt;

use crate::backend::refuse_day_fields_and;
use crate::error::Error;
use crate::spec::{DaySelection, Id, Schedule, invalid};

/// What a task holds: 48 are accepted and 49 refused.
pub(crate) const TRIGGER_LIMIT: usize = 48;

/// A repetition, in the XML `Interval` and `Duration` as `PT<n>M`. The ticks are the start, then each
/// `every_minutes` up to and including `for_minutes` after it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Repeat {
    pub every_minutes: u32,
    pub for_minutes: u32,
}

/// One Task Scheduler trigger. Each starts at `at` (hour, minute) on the days it names, then repeats.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Trigger {
    Daily {
        every_days: u32,
        at: (u8, u8),
        repeat: Option<Repeat>,
    },
    /// Weekdays 0 to 6, Sunday 0, of every week.
    Weekly {
        weekdays: BTreeSet<u8>,
        at: (u8, u8),
        repeat: Option<Repeat>,
    },
    /// The days of the month in `days` (a month that lacks one skips it), in the months `months`.
    MonthlyDays {
        days: BTreeSet<u8>,
        months: BTreeSet<u8>,
        at: (u8, u8),
        repeat: Option<Repeat>,
    },
    /// Every occurrence of the weekdays in the months `months`.
    MonthlyWeekdays {
        weekdays: BTreeSet<u8>,
        months: BTreeSet<u8>,
        at: (u8, u8),
        repeat: Option<Repeat>,
    },
    /// A schedule that never fires: a one-time trigger at a fixed past start boundary.
    Never,
}

/// One line, for a definition's text: the days, the time, the repetition.
impl fmt::Display for Trigger {
    fn fmt(&self, out: &mut fmt::Formatter<'_>) -> fmt::Result {
        fn list(values: &BTreeSet<u8>) -> String {
            values.iter().map(u8::to_string).collect::<Vec<_>>().join(",")
        }
        let (at, repeat) = match self {
            Trigger::Daily { every_days, at, repeat } => {
                write!(out, "daily every {every_days} day(s)")?;
                (at, repeat)
            }
            Trigger::Weekly { weekdays, at, repeat } => {
                write!(out, "weekly on weekdays {}", list(weekdays))?;
                (at, repeat)
            }
            Trigger::MonthlyDays {
                days,
                months,
                at,
                repeat,
            } => {
                write!(out, "monthly on days {} of months {}", list(days), list(months))?;
                (at, repeat)
            }
            Trigger::MonthlyWeekdays {
                weekdays,
                months,
                at,
                repeat,
            } => {
                write!(out, "monthly on weekdays {} of months {}", list(weekdays), list(months))?;
                (at, repeat)
            }
            Trigger::Never => return write!(out, "never"),
        };
        write!(out, " at {:02}:{:02}", at.0, at.1)?;
        match repeat {
            Some(Repeat {
                every_minutes,
                for_minutes,
            }) => write!(out, " repeating every {every_minutes} min for {for_minutes} min"),
            None => Ok(()),
        }
    }
}

/// The triggers that fire exactly when `schedule` does, with the fewest this compilation finds. A
/// schedule that never fires (no month of its month set has any of its days) is `[Trigger::Never]`:
/// the result is never empty.
///
/// Refused with [`Error::Invalid`] for `id`: a day of the month and a day of the week that must both
/// match, with neither full (Task Scheduler's day forms are either-or); a day of the month and a day
/// of the week of which either may match, when some month has the day (a date both name would start
/// the task from two triggers at one instant: see the module doc); and a schedule that needs more
/// than [`TRIGGER_LIMIT`] triggers, saying how many.
pub(crate) fn compile(schedule: &Schedule, id: &Id) -> Result<Vec<Trigger>, Error> {
    let [minutes, hours, days, months, weekdays] = schedule.fields();
    let dates = match schedule.days() {
        DaySelection::Every => Dates::Every,
        DaySelection::OfMonth => Dates::Days(&days.values),
        DaySelection::OfWeek => Dates::Weekdays(&weekdays.values),
        // Days no month has never fire, so the weekdays alone are the schedule.
        DaySelection::Either if !some_month_has(&days.values, &months.values) => Dates::Weekdays(&weekdays.values),
        DaySelection::Either => return Err(refuse_either_day_fields(id, schedule.text())),
        DaySelection::Both => return Err(refuse_day_fields_and(id, "Task Scheduler", schedule.text())),
    };
    // Task Scheduler refuses days no month has at registration, so such days are no trigger at all.
    if let Dates::Days(days) = &dates
        && !some_month_has(days, &months.values)
    {
        return Ok(vec![Trigger::Never]);
    }

    let times = times(&hours.values, &minutes.values);
    if times.len() > TRIGGER_LIMIT {
        return Err(invalid(
            id,
            &format!(
                "schedule `{}` compiles to {} Task Scheduler triggers, and a task holds at most {TRIGGER_LIMIT}",
                schedule.text(),
                times.len()
            ),
        ));
    }
    Ok(times.iter().map(|time| dates.trigger(&months.values, time)).collect())
}

/// The refusal of a schedule whose either-or day fields would give two triggers on one date.
fn refuse_either_day_fields(id: &Id, text: &str) -> Error {
    invalid(
        id,
        &format!(
            "schedule `{text}` matches a day of the month or a day of the week, and on a date both name Task \
             Scheduler would start the task from two triggers at one instant, which `IgnoreNew` does not reliably \
             make one run; a task cannot hold a schedule whose day fields are both restricted: use `*` for one of \
             the two"
        ),
    )
}

// The days ============================================================================================================

/// The days a trigger names, before its months and its time.
enum Dates<'a> {
    Every,
    Days(&'a BTreeSet<u8>),
    Weekdays(&'a BTreeSet<u8>),
}

impl Dates<'_> {
    fn trigger(&self, months: &BTreeSet<u8>, time: &Time) -> Trigger {
        let Time { at, repeat } = *time;
        let every_month = months.len() == 12;
        match self {
            Dates::Every if every_month => Trigger::Daily {
                every_days: 1,
                at,
                repeat,
            },
            Dates::Every => Trigger::MonthlyDays {
                days: (1..=31).collect(),
                months: months.clone(),
                at,
                repeat,
            },
            Dates::Days(days) => Trigger::MonthlyDays {
                days: (*days).clone(),
                months: months.clone(),
                at,
                repeat,
            },
            Dates::Weekdays(weekdays) if every_month => Trigger::Weekly {
                weekdays: (*weekdays).clone(),
                at,
                repeat,
            },
            Dates::Weekdays(weekdays) => Trigger::MonthlyWeekdays {
                weekdays: (*weekdays).clone(),
                months: months.clone(),
                at,
                repeat,
            },
        }
    }
}

/// The days each month has, February's counted as a leap year's: it has a 29th some years.
const MONTH_DAYS: [u8; 12] = [31, 29, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31];

/// Whether some day of `days` is a day of some month of `months`.
fn some_month_has(days: &BTreeSet<u8>, months: &BTreeSet<u8>) -> bool {
    days.iter()
        .any(|&day| months.iter().any(|&month| day <= MONTH_DAYS[usize::from(month) - 1]))
}

// The time of day =====================================================================================================

/// A trigger's start and repetition.
#[derive(Debug, Clone, Copy)]
struct Time {
    at: (u8, u8),
    repeat: Option<Repeat>,
}

/// The minutes of one trigger's start and repetition, covered together.
struct Group {
    /// Ascending.
    minutes: Vec<u8>,
    shape: Shape,
}

#[derive(Debug, Clone, Copy)]
enum Shape {
    Single,
    /// The minutes have this step.
    Progression(u8),
}

/// A trigger per hour of `hours` for each group: a repetition inside the hour for a progression, a lone
/// trigger for a minute.
fn times(hours: &BTreeSet<u8>, minutes: &BTreeSet<u8>) -> Vec<Time> {
    let mut times = Vec::new();
    for group in groups(minutes) {
        let (first, last) = (group.minutes[0], group.minutes[group.minutes.len() - 1]);
        let repeat = match group.shape {
            Shape::Single => None,
            Shape::Progression(step) => Some(Repeat {
                every_minutes: u32::from(step),
                for_minutes: u32::from(last - first),
            }),
        };
        times.extend(hours.iter().map(|&hour| Time {
            at: (hour, first),
            repeat,
        }));
    }
    times
}

/// `minutes` split into progressions, longest first, then single minutes.
fn groups(minutes: &BTreeSet<u8>) -> Vec<Group> {
    let mut left = minutes.clone();
    let mut groups = Vec::new();
    while let Some(&first) = left.first() {
        let mut longest = vec![first];
        for step in 1..=59 - first {
            let run: Vec<u8> = (first..60)
                .step_by(usize::from(step))
                .take_while(|minute| left.contains(minute))
                .collect();
            if run.len() > longest.len() {
                longest = run;
            }
        }
        for minute in &longest {
            left.remove(minute);
        }
        let shape = match longest.as_slice() {
            [only] => {
                debug_assert_eq!(*only, first);
                Shape::Single
            }
            [a, b, ..] => Shape::Progression(b - a),
            [] => unreachable!("a progression starts at its first minute"),
        };
        groups.push(Group {
            minutes: longest,
            shape,
        });
    }
    groups
}

#[cfg(test)]
#[path = "triggers_tests.rs"]
mod triggers_tests;
