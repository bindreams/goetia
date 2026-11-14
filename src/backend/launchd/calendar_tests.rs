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

fn compile(text: &str) -> Vec<Entry> {
    entries(&schedule(text), &id()).unwrap_or_else(|e| panic!("{text:?} was refused: {e}"))
}

/// The text of the refusal of `text`.
fn refusal(text: &str) -> String {
    match entries(&schedule(text), &id()) {
        Err(error @ Error::Invalid { .. }) => error.to_string(),
        Err(other) => panic!("{text:?}: expected Error::Invalid, got {other:?}"),
        Ok(compiled) => panic!("{text:?} compiled to {} entries, expected a refusal", compiled.len()),
    }
}

fn at(minute: u8, hour: u8) -> Entry {
    Entry {
        minute: Some(minute),
        hour: Some(hour),
        ..Entry::default()
    }
}

fn february_29(field: &str, text: &str) -> String {
    format!(
        "daemon `{ID}`: schedule {field} field: launchd has no year, so it cannot run on 29 February only in leap \
         years, and `{text}` includes it"
    )
}

fn weekday_under_month(text: &str) -> String {
    format!(
        "daemon `{ID}`: schedule day-of-week field: launchd ignores the month in an entry that names a weekday, so it \
         cannot limit a weekday schedule to some months, and `{text}` does"
    )
}

fn either_or(text: &str) -> String {
    format!(
        "daemon `{ID}`: launchd matches a day of the month and a day of the week as either-or, so it cannot hold \
         `{text}`: one of its day fields starts with `*` (both must match) while both day fields are restricted; use \
         `*` for one of the two"
    )
}

// The table ===========================================================================================================

#[skuld::test]
fn entries_table() {
    assert_eq!(compile("30 3 * * *"), [at(30, 3)]);

    let weekdays = compile("0,30 9-17 * * 1-5");
    assert_eq!(weekdays.len(), 90);
    assert!(
        weekdays
            .iter()
            .all(|e| e.weekday.is_some() && e.day.is_none() && e.month.is_none())
    );

    let or = |day: Option<u8>, weekday: Option<u8>| Entry {
        day,
        weekday,
        ..at(0, 0)
    };
    assert_eq!(
        compile("0 0 1,15 * 1"),
        [or(Some(1), None), or(Some(15), None), or(None, Some(1))]
    );

    assert_eq!(
        compile("0 0 * * 0"),
        [Entry {
            weekday: Some(0),
            ..at(0, 0)
        }]
    );
    assert_eq!(
        compile("0 0 * * 7"),
        [Entry {
            weekday: Some(0),
            ..at(0, 0)
        }]
    );

    assert_eq!(
        compile("15 4 1 6 *"),
        [Entry {
            day: Some(1),
            month: Some(6),
            ..at(15, 4)
        }]
    );

    // The empty dict fires every minute.
    assert_eq!(compile("* * * * *"), [Entry::default()]);
}

#[skuld::test]
fn a_full_field_below_a_given_one_is_written_out() {
    let every_minute_of_noon = compile("* 12 * * *");
    assert_eq!(every_minute_of_noon.len(), 60);
    for (minute, entry) in (0..60).zip(&every_minute_of_noon) {
        assert_eq!(
            *entry,
            Entry {
                minute: Some(minute),
                hour: Some(12),
                ..Entry::default()
            }
        );
    }

    let every_hour_of_the_first = compile("0 * 1 * *");
    assert_eq!(every_hour_of_the_first.len(), 24);
    for (hour, entry) in (0..24).zip(&every_hour_of_the_first) {
        assert_eq!(
            *entry,
            Entry {
                day: Some(1),
                ..at(0, hour)
            }
        );
    }

    assert_eq!(compile("* * 1 * *").len(), 1440);
    assert_eq!(
        compile("0 * * * *"),
        [Entry {
            minute: Some(0),
            ..Entry::default()
        }]
    );
}

