//! Compiles a cron schedule into Task Scheduler triggers, or refuses it.
//!
//! The triggers fire at the minutes the schedule names and no others, in local civil time. A
//! trigger names days (every day, weekdays, days of the month, in some months or all), a start time
//! and optionally a repetition that restarts at its start each day. What the shapes rest on was
//! measured: a day a month lacks is skipped, never moved, but Task Scheduler refuses a trigger
//! whose days no month in its month set has (that schedule is [`Trigger::Never`]); a repetition is
//! one trigger, and a `Duration` includes its last tick; a task holds at most [`TRIGGER_LIMIT`]
//! triggers.
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

/// The triggers that fire exactly when `schedule` does, with the fewest this compilation finds. A
/// schedule that never fires (no month of its month set has any of its days) is `[Trigger::Never]`:
/// the result is never empty.
///
/// Refused with [`Error::Invalid`] for `id`: a day of the month and a day of the week that must both
/// match, with neither full (Task Scheduler's day forms are either-or), and a schedule that needs
/// more than [`TRIGGER_LIMIT`] triggers, saying how many.
pub(crate) fn compile(schedule: &Schedule, id: &Id) -> Result<Vec<Trigger>, Error> {
    let [minutes, hours, days, months, weekdays] = schedule.fields();
    let dates = match schedule.days() {
        DaySelection::Every => vec![Dates::Every],
        DaySelection::OfMonth => vec![Dates::Days(&days.values)],
        DaySelection::OfWeek => vec![Dates::Weekdays(&weekdays.values)],
        DaySelection::Either => vec![Dates::Days(&days.values), Dates::Weekdays(&weekdays.values)],
        DaySelection::Both => return Err(refuse_day_fields_and(id, "Task Scheduler", schedule.text())),
    };
    // Task Scheduler refuses days no month has at registration, so such days are no trigger at all.
    let dates: Vec<Dates> = dates
        .into_iter()
        .filter(|dates| match dates {
            Dates::Days(days) => some_month_has(days, &months.values),
            Dates::Every | Dates::Weekdays(_) => true,
        })
        .collect();
    if dates.is_empty() {
        return Ok(vec![Trigger::Never]);
    }

    let times = times(&hours.values, &minutes.values);
    let count = dates.len() * times.len();
    if count > TRIGGER_LIMIT {
        return Err(invalid(
            id,
            &format!(
                "schedule `{}` needs {count} Task Scheduler triggers, and a task holds at most {TRIGGER_LIMIT}",
                schedule.text()
            ),
        ));
    }
    Ok(dates
        .iter()
        .flat_map(|dates| times.iter().map(|time| dates.trigger(&months.values, time)))
        .collect())
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
