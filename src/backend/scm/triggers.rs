//! Compiles a cron schedule into Task Scheduler triggers, or refuses it.
//!
//! The triggers fire at the minutes the schedule names, in local civil time, by Task Scheduler's rule for
//! a time that does not exist. A repetition's ticks are civil too: across a change of clocks
//! `GetRunTimes` lists the same grid each day, listing an hour that repeats once and the ticks of an hour
//! a change skips an hour earlier, so those minutes are listed twice (the knowledge base's "GetRunTimes
//! lists a repetition across a DST change in local civil time ..."). What Task Scheduler fires at those
//! minutes is not measured.
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
//! Cron's minute grid restarts each hour and Task Scheduler's each day, so minutes are one
//! repetition across hours only when they are a whole residue class of 60 (`*/15`, `0,30`, `*`).
//! The minutes are split into such classes, then (greedily, longest first) into other
//! progressions, then single minutes. The hours of each group are covered by the fewest triggers
//! (found exactly), each one of:
//!
//! - a class over a run of consecutive hours, repeating through them;
//! - another progression within one hour, repeating through it;
//! - one minute of the group over evenly spaced hours, repeating through them.
//!
//! A repetition ends inside its day, so it never runs into a day the trigger does not name.
//! `*/7 * * * *` is not one repetition: it costs a trigger per minute it names.

use std::collections::{BTreeSet, HashMap};
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
    /// Every minute of the hour with this step (a divisor of 60 below 60): the grid carries on
    /// across hours.
    Class(u8),
}

/// The steps of a minute class, the divisors of 60 below 60, smallest (largest class) first.
const CLASS_STEPS: [u8; 11] = [1, 2, 3, 4, 5, 6, 10, 12, 15, 20, 30];

fn times(hours: &BTreeSet<u8>, minutes: &BTreeSet<u8>) -> Vec<Time> {
    let mut times = Vec::new();
    for group in groups(minutes) {
        let (first, last) = (group.minutes[0], group.minutes[group.minutes.len() - 1]);
        for span in spans(hours, &group) {
            match span {
                Span::Run { first: from, last: to } => {
                    let (Shape::Progression(step) | Shape::Class(step)) = group.shape else {
                        unreachable!("a lone minute is never covered by a run of hours");
                    };
                    times.push(Time {
                        at: (from, first),
                        repeat: Some(Repeat {
                            every_minutes: u32::from(step),
                            for_minutes: u32::from(to - from) * 60 + u32::from(last - first),
                        }),
                    });
                }
                Span::Hours {
                    first: from,
                    step,
                    last: to,
                } => {
                    for &minute in &group.minutes {
                        times.push(Time {
                            at: (from, minute),
                            repeat: (to > from).then(|| Repeat {
                                every_minutes: u32::from(step) * 60,
                                for_minutes: u32::from(to - from) * 60,
                            }),
                        });
                    }
                }
            }
        }
    }
    times
}

/// `minutes` split into classes, then progressions, then single minutes.
fn groups(minutes: &BTreeSet<u8>) -> Vec<Group> {
    let mut left = minutes.clone();
    let mut groups = Vec::new();
    for step in CLASS_STEPS {
        for first in 0..step {
            let class: Vec<u8> = (first..60).step_by(usize::from(step)).collect();
            if class.iter().all(|minute| left.contains(minute)) {
                for minute in &class {
                    left.remove(minute);
                }
                groups.push(Group {
                    minutes: class,
                    shape: Shape::Class(step),
                });
            }
        }
    }
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

/// How one group's hours are covered.
#[derive(Debug, Clone, Copy)]
enum Span {
    /// Hours `first` to `last` in a row, all the group's minutes in one trigger.
    Run { first: u8, last: u8 },
    /// Every `step` hours from `first` to `last`, a trigger for each minute of the group.
    Hours { first: u8, step: u8, last: u8 },
}

/// The spans that cover `hours` for `group` with the fewest triggers.
fn spans(hours: &BTreeSet<u8>, group: &Group) -> Vec<Span> {
    let all = hours.iter().fold(0u32, |mask, &hour| mask | 1 << hour);
    let mut memo = HashMap::new();
    cheapest(all, group, &mut memo);
    let mut spans = Vec::new();
    let mut rest = all;
    while rest != 0 {
        let (_, span, taken) = memo[&rest];
        spans.push(span);
        rest &= !taken;
    }
    spans
}

/// The fewest triggers that cover the hours of `mask`, the best first span of each remaining set
/// kept in `memo` (the cost, the span, the hours it takes).
fn cheapest(mask: u32, group: &Group, memo: &mut HashMap<u32, (usize, Span, u32)>) -> usize {
    if mask == 0 {
        return 0;
    }
    if let Some(&(cost, ..)) = memo.get(&mask) {
        return cost;
    }
    let mut best: Option<(usize, Span, u32)> = None;
    for (span, taken, cost) in options(mask, group) {
        let total = cost + cheapest(mask & !taken, group, memo);
        if best.is_none_or(|(least, ..)| total < least) {
            best = Some((total, span, taken));
        }
    }
    let best = best.expect("an hour is always a span of its own");
    memo.insert(mask, best);
    best.0
}

/// The spans that start at the first hour of `mask`: each with the hours it takes and its triggers.
fn options(mask: u32, group: &Group) -> Vec<(Span, u32, usize)> {
    let first = u8::try_from(mask.trailing_zeros()).expect("an hour mask has 24 bits");
    let has = |hour: u8| hour < 24 && mask & 1 << hour != 0;
    let minutes = group.minutes.len();
    let mut options = Vec::new();
    match group.shape {
        Shape::Single => options.push((
            Span::Hours {
                first,
                step: 1,
                last: first,
            },
            1 << first,
            1,
        )),
        Shape::Progression(_) => options.push((Span::Run { first, last: first }, 1 << first, 1)),
        Shape::Class(_) => {
            let mut taken = 0;
            for last in first..24 {
                if !has(last) {
                    break;
                }
                taken |= 1 << last;
                options.push((Span::Run { first, last }, taken, 1));
            }
        }
    }
    for step in 1..24 {
        let mut taken = 1 << first;
        let mut last = first;
        let mut hours = 1;
        while has(last + step) {
            last += step;
            hours += 1;
            taken |= 1 << last;
            // A trigger per minute for fewer hours than minutes is never fewer than a trigger per hour.
            if minutes == 1 || hours > minutes {
                options.push((Span::Hours { first, step, last }, taken, minutes));
            }
        }
    }
    options
}

#[cfg(test)]
#[path = "triggers_tests.rs"]
mod triggers_tests;
