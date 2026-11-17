use super::*;
use crate::spec::HashSeed;
use crate::testing::{FIRING_SCHEDULES, cron_matches};

const ID: &str = "nightly";

fn id() -> Id {
    Id::try_from(ID).unwrap()
}

fn schedule(text: &str) -> Schedule {
    let id = id();
    let seed = HashSeed {
        hostname: "testing",
        id: &id,
    };
    Schedule::parse(text, &seed).unwrap_or_else(|e| panic!("{text:?}: {e}"))
}

fn triggers(text: &str) -> Vec<Trigger> {
    compile(&schedule(text), &id()).unwrap_or_else(|e| panic!("{text:?} was refused: {e}"))
}

/// The text of the refusal of `text`.
fn refusal(text: &str) -> String {
    match compile(&schedule(text), &id()) {
        Err(error @ Error::Invalid { .. }) => error.to_string(),
        Err(other) => panic!("{text:?}: expected Error::Invalid, got {other:?}"),
        Ok(compiled) => panic!("{text:?} compiled to {} triggers, expected a refusal", compiled.len()),
    }
}

fn set<const N: usize>(values: [u8; N]) -> BTreeSet<u8> {
    values.into_iter().collect()
}

fn every_month() -> BTreeSet<u8> {
    (1..=12).collect()
}

fn repeat(every_minutes: u32, for_minutes: u32) -> Option<Repeat> {
    Some(Repeat {
        every_minutes,
        for_minutes,
    })
}

// A model of what Task Scheduler does with a trigger ==================================================================

/// One civil day, 2027-01-01 to 2028-12-31 (2028 is a leap year). Local civil time: no zone, no DST.
struct Day {
    /// Days since 2027-01-01.
    index: u32,
    month: u8,
    day: u8,
    /// Sunday is 0.
    weekday: u8,
}

fn month_length(year: u32, month: u8) -> u8 {
    match month {
        2 if year.is_multiple_of(4) => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    }
}

fn two_years() -> Vec<Day> {
    let mut days = Vec::new();
    // 2027-01-01 is a Friday.
    let mut weekday = 5;
    for year in [2027, 2028] {
        for month in 1..=12 {
            for day in 1..=month_length(year, month) {
                days.push(Day {
                    index: u32::try_from(days.len()).unwrap(),
                    month,
                    day,
                    weekday,
                });
                weekday = (weekday + 1) % 7;
            }
        }
    }
    days
}

/// M2: a day a month lacks is skipped, never moved; a trigger's weekdays and days of the month are
/// every one of them (weeks 1 to 4 and `Last` for a month's weekdays); a daily trigger counts days
/// from its start boundary, here the first day.
fn on_day(trigger: &Trigger, day: &Day) -> bool {
    match trigger {
        Trigger::Daily { every_days, .. } => day.index.is_multiple_of(*every_days),
        Trigger::Weekly { weekdays, .. } => weekdays.contains(&day.weekday),
        Trigger::MonthlyDays { days, months, .. } => months.contains(&day.month) && days.contains(&day.day),
        Trigger::MonthlyWeekdays { weekdays, months, .. } => {
            months.contains(&day.month) && weekdays.contains(&day.weekday)
        }
        Trigger::Never => false,
    }
}

/// The minutes of the day a firing day's trigger runs at: its start, then each repetition tick up to
/// and including the end of the duration. M2: the grid restarts each day, and the ticks must stay in
/// the day, or they would run on into a day the trigger's days do not name.
fn minutes_of_day(trigger: &Trigger) -> Vec<u32> {
    let (at, repeat) = match trigger {
        Trigger::Daily { at, repeat, .. }
        | Trigger::Weekly { at, repeat, .. }
        | Trigger::MonthlyDays { at, repeat, .. }
        | Trigger::MonthlyWeekdays { at, repeat, .. } => (*at, *repeat),
        Trigger::Never => return Vec::new(),
    };
    let start = u32::from(at.0) * 60 + u32::from(at.1);
    let times: Vec<u32> = match repeat {
        None => vec![start],
        Some(Repeat {
            every_minutes,
            for_minutes,
        }) => {
            assert!(every_minutes >= 1, "{trigger:?}: a repetition of 0 minutes");
            (0..=for_minutes / every_minutes)
                .map(|tick| start + tick * every_minutes)
                .collect()
        }
    };
    assert!(
        times.last().is_some_and(|&last| last < 24 * 60),
        "{trigger:?} runs on past midnight"
    );
    times
}

