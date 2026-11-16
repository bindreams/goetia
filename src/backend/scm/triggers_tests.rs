use super::*;
use crate::spec::HashSeed;
use crate::testing::cron_matches;

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
    assert_fires_as_cron_says(&[
        "30 3 * * *",
        "H 3 * * *",
        "0 9-17 * * 1-5",
        "*/15 9-17 * * 1-5",
        "*/7 * * * *",
        "0 0 1 * 1",
        "0 0 */2 * *",
        "0 0 29-31 * *",
        "0 0 * 6 1",
        "0 3 * 3-5 *",
        "* * * * *",
        "5,35 */3 * * *",
    ]);
}

#[skuld::test]
fn a_time_with_no_single_progression_still_fires_exactly_when_the_cron_says() {
    assert_fires_as_cron_says(&[
        // A class of minutes over two runs of hours, a lone minute, and hours in no run.
        "0,15,30,45 9-11,14-16 * * *",
        "1,2,4,7 0,1,3,5,7,9,11 * * *",
        "0,15,30,45,7 9-17 * * 1-5",
        // A step that does not divide 60, in one hour and in all of them.
        "*/7 3 * * *",
        "*/7 9-17 * * 1-5",
        "0,10,20 22,23,0 * * *",
        "17 */5 * * *",
        "0 * * * *",
        "0 0-4,7,9-12,14,17,20-23 * * 1",
        "0,1 * * * *",
        "0 0,1,2,3,5,6,8,10,11,13,14,16,17,19,21,22 * * *",
        "0,1 0,1,2,3,5,6,8,10,11,13,14,16,17,19,21,22 * * *",
    ]);
}

#[skuld::test]
fn a_fifth_occurrence_of_a_weekday_runs_through_the_last_week() {
    // August 2027 has five Mondays, and so does May 2028: `Last` is the fifth in those months.
    assert_fires_as_cron_says(&["0 0 * 8 1", "0 0 * 5 1"]);
}

// Shapes ==============================================================================================================

#[skuld::test]
fn one_time_of_day_is_one_trigger() {
    assert_eq!(
        triggers("30 3 * * *"),
        [Trigger::Daily {
            every_days: 1,
            at: (3, 30),
            repeat: None
        }]
    );
    assert_eq!(
        triggers("* * * * *"),
        [Trigger::Daily {
            every_days: 1,
            at: (0, 0),
            repeat: repeat(1, 1439)
        }]
    );
}

#[skuld::test]
fn an_evenly_spaced_list_is_one_trigger_with_a_repetition() {
    assert_eq!(
        triggers("0 9-17 * * 1-5"),
        [Trigger::Weekly {
            weekdays: set([1, 2, 3, 4, 5]),
            at: (9, 0),
            repeat: repeat(60, 480)
        }]
    );
    // The last tick is the end of the duration, 17:45.
    assert_eq!(
        triggers("*/15 9-17 * * 1-5"),
        [Trigger::Weekly {
            weekdays: set([1, 2, 3, 4, 5]),
            at: (9, 0),
            repeat: repeat(15, 525)
        }]
    );
    // Every hour of the day from 03:00 and a lone hour: the grid is the minutes' own.
    assert_eq!(
        triggers("*/7 3 * * *"),
        [Trigger::Daily {
            every_days: 1,
            at: (3, 0),
            repeat: repeat(7, 56)
        }]
    );
}

#[skuld::test]
fn distinct_minute_sets_cost_one_trigger_each() {
    let each = |minute| Trigger::Daily {
        every_days: 1,
        at: (0, minute),
        repeat: repeat(60, 23 * 60),
    };
    // Two minutes with no common step, in every hour.
    assert_eq!(triggers("5,17 * * * *"), [each(5), each(17)]);
    // A step that does not divide 60 restarts each hour: one trigger per minute it names.
    assert_eq!(triggers("*/7 * * * *").len(), 9);
    // The two minutes of a half-hour step, every third hour.
    let every_third_hour = |minute| Trigger::Daily {
        every_days: 1,
        at: (0, minute),
        repeat: repeat(180, 21 * 60),
    };
    assert_eq!(triggers("5,35 */3 * * *"), [every_third_hour(5), every_third_hour(35)]);
}

#[skuld::test]
fn more_than_48_triggers_is_refused_naming_the_limit() {
    // 25 minutes of the first half hour, no two of them half an hour apart and none a class of 60:
    // 25 triggers for the days of the month and again for the Mondays.
    let minutes = "0-4,6-10,12-16,18-22,24-28";
    let text = format!("{minutes} * 1 * 1");
    assert_eq!(
        refusal(&text),
        format!("daemon `{ID}`: schedule `{text}` needs 50 Task Scheduler triggers, and a task holds at most 48")
    );
    // The days of the month alone are 25, under the cap.
    assert_eq!(triggers(&format!("{minutes} * 1 * *")).len(), 25);
    // The cap itself is accepted: a repetition of the first half hour in each of the 24 hours, for the
    // days of the month and again for the Mondays.
    assert_eq!(triggers("0-29 * 1 * 1").len(), TRIGGER_LIMIT);
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
    // The OR of the two day fields is both, days of the month first.
    assert_eq!(
        triggers("0 0 1 * 1"),
        [
            Trigger::MonthlyDays {
                days: set([1]),
                months: months.clone(),
                at: (0, 0),
                repeat: None
            },
            Trigger::Weekly {
                weekdays: set([1]),
                at: (0, 0),
                repeat: None
            }
        ]
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