#[skuld::test]
fn days_a_month_lacks_become_pairs_that_exist() {
    let pair = |day: u8, month: u8| Entry {
        day: Some(day),
        month: Some(month),
        ..at(0, 0)
    };

    let thirty_first = compile("0 0 31 * *");
    assert_eq!(thirty_first.len(), 7);
    assert_eq!(
        thirty_first,
        [1, 3, 5, 7, 8, 10, 12].map(|month| pair(31, month)),
        "exactly the 31-day months"
    );

    let thirtieth = compile("0 0 30 * *");
    assert_eq!(
        thirtieth,
        [1, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12].map(|month| pair(30, month))
    );

    assert_eq!(compile("0 0 29 3 *"), [pair(29, 3)]);
    assert!(compile("0 0 28 * *").iter().all(|e| e.month.is_none()));
    assert_eq!(compile("0 0 28 * *").len(), 1);
    assert_eq!(
        compile("0 0 15,31 4,5 *"),
        [pair(15, 4), pair(15, 5), pair(31, 5)],
        "ascending by month, then day; April lacks the 31st"
    );
}

#[skuld::test]
fn a_date_that_never_exists_compiles_to_nothing() {
    assert_eq!(compile("0 0 30 2 *"), []);
    assert_eq!(compile("0 0 31 4,6 *"), []);
}

#[skuld::test]
fn a_schedule_that_must_name_29_february_is_refused_naming_the_field() {
    for text in ["0 0 */2 * *", "0 0 29 * *", "0 0 29 2 *", "0 0 28-31 2 *"] {
        assert_eq!(refusal(text), february_29("day-of-month", text), "{text}");
    }
    for text in ["0 3 * 2 *", "0 0 * 1-6 *"] {
        assert_eq!(refusal(text), february_29("month", text), "{text}");
    }
    for text in ["0 0 * * *", "0 0 */3 * *", "0 3 * 3-12 *"] {
        compile(text);
    }
}

#[skuld::test]
fn a_weekday_under_a_restricted_month_is_refused() {
    for text in ["0 0 * 6 1", "0 0 1 6 1"] {
        assert_eq!(refusal(text), weekday_under_month(text), "{text}");
    }
    compile("0 0 * * 1");
}

#[skuld::test]
fn a_full_day_of_month_absorbs_the_weekday_under_a_restricted_month() {
    // Or with every day of the month: every day of June, so the weekday is moot and nothing is refused.
    let june = compile("0 0 1-31 6 1");
    let expected: Vec<Entry> = (1..=30)
        .map(|day| Entry {
            day: Some(day),
            month: Some(6),
            ..at(0, 0)
        })
        .collect();
    assert_eq!(june, expected);
}

#[skuld::test]
fn an_and_rule_with_both_day_fields_partial_is_refused() {
    let text = "0 0 */2 * 1";
    assert_eq!(refusal(text), either_or(text));
}

#[skuld::test]
fn and_with_one_full_side_is_the_other_side() {
    assert_eq!(
        compile("0 0 * * 1"),
        [Entry {
            weekday: Some(1),
            ..at(0, 0)
        }]
    );

    let days: Vec<(Option<u8>, Option<u8>)> = compile("0 0 */3 * *").iter().map(|e| (e.day, e.month)).collect();
    let mut expected: Vec<(Option<u8>, Option<u8>)> = (1..=28).step_by(3).map(|day| (Some(day), None)).collect();
    // Only the 31st is a day some months lack.
    expected.extend([1, 3, 5, 7, 8, 10, 12].map(|month| (Some(31), Some(month))));
    assert_eq!(days, expected);
    assert!(compile("0 0 */3 * *").iter().all(|e| e.weekday.is_none()));
}

#[skuld::test]
fn or_with_a_full_side_is_every_day() {
    assert_eq!(compile("0 0 1-31 * 1"), [at(0, 0)]);
    assert_eq!(compile("0 0 1 * 0-6"), [at(0, 0)]);
}

#[skuld::test]
fn the_largest_schedule_entries_can_produce_is_produced_whole() {
    let compiled = compile("* * 1-30 1,3-12 *");
    // Eleven months (February is out), thirty days, every minute of every hour.
    assert_eq!(compiled.len(), 11 * 30 * 24 * 60);
    assert!(compiled.iter().all(|e| e.minute.is_some() && e.hour.is_some()));
}