/// Every minute of the two years where the model's run times and `cron_matches` disagree, as text.
fn disagreements(text: &str) -> Vec<String> {
    let schedule = schedule(text);
    let compiled = compile(&schedule, &id()).unwrap_or_else(|e| panic!("{text:?} was refused: {e}"));
    let ticks: Vec<Vec<u32>> = compiled.iter().map(minutes_of_day).collect();
    let mut wrong = Vec::new();
    for day in &two_years() {
        let mut fired = BTreeSet::<u32>::new();
        for (trigger, minutes) in compiled.iter().zip(&ticks) {
            if on_day(trigger, day) {
                fired.extend(minutes);
            }
        }
        for minute in 0..24 * 60u32 {
            let named = cron_matches(
                &schedule,
                u8::try_from(minute % 60).unwrap(),
                u8::try_from(minute / 60).unwrap(),
                day.day,
                day.month,
                day.weekday,
            );
            if named != fired.contains(&minute) {
                wrong.push(format!(
                    "{text:?}: day {} (month {}, day {}, weekday {}) at {:02}:{:02}: cron says {named}, triggers say {}",
                    day.index,
                    day.month,
                    day.day,
                    day.weekday,
                    minute / 60,
                    minute % 60,
                    !named
                ));
            }
        }
    }
    wrong
}

fn assert_fires_as_cron_says(texts: &[&str]) {
    for text in texts {
        let wrong = disagreements(text);
        assert!(
            wrong.is_empty(),
            "{} disagreements, the first: {}",
            wrong.len(),
            wrong[0]
        );
    }
}

// The set of run times ================================================================================================

#[skuld::test]
fn compiled_triggers_fire_exactly_when_the_cron_says() {
    assert_fires_as_cron_says(FIRING_SCHEDULES[0]);
}

#[skuld::test]
fn a_time_with_no_single_progression_still_fires_exactly_when_the_cron_says() {
    assert_fires_as_cron_says(FIRING_SCHEDULES[1]);
}

#[skuld::test]
fn a_fifth_occurrence_of_a_weekday_runs_through_the_last_week() {
    assert_fires_as_cron_says(FIRING_SCHEDULES[2]);
}

// Shapes ==============================================================================================================

/// A daily trigger at `hour`:`minute`, repeating every `every` minutes for `for_minutes` after it.
fn daily(hour: u8, minute: u8, repeat: Option<Repeat>) -> Trigger {
    Trigger::Daily {
        every_days: 1,
        at: (hour, minute),
        repeat,
    }
}

#[skuld::test]
fn one_time_of_day_is_one_trigger() {
    assert_eq!(triggers("30 3 * * *"), [daily(3, 30, None)]);
}

/// Task Scheduler runs a repetition across a change of clocks on both the civil grid and elapsed time
/// (module doc), so none may cross a local hour boundary: a minute class over consecutive hours is one
/// repetition inside each hour.
#[skuld::test]
fn a_minute_class_over_hours_is_one_repetition_per_hour() {
    assert_eq!(
        triggers("*/15 * * * *"),
        (0..24).map(|h| daily(h, 0, repeat(15, 45))).collect::<Vec<_>>()
    );
    // The every-minute schedule: 24 triggers, one repetition of 60 ticks in each hour.
    assert_eq!(
        triggers("* * * * *"),
        (0..24).map(|h| daily(h, 0, repeat(1, 59))).collect::<Vec<_>>()
    );
    // The last tick of the hour's repetition is its :45.
    assert_eq!(
        triggers("*/15 9-17 * * 1-5"),
        (9..=17)
            .map(|hour| Trigger::Weekly {
                weekdays: set([1, 2, 3, 4, 5]),
                at: (hour, 0),
                repeat: repeat(15, 45)
            })
            .collect::<Vec<_>>()
    );
    // The two minutes of a half-hour step, every third hour.
    assert_eq!(
        triggers("5,35 */3 * * *"),
        (0..24)
            .step_by(3)
            .map(|h| daily(h, 5, repeat(30, 30)))
            .collect::<Vec<_>>()
    );
}

