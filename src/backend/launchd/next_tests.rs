use std::path::Path;
use std::time::SystemTime;

use jiff::Timestamp;
use jiff::tz::TimeZone;
use skuld::{EnvGuard, env, temp_dir};

use super::{next_fire, zone_from};
use crate::backend::launchd::calendar::{self, Entry};
use crate::error::Error;
use crate::spec::{HashSeed, Id, Schedule};
use crate::testing::cron_matches;

const MINUTE: i64 = 60;
const LOCAL: &str = "%Y-%m-%dT%H:%M";
const NEW_YORK: &str = "America/New_York";

fn schedule(text: &str) -> Schedule {
    let id = Id::try_from("task").unwrap();
    let seed = HashSeed {
        hostname: "testing",
        id: &id,
    };
    Schedule::parse(text, &seed).unwrap_or_else(|e| panic!("{text:?}: {e}"))
}

/// The entries launchd is given for the cron `text`.
fn entries(text: &str) -> Vec<Entry> {
    let id = Id::try_from("task").unwrap();
    calendar::entries(&schedule(text), &id).unwrap_or_else(|e| panic!("{text:?}: {e}"))
}

fn zone(name: &str) -> TimeZone {
    TimeZone::get(name).unwrap_or_else(|e| panic!("zone {name}: {e}"))
}

/// An instant from an RFC 3339 text with a `Z` or an offset.
fn instant(text: &str) -> SystemTime {
    text.parse::<Timestamp>()
        .unwrap_or_else(|e| panic!("{text:?}: {e}"))
        .into()
}

fn timestamp(time: SystemTime) -> Timestamp {
    Timestamp::try_from(time).unwrap()
}

/// `time` as `zone` reads it, to the minute.
fn local(zone: &TimeZone, time: SystemTime) -> String {
    timestamp(time).to_zoned(zone.clone()).strftime(LOCAL).to_string()
}

/// The next fire of the cron `text` after `after` in `zone_name`, as an RFC 3339 UTC text (`None` as "none").
fn next_utc(text: &str, zone_name: &str, after: &str) -> String {
    next_fire(&entries(text), instant(after), &zone(zone_name))
        .map_or("none".to_string(), |fire| timestamp(fire).to_string())
}

// The cron model ======================================================================================================

/// Whether `schedule` names the whole minute at `second` (Unix seconds) in `zone`, by the cron model.
fn names(schedule: &Schedule, zone: &TimeZone, second: i64) -> bool {
    let at = Timestamp::from_second(second).unwrap().to_zoned(zone.clone());
    let [minute, hour, day, month] = [at.minute(), at.hour(), at.day(), at.month()].map(|v| u8::try_from(v).unwrap());
    let weekday = u8::try_from(at.weekday().to_sunday_zero_offset()).unwrap();
    cron_matches(schedule, minute, hour, day, month, weekday)
}

/// Checks that `answer` is the first whole minute strictly after `after` that the schedule names, by
/// looking at every minute between them. Only a zone without DST agrees with the cron model everywhere.
fn assert_is_next(schedule: &Schedule, zone: &TimeZone, after: SystemTime, answer: SystemTime) {
    let after = timestamp(after).as_second() + i64::from(timestamp(after).subsec_nanosecond() > 0);
    let answer = timestamp(answer).as_second();
    let first_minute_after = (after.div_euclid(MINUTE) + 1) * MINUTE;
    assert_eq!(answer.rem_euclid(MINUTE), 0, "{answer} is on a whole minute");
    assert!(answer >= first_minute_after, "{answer} is after {after}");
    for second in (first_minute_after..answer).step_by(MINUTE as usize) {
        assert!(
            !names(schedule, zone, second),
            "{second} is named, before the answer {answer}"
        );
    }
    assert!(names(schedule, zone, answer), "{answer} is not named");
}

/// The fire after `after` in the zone `zone_name` (one without DST), checked against the cron model, as
/// the zone reads it.
fn fire_after(text: &str, zone_name: &str, after: &str) -> String {
    let (schedule, zone, after) = (schedule(text), zone(zone_name), instant(after));
    let fire = next_fire(&entries(text), after, &zone).unwrap_or_else(|| panic!("{text:?} has a next fire"));
    assert_is_next(&schedule, &zone, after, fire);
    local(&zone, fire)
}