#[skuld::test]
fn entry_order_and_key_order_are_fixed() {
    // All day entries (no month first, then by month), then all weekday entries; within each, by hour,
    // then minute.
    let key = |e: &Entry| (e.weekday.is_some(), e.month, e.day, e.weekday, e.hour, e.minute);
    let compiled = compile("0,30 9,10 1,15 4,5 *");
    let keys: Vec<_> = compiled.iter().map(key).collect();
    let mut sorted = keys.clone();
    sorted.sort();
    assert_eq!(keys, sorted);
    assert_eq!(compiled.len(), 2 * 2 * 2 * 2);

    // Days come before weekdays.
    let or = compile("0 0 1,15 * 1");
    assert_eq!(
        or.iter().map(|e| (e.day, e.weekday)).collect::<Vec<_>>(),
        [(Some(1), None), (Some(15), None), (None, Some(1))]
    );

    // A day some months lack is written with a month, after the days that need none, and the
    // weekday entries still come last.
    let mixed = compile("0 0 1,31 * 1");
    assert_eq!(
        mixed.iter().map(|e| (e.month, e.day, e.weekday)).collect::<Vec<_>>(),
        [
            (None, Some(1), None),
            (Some(1), Some(31), None),
            (Some(3), Some(31), None),
            (Some(5), Some(31), None),
            (Some(7), Some(31), None),
            (Some(8), Some(31), None),
            (Some(10), Some(31), None),
            (Some(12), Some(31), None),
            (None, None, Some(1)),
        ]
    );
    let keys: Vec<_> = mixed.iter().map(key).collect();
    let mut sorted = keys.clone();
    sorted.sort();
    assert_eq!(keys, sorted);

    // The renderer writes the keys in the order the fields are declared: Minute, Hour, Day, Weekday, Month.
    assert_eq!(
        format!("{:?}", compile("15 4 1 6 *")[0]),
        "Entry { minute: Some(15), hour: Some(4), day: Some(1), weekday: None, month: Some(6) }"
    );
}

// The property ========================================================================================================

/// A day of a year, with the weekday it falls on.
struct Date {
    month: u8,
    day: u8,
    weekday: u8,
}

fn month_days(month: u8, leap: bool) -> u8 {
    match month {
        2 if leap => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    }
}

/// Every day of 2024 (a leap year, which starts on a Monday) or 2023 (a Sunday).
fn dates(leap: bool) -> Vec<Date> {
    let mut weekday = if leap { 1 } else { 0 };
    let mut dates = Vec::new();
    for month in 1..=12 {
        for day in 1..=month_days(month, leap) {
            dates.push(Date { month, day, weekday });
            weekday = (weekday + 1) % 7;
        }
    }
    dates
}

fn minute_of_year(date_index: usize, hour: u8, minute: u8) -> usize {
    (date_index * 24 + usize::from(hour)) * 60 + usize::from(minute)
}

/// Every minute of the year at which launchd fires `entries`, by launchd's measured semantics:
/// - a key omitted above the most significant key an entry gives is a wildcard, one omitted below or
///   between is its smallest value;
/// - a dict matches when every key it has equals the time's field, and the array is a union;
/// - a dict that has `Weekday` ignores `Month`;
/// - a `Day` that a month lacks fires on the first of the next month at 00:00, keeping no hour or minute.
fn launchd_fires(entries: &[Entry], leap: bool) -> Vec<bool> {
    let dates = dates(leap);
    let mut fires = vec![false; dates.len() * 24 * 60];
    for entry in entries {
        let month = if entry.weekday.is_some() { None } else { entry.month };
        let day_level = entry.day.is_some() || entry.weekday.is_some();
        let top = [month.is_some(), day_level, entry.hour.is_some(), entry.minute.is_some()]
            .iter()
            .position(|given| *given);
        // What an omitted key at `level` must equal: `None` is a wildcard.
        let omitted = |level: usize, smallest: u8| match top {
            Some(top) if level > top => Some(smallest),
            _ => None,
        };
        let want_day = match (entry.day, entry.weekday) {
            (None, Some(_)) => None,
            (day, _) => day.or_else(|| omitted(1, 1)),
        };
        let want_hour = entry.hour.or_else(|| omitted(2, 0));
        let want_minute = entry.minute.or_else(|| omitted(3, 0));
        let hours: Vec<u8> = want_hour.map_or_else(|| (0..24).collect(), |hour| vec![hour]);
        let minutes: Vec<u8> = want_minute.map_or_else(|| (0..60).collect(), |minute| vec![minute]);

        for (index, date) in dates.iter().enumerate() {
            let matches = month.is_none_or(|m| m == date.month)
                && want_day.is_none_or(|d| d == date.day)
                && entry.weekday.is_none_or(|w| w == date.weekday);
            if matches {
                for hour in &hours {
                    for minute in &minutes {
                        fires[minute_of_year(index, *hour, *minute)] = true;
                    }
                }
            }
        }

        if let (Some(day), None) = (entry.day, entry.weekday) {
            for m in (1..=11).filter(|m| month.is_none_or(|w| w == *m) && day > month_days(*m, leap)) {
                let next = dates.iter().position(|d| d.month == m + 1 && d.day == 1).unwrap();
                fires[minute_of_year(next, 0, 0)] = true;
            }
        }
    }
    fires
}