#[skuld::test]
fn a_progression_is_one_repetition_inside_each_hour() {
    assert_eq!(
        triggers("5,17 * * * *"),
        (0..24).map(|h| daily(h, 5, repeat(12, 12))).collect::<Vec<_>>()
    );
    assert_eq!(
        triggers("5,10 * * * *"),
        (0..24).map(|h| daily(h, 5, repeat(5, 5))).collect::<Vec<_>>()
    );
    // Three minutes of one progression are still one repetition per hour: 24 triggers, accepted.
    assert_eq!(
        triggers("5,10,15 * * * *"),
        (0..24).map(|h| daily(h, 5, repeat(5, 10))).collect::<Vec<_>>()
    );
    // A step that does not divide 60 restarts each hour, on the minutes' own grid.
    assert_eq!(
        triggers("*/7 * * * *"),
        (0..24).map(|h| daily(h, 0, repeat(7, 56))).collect::<Vec<_>>()
    );
    assert_eq!(triggers("*/7 3 * * *"), [daily(3, 0, repeat(7, 56))]);
}

#[skuld::test]
fn a_minute_over_hours_is_a_lone_trigger_per_hour() {
    assert_eq!(
        triggers("0 * * * *"),
        (0..24).map(|h| daily(h, 0, None)).collect::<Vec<_>>()
    );
    assert_eq!(
        triggers("30 */2 * * *"),
        (0..24).step_by(2).map(|h| daily(h, 30, None)).collect::<Vec<_>>()
    );
    assert_eq!(
        triggers("0 9-17 * * 1-5"),
        (9..=17)
            .map(|hour| Trigger::Weekly {
                weekdays: set([1, 2, 3, 4, 5]),
                at: (hour, 0),
                repeat: None
            })
            .collect::<Vec<_>>()
    );
}

#[skuld::test]
fn no_repetition_crosses_a_local_hour_boundary() {
    for text in FIRING_SCHEDULES.iter().flat_map(|texts| texts.iter()) {
        for trigger in triggers(text) {
            if let Trigger::Daily {
                at,
                repeat: Some(repeat),
                ..
            }
            | Trigger::Weekly {
                at,
                repeat: Some(repeat),
                ..
            }
            | Trigger::MonthlyDays {
                at,
                repeat: Some(repeat),
                ..
            }
            | Trigger::MonthlyWeekdays {
                at,
                repeat: Some(repeat),
                ..
            } = &trigger
            {
                assert!(
                    u32::from(at.1) + repeat.for_minutes < 60,
                    "{text:?}: {trigger} crosses the hour"
                );
            }
        }
    }
}

#[skuld::test]
fn more_than_48_triggers_is_refused_naming_the_limit() {
    // Two groups of minutes in every hour: 48 triggers, the cap, which is accepted.
    assert_eq!(triggers("5,17,40 * * * *").len(), TRIGGER_LIMIT);
    // Three groups make 72.
    let text = "1,2,4,8,16 * * * *";
    assert_eq!(
        refusal(text),
        format!("daemon `{ID}`: schedule `{text}` compiles to 72 Task Scheduler triggers, and a task holds at most 48")
    );
}

#[skuld::test]
fn an_and_of_two_restricted_day_fields_is_refused() {
    let text = "0 0 */2 * 1";
    assert_eq!(
        refusal(text),
        format!(
            "daemon `{ID}`: Task Scheduler matches a day of the month and a day of the week as either-or, so it \
             cannot hold `{text}`: one of its day fields starts with `*` (both must match) while both day fields \
             are restricted; use `*` for one of the two"
        )
    );
}