#[skuld::test]
fn next_fire_matches_the_cron_model_in_a_zone_without_dst() {
    // (schedule, zone, after, the fire as the zone reads it)
    let table = [
        ("*/15 * * * *", "UTC", "2026-11-14T12:07:30Z", "2026-11-14T12:15"),
        ("0 9 * * 1-5", "UTC", "2026-11-13T15:00:00Z", "2026-11-16T09:00"),
        (
            "45 23 31 * *",
            "Asia/Kolkata",
            "2026-04-01T00:00:00Z",
            "2026-05-31T23:45",
        ),
        ("0 0 1 1 *", "UTC", "2026-11-14T12:00:00Z", "2027-01-01T00:00"),
        ("5 4 * * 0", "UTC", "2026-11-14T12:00:00Z", "2026-11-15T04:05"),
        ("15 3 * * *", "Asia/Kolkata", "2026-11-14T12:00:00Z", "2026-11-15T03:15"),
        ("0 12 * 6-8 *", "UTC", "2026-09-01T00:00:00Z", "2027-06-01T12:00"),
        (
            "*/20 */6 * * 1-5",
            "Asia/Kolkata",
            "2026-11-14T12:00:00Z",
            "2026-11-16T00:00",
        ),
        ("* 12 * * *", "UTC", "2026-11-14T12:07:30Z", "2026-11-14T12:08"),
    ];
    for (text, zone_name, after, expected) in table {
        assert_eq!(
            fire_after(text, zone_name, after),
            expected,
            "{text} in {zone_name} after {after}"
        );
    }
}

/// A zone with DST, away from its transitions: the cron model and launchd agree.
#[skuld::test]
fn next_fire_in_a_zone_with_dst_away_from_a_transition() {
    for (text, zone_name, after, expected) in [
        ("0 9 * * 1-5", NEW_YORK, "2026-11-13T15:00:00Z", "2026-11-16T14:00:00Z"),
        (
            "30 6 * * *",
            "Europe/London",
            "2026-03-28T07:00:00Z",
            "2026-03-29T05:30:00Z",
        ),
        ("0 0 1 1 *", NEW_YORK, "2026-11-14T12:00:00Z", "2027-01-01T05:00:00Z"),
        (
            "0 12 * 6-8 *",
            "Europe/London",
            "2026-09-01T00:00:00Z",
            "2027-06-01T11:00:00Z",
        ),
    ] {
        assert_eq!(
            next_utc(text, zone_name, after),
            expected,
            "{text} in {zone_name} after {after}"
        );
    }
}

#[skuld::test]
fn a_schedule_with_no_entries_has_no_fire() {
    let after = instant("2026-11-14T00:00:00Z");
    assert_eq!(next_fire(&[], after, &zone("UTC")), None);
    // A schedule that names no date compiles to no entries.
    for text in ["0 0 30 2 *", "0 0 31 4,6,9,11 *"] {
        assert!(entries(text).is_empty(), "{text}");
    }
}

#[skuld::test]
fn or_and_and_day_rules() {
    // Both day fields restricted: the 13th or a Friday. 2026-11-06 is a Friday.
    assert_eq!(
        fire_after("0 12 13 * 5", "UTC", "2026-11-01T00:00:00Z"),
        "2026-11-06T12:00"
    );
    assert_eq!(
        fire_after("0 12 13 * 5", "UTC", "2026-11-06T12:00:00Z"),
        "2026-11-13T12:00"
    );
    assert_eq!(
        fire_after("0 12 13 * 5", "UTC", "2026-11-13T12:00:00Z"),
        "2026-11-20T12:00"
    );
    // Day-of-week starts with `*`: the 13th, whatever it is.
    assert_eq!(
        fire_after("0 12 13 * *", "UTC", "2026-11-01T00:00:00Z"),
        "2026-11-13T12:00"
    );
}

#[skuld::test]
fn strictly_after() {
    let (entries, zone) = (entries("0 * * * *"), zone("UTC"));
    for (after, expected) in [
        ("2026-11-14T10:00:00Z", "2026-11-14T11:00:00Z"),
        ("2026-11-14T10:00:00.5Z", "2026-11-14T11:00:00Z"),
        ("2026-11-14T09:59:59.999Z", "2026-11-14T10:00:00Z"),
    ] {
        let fire = next_fire(&entries, instant(after), &zone).unwrap();
        assert_eq!(timestamp(fire), expected.parse::<Timestamp>().unwrap(), "after {after}");
    }
}