/// Every minute of the year at which the cron `schedule` fires.
fn cron_fires(schedule: &Schedule, leap: bool) -> Vec<bool> {
    let mut fires = Vec::new();
    for date in dates(leap) {
        for hour in 0..24 {
            for minute in 0..60 {
                fires.push(cron_matches(schedule, minute, hour, date.day, date.month, date.weekday));
            }
        }
    }
    fires
}

/// The invariant of Decision 7: the omitted fields are a prefix of month, day or weekday, hour,
/// minute, and a weekday entry has no month and no day.
fn assert_invariant(text: &str, entry: &Entry) {
    let given = [
        entry.month.is_some(),
        entry.day.is_some() || entry.weekday.is_some(),
        entry.hour.is_some(),
        entry.minute.is_some(),
    ];
    let first = given.iter().position(|g| *g).unwrap_or(given.len());
    assert!(
        given[first..].iter().all(|g| *g),
        "{text}: {entry:?} omits a key below a given one"
    );
    if entry.weekday.is_some() {
        assert!(entry.month.is_none() && entry.day.is_none(), "{text}: {entry:?}");
    }
}

#[skuld::test]
fn entries_fire_exactly_when_the_cron_says() {
    let compiling = [
        "30 3 * * *",
        "0,30 9-17 * * 1-5",
        "0 0 1,15 * 1",
        "0 0 * * 0",
        "0 0 * * 7",
        "15 4 1 6 *",
        "* * * * *",
        "* 12 * * *",
        "0 * 1 * *",
        "* * 1 * *",
        "0 * * * *",
        "*/15 */6 * * *",
        "0 0 31 * *",
        "0 0 30 * *",
        "0 0 29 3 *",
        "0 0 28 * *",
        "0 0 15,31 4,5 *",
        "0 0 30 2 *",
        "0 0 31 4,6 *",
        "0 0 * * *",
        "0 3 * 3-12 *",
        "5 4 * 1-12 *",
        "5 4 * 1,3,5,7,8,10,12 *",
        "0 0 * * 1",
        "0 0 */3 * *",
        "0 0 1-31 * 1",
        "0 0 1-31 6 1",
        "0 0 31 * 5",
        "H H/2 H * *",
        "H(0-29)/10 H(8-12) * * H(1-5)",
    ];
    let refused = [
        "0 0 */2 * *",
        "0 0 29 * *",
        "0 0 29 2 *",
        "0 0 28-31 2 *",
        "0 3 * 2 *",
        "0 0 * 1-6 *",
        "0 0 * 6 1",
        "0 0 1 6 1",
        "0 0 */2 * 1",
    ];

    for text in compiling {
        let schedule = schedule(text);
        let compiled = entries(&schedule, &id()).unwrap_or_else(|e| panic!("{text:?} was refused: {e}"));
        compiled.iter().for_each(|entry| assert_invariant(text, entry));
        for leap in [true, false] {
            let launchd = launchd_fires(&compiled, leap);
            let cron = cron_fires(&schedule, leap);
            let differ = launchd.iter().zip(&cron).position(|(l, c)| l != c);
            assert!(
                differ.is_none(),
                "{text:?}, leap year {leap}: launchd and the cron differ at minute {differ:?} of the year (launchd {:?})",
                differ.map(|i| launchd[i])
            );
        }
    }
    for text in refused {
        refusal(text);
    }
}