#[skuld::test]
fn an_either_or_of_two_restricted_day_fields_is_refused_naming_the_limit() {
    // A date both name would start the task from two triggers at one instant, which `IgnoreNew` has
    // been measured not to make one run.
    for text in [
        "0 0 1 * 1",
        "30 3 1,15 * 1-5",
        "0 0 29-31 * 0",
        "0 0 1 3-5 1",
        "*/15 9-17 13 * 5",
    ] {
        assert_eq!(
            refusal(text),
            format!(
                "daemon `{ID}`: schedule `{text}` matches a day of the month or a day of the week, and on a date \
                 both name Task Scheduler would start the task from two triggers at one instant, which `IgnoreNew` \
                 does not reliably make one run; a task cannot hold a schedule whose day fields are both restricted: \
                 use `*` for one of the two"
            ),
            "{text}"
        );
    }
    // Either field alone, and either with the other full, compile.
    assert_eq!(triggers("0 0 1 * *").len(), 1);
    assert_eq!(triggers("0 0 * * 1").len(), 1);
    // Days no month of the month set has never fire: the weekdays are all the schedule, and there is
    // no second trigger to coincide with.
    assert_eq!(
        triggers("0 0 30 2 1"),
        [Trigger::MonthlyWeekdays {
            weekdays: set([1]),
            months: set([2]),
            at: (0, 0),
            repeat: None
        }]
    );
}

#[skuld::test]
fn a_date_no_month_has_is_never() {
    assert_eq!(triggers("0 0 30 2 *"), [Trigger::Never]);
    assert_eq!(triggers("0 0 31 4,6 *"), [Trigger::Never]);
    // No run time in the model over two years, the same as cron.
    assert_fires_as_cron_says(&["0 0 30 2 *", "0 0 31 4,6 *"]);
}

#[skuld::test]
fn a_date_none_of_the_day_fields_has_is_the_weekday_part_alone() {
    // An OR: no 30 February, so only the Mondays are left.
    assert_eq!(
        triggers("0 0 30 2 1"),
        [Trigger::MonthlyWeekdays {
            weekdays: set([1]),
            months: set([2]),
            at: (0, 0),
            repeat: None
        }]
    );
    assert_fires_as_cron_says(&["0 0 30 2 1"]);
}

#[skuld::test]
fn a_day_some_month_has_is_a_monthly_trigger() {
    assert_eq!(
        triggers("0 0 30 2,4 *"),
        [Trigger::MonthlyDays {
            days: set([30]),
            months: set([2, 4]),
            at: (0, 0),
            repeat: None
        }]
    );
    // 29 February exists in a leap year.
    assert_ne!(triggers("0 0 29 2 *"), [Trigger::Never]);
    // A day the month has, among days it lacks.
    assert_ne!(triggers("0 0 30,31 2,4 *"), [Trigger::Never]);
}

/// Weeks 1 to 4 and `Last` over several weekdays are listed twice by `GetRunTimes` on a day that is a
/// weekday's fourth and last occurrence, but the task fires once there (Task 3 Step 0, measured), so
/// the shape is accepted.
#[skuld::test]
fn weeks_one_to_four_plus_last_on_several_weekdays_is_one_trigger() {
    assert_eq!(
        triggers("0 3 * 3-5 1-5"),
        [Trigger::MonthlyWeekdays {
            weekdays: set([1, 2, 3, 4, 5]),
            months: set([3, 4, 5]),
            at: (3, 0),
            repeat: None
        }]
    );
    assert_fires_as_cron_says(&["0 3 * 3-5 1-5"]);
}

#[skuld::test]
fn the_day_forms() {
    let months = every_month();
    assert_eq!(
        triggers("0 3 1,15 * *"),
        [Trigger::MonthlyDays {
            days: set([1, 15]),
            months: months.clone(),
            at: (3, 0),
            repeat: None
        }]
    );
    assert_eq!(
        triggers("0 3 * * 0,6"),
        [Trigger::Weekly {
            weekdays: set([0, 6]),
            at: (3, 0),
            repeat: None
        }]
    );
    // Some months, every day: the days of the month, not a daily trigger.
    assert_eq!(
        triggers("0 3 * 3-5 *"),
        [Trigger::MonthlyDays {
            days: (1..=31).collect(),
            months: set([3, 4, 5]),
            at: (3, 0),
            repeat: None
        }]
    );
    assert_eq!(
        triggers("0 0 * 6 1"),
        [Trigger::MonthlyWeekdays {
            weekdays: set([1]),
            months: set([6]),
            at: (0, 0),
            repeat: None
        }]
    );
}