// Across DST ==========================================================================================================
// launchd's measured behaviour in America/New_York (macOS 26.7.2; the knowledge-base notes on a gap and on a fold,
// and the 12 of 66 (clock x schedule) cases that goetia's earlier model got wrong), as literal expectations.

/// 2027-03-14: 02:00 EST jumps to 03:00 EDT at 07:00:00 UTC.
const GAP_END: &str = "2027-03-14T07:00:00Z";

/// An entry that names its hour and falls in the gap fires once, at the gap's end, whatever its minute and
/// whether or not it names a date.
#[skuld::test]
fn an_entry_in_the_gap_fires_once_at_the_gaps_end() {
    // The clocks: 12:00 EST the day before, 01:30 EST on the day, and just after the gap's end.
    for after in ["2027-03-13T17:00:00Z", "2027-03-14T06:30:00Z"] {
        for text in ["30 2 * * *", "30 2 14 3 *", "30 2 14 * *", "30 2 * * 0", "0 2 * * *"] {
            assert_eq!(next_utc(text, NEW_YORK, after), GAP_END, "{text} after {after}");
        }
    }
    // The entries around the gap are not moved.
    assert_eq!(
        next_utc("59 1 * * *", NEW_YORK, "2027-03-13T17:00:00Z"),
        "2027-03-14T06:59:00Z"
    );
    assert_eq!(next_utc("0 3 * * *", NEW_YORK, "2027-03-14T06:30:00Z"), GAP_END);
    assert_eq!(
        next_utc("30 3 * * *", NEW_YORK, "2027-03-14T06:30:00Z"),
        "2027-03-14T07:30:00Z"
    );
}

/// Once the gap is past, the entries that named it are for 02:xx of a later day.
#[skuld::test]
fn an_entry_in_the_gap_waits_for_a_later_day_once_the_gap_is_past() {
    let after = "2027-03-14T07:05:00Z";
    for (text, expected) in [
        ("30 2 * * *", "2027-03-15T06:30:00Z"),
        ("0 2 * * *", "2027-03-15T06:00:00Z"),
        ("30 2 14 * *", "2027-04-14T06:30:00Z"),
        ("30 2 * * 0", "2027-03-21T06:30:00Z"),
        // 2028-03-14 is after that year's gap (03-12).
        ("30 2 14 3 *", "2028-03-14T06:30:00Z"),
    ] {
        assert_eq!(next_utc(text, NEW_YORK, after), expected, "{text}");
    }
}

/// An entry with no hour has no 02:xx to move: it skips the missing hour, and an entry for every minute
/// fires when the gap ends.
#[skuld::test]
fn an_entry_with_no_hour_skips_the_gap() {
    // Registered at 01:59:40 EST.
    let after = "2027-03-14T06:59:40Z";
    assert_eq!(
        next_utc("30 * * * *", NEW_YORK, after),
        "2027-03-14T07:30:00Z",
        "03:30 EDT"
    );
    assert_eq!(next_utc("* * * * *", NEW_YORK, after), GAP_END, "03:00 EDT");
    // Before the gap the hourly entry is on time.
    assert_eq!(
        next_utc("30 * * * *", NEW_YORK, "2027-03-14T06:29:40Z"),
        "2027-03-14T06:30:00Z"
    );
}

/// 2027-11-07: 02:00 EDT goes back to 01:00 EST at 06:00:00 UTC, so 01:30 is 05:30Z and 06:30Z.
/// (schedule, after, the next fire)
#[skuld::test]
fn an_entry_with_no_date_or_a_weekday_fires_at_both_passes_of_the_fold() {
    for (text, after, expected) in [
        ("30 1 * * *", "2027-11-07T05:00:00Z", "2027-11-07T05:30:00Z"),
        ("30 1 * * *", "2027-11-07T05:30:05Z", "2027-11-07T06:30:00Z"),
        ("30 1 * * *", "2027-11-07T06:15:00Z", "2027-11-07T06:30:00Z"),
        ("30 1 * * *", "2027-11-07T06:30:05Z", "2027-11-08T06:30:00Z"),
        // 2027-11-07 is a Sunday.
        ("30 1 * * 0", "2027-11-07T05:00:00Z", "2027-11-07T05:30:00Z"),
        ("30 1 * * 0", "2027-11-07T05:30:05Z", "2027-11-07T06:30:00Z"),
        ("30 1 * * 0", "2027-11-07T06:15:00Z", "2027-11-07T06:30:00Z"),
        ("30 1 * * 0", "2027-11-07T06:30:05Z", "2027-11-14T06:30:00Z"),
        // The day before: the first pass.
        ("30 1 * * *", "2027-11-06T17:00:00Z", "2027-11-07T05:30:00Z"),
    ] {
        assert_eq!(next_utc(text, NEW_YORK, after), expected, "{text} after {after}");
    }
}

#[skuld::test]
fn an_entry_with_a_day_or_a_month_and_day_fires_at_the_first_pass_of_the_fold_only() {
    for (text, after, expected) in [
        ("30 1 7 * *", "2027-11-06T17:00:00Z", "2027-11-07T05:30:00Z"),
        ("30 1 7 * *", "2027-11-07T05:00:00Z", "2027-11-07T05:30:00Z"),
        // Past the first pass, the second one is not a fire: December, or next year.
        ("30 1 7 * *", "2027-11-07T05:30:05Z", "2027-12-07T06:30:00Z"),
        ("30 1 7 * *", "2027-11-07T06:15:00Z", "2027-12-07T06:30:00Z"),
        ("30 1 7 11 *", "2027-11-06T17:00:00Z", "2027-11-07T05:30:00Z"),
        ("30 1 7 11 *", "2027-11-07T05:30:05Z", "2028-11-07T06:30:00Z"),
        ("30 1 7 11 *", "2027-11-07T06:15:00Z", "2028-11-07T06:30:00Z"),
    ] {
        assert_eq!(next_utc(text, NEW_YORK, after), expected, "{text} after {after}");
    }
}

/// An entry with no hour or date fires at both passes of the fold, once each.
#[skuld::test]
fn an_hourly_entry_fires_at_both_passes_of_the_fold() {
    for (after, expected) in [
        ("2027-11-07T05:29:40Z", "2027-11-07T05:30:00Z"),
        ("2027-11-07T05:30:05Z", "2027-11-07T06:30:00Z"),
        ("2027-11-07T05:40:00Z", "2027-11-07T06:30:00Z"),
        ("2027-11-07T06:30:05Z", "2027-11-07T07:30:00Z"),
        ("2027-11-07T06:40:00Z", "2027-11-07T07:30:00Z"),
    ] {
        assert_eq!(next_utc("30 * * * *", NEW_YORK, after), expected, "after {after}");
    }
}

#[skuld::test]
fn a_repeated_local_time_fires_at_both_passes_in_interleaved_order() {
    // New York's clocks go from 02:00 EDT back to 01:00 EST on 2026-11-01.
    let zone = zone(NEW_YORK);
    for (text, expected) in [
        (
            "30 1 * * *",
            &["2026-11-01T05:30:00Z", "2026-11-01T06:30:00Z", "2026-11-02T06:30:00Z"][..],
        ),
        // The instants interleave: the first pass of 01:45 comes before the second pass of 01:30.
        (
            "30,45 1 * * *",
            &[
                "2026-11-01T05:30:00Z",
                "2026-11-01T05:45:00Z",
                "2026-11-01T06:30:00Z",
                "2026-11-01T06:45:00Z",
            ],
        ),
    ] {
        let entries = entries(text);
        let mut after = instant("2026-11-01T04:00:00Z");
        for expected in expected {
            let fire = next_fire(&entries, after, &zone).unwrap();
            assert_eq!(timestamp(fire), expected.parse::<Timestamp>().unwrap(), "{text}");
            after = fire;
        }
    }
}

// Synthetic zones =====================================================================================================

/// A TZif (version 1) file whose zone has the offsets `offsets` (seconds east of UTC, none in DST), the
/// first holding until the first of `transitions` (Unix second, index into `offsets`).
fn tzif(offsets: &[i32], transitions: &[(i32, u8)]) -> Vec<u8> {
    let abbreviations: Vec<u8> = offsets.iter().flat_map(|_| *b"X\0").collect();
    let mut bytes = b"TZif".to_vec();
    bytes.extend([0; 16]); // the version, and the reserved bytes
    // isutcnt, isstdcnt, leapcnt, timecnt, typecnt, charcnt
    for count in [0, 0, 0, transitions.len(), offsets.len(), abbreviations.len()] {
        bytes.extend(u32::try_from(count).unwrap().to_be_bytes());
    }
    bytes.extend(transitions.iter().flat_map(|(second, _)| second.to_be_bytes()));
    bytes.extend(transitions.iter().map(|&(_, index)| index));
    for (index, offset) in offsets.iter().enumerate() {
        bytes.extend(offset.to_be_bytes());
        bytes.extend([0, u8::try_from(index * 2).unwrap()]); // not DST, the abbreviation
    }
    bytes.extend(abbreviations);
    bytes
}

/// UTC+05:00 all year.
fn plus_five() -> Vec<u8> {
    tzif(&[18_000], &[])
}

/// A zone at UTC+01:00 until 2026-06-01T23:30:00Z, then at UTC: the clocks go from 00:30 back to 23:30 of
/// the day before, so the minutes of 23:30 to 24:00 on 2026-06-01 are read twice.
fn back_across_midnight() -> TimeZone {
    let at = i32::try_from(timestamp(instant("2026-06-01T23:30:00Z")).as_second()).unwrap();
    TimeZone::tzif("synthetic", &tzif(&[3_600, 0], &[(at, 1)])).unwrap()
}

/// The date a repeated minute reads is not always `after`'s own, and a later date is not always a later
/// instant: the walk looks before `after`'s date, and past the first date that has a fire.
#[skuld::test]
fn a_fold_across_midnight_fires_on_the_date_it_reads_again() {
    let zone = back_across_midnight();
    // 2026-06-02T00:10 in the first pass. The clocks then go back to 2026-06-01T23:30.
    let after = instant("2026-06-01T23:10:00Z");
    assert_eq!(local(&zone, after), "2026-06-02T00:10");
    for (text, expected) in [
        // 23:40 on 2026-06-01 is yet to come, in the second pass.
        ("40 23 * * *", "2026-06-01T23:40:00Z"),
        // 00:20 on 2026-06-02 comes first, in the first pass, before 23:40 on the date before it.
        ("20,40 0,23 * * *", "2026-06-01T23:20:00Z"),
    ] {
        let fire = next_fire(&entries(text), after, &zone).unwrap();
        assert_eq!(timestamp(fire), expected.parse::<Timestamp>().unwrap(), "{text}");
    }
}

/// An offset that falls by more than a day (UTC+25:00 to UTC-25:00, the widest jiff holds: no real zone
/// does it) reads dates in an order the walk must not assume: here 2026-06-11 comes before 2026-06-09.
#[skuld::test]
fn an_offset_that_falls_by_more_than_a_day_still_gives_the_first_instant() {
    let at = i32::try_from(timestamp(instant("2026-06-10T05:30:00Z")).as_second()).unwrap();
    let zone = TimeZone::tzif("synthetic", &tzif(&[90_000, -90_000], &[(at, 1)])).unwrap();
    let entries = entries("0 6 9,11 * *");
    // 2026-06-10T06:30 in the first pass. 06:00 on the 11th is read at 05:00Z, before the 9th is read again.
    let after = instant("2026-06-09T05:30:00Z");
    let fire = next_fire(&entries, after, &zone).unwrap();
    assert_eq!(timestamp(fire), "2026-06-10T05:00:00Z".parse::<Timestamp>().unwrap());
}

// The system zone =====================================================================================================

#[skuld::test]
fn system_zone_ignores_tz(#[fixture(temp_dir)] dir: &Path, #[fixture(env)] env: &EnvGuard) {
    let path = dir.join("localtime");
    std::fs::write(&path, plus_five()).unwrap();
    env.set("TZ", NEW_YORK);
    let zone = zone_from(&path).unwrap();
    // 17:00 at +05:00 is 12:00Z; in New York it would be 22:00Z.
    let fire = next_fire(&entries("0 17 * * *"), instant("2026-11-14T11:00:00Z"), &zone).unwrap();
    assert_eq!(timestamp(fire), "2026-11-14T12:00:00Z".parse::<Timestamp>().unwrap());
}

#[skuld::test]
fn an_unreadable_localtime_is_an_error(#[fixture(temp_dir)] dir: &Path) {
    let path = dir.join("no-such-localtime");
    let Err(Error::Os { what, path: at, .. }) = zone_from(&path) else {
        panic!("a missing file is Error::Os");
    };
    assert_eq!(at.as_deref(), Some(path.as_path()), "{what}");
}

#[skuld::test]
fn a_localtime_that_is_not_tzif_is_an_error(#[fixture(temp_dir)] dir: &Path) {
    let path = dir.join("localtime");
    std::fs::write(&path, b"not a time zone").unwrap();
    let Err(Error::Os { what, path: at, .. }) = zone_from(&path) else {
        panic!("a file that is not TZif is Error::Os");
    };
    assert_eq!(at.as_deref(), Some(path.as_path()), "{what}");
}
