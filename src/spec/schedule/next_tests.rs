use std::path::Path;
use std::time::SystemTime;

use jiff::Timestamp;
use jiff::tz::TimeZone;
use skuld::{EnvGuard, env, temp_dir};

use super::{next_fire, zone_from};
use crate::error::Error;
use crate::spec::{HashSeed, Id, Schedule};
use crate::testing::cron_matches;

const MINUTE: i64 = 60;
const LOCAL: &str = "%Y-%m-%dT%H:%M";

fn schedule(text: &str) -> Schedule {
    let id = Id::try_from("task").unwrap();
    let seed = HashSeed {
        hostname: "testing",
        id: &id,
    };
    Schedule::parse(text, &seed).unwrap_or_else(|e| panic!("{text:?}: {e}"))
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

/// Whether `schedule` names the whole minute at `second` (Unix seconds) in `zone`, by the cron model.
fn names(schedule: &Schedule, zone: &TimeZone, second: i64) -> bool {
    let at = Timestamp::from_second(second).unwrap().to_zoned(zone.clone());
    let [minute, hour, day, month] = [at.minute(), at.hour(), at.day(), at.month()].map(|v| u8::try_from(v).unwrap());
    let weekday = u8::try_from(at.weekday().to_sunday_zero_offset()).unwrap();
    cron_matches(schedule, minute, hour, day, month, weekday)
}

/// Checks that `answer` is the first whole minute strictly after `after` that the schedule names, by
/// looking at every minute between them.
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

/// The fire after `after` in `zone_name`, checked against the model, as `zone_name` reads it.
fn fire_after(text: &str, zone_name: &str, after: &str) -> String {
    let (schedule, zone, after) = (schedule(text), zone(zone_name), instant(after));
    let fire = next_fire(&schedule, after, &zone).unwrap_or_else(|| panic!("{text:?} has a next fire"));
    assert_is_next(&schedule, &zone, after, fire);
    local(&zone, fire)
}

#[skuld::test]
fn next_fire_matches_the_cron_model() {
    // (schedule, zone, after, the fire as the zone reads it)
    let table = [
        ("*/15 * * * *", "UTC", "2026-11-14T12:07:30Z", "2026-11-14T12:15"),
        (
            "0 9 * * 1-5",
            "America/New_York",
            "2026-11-13T15:00:00Z",
            "2026-11-16T09:00",
        ),
        (
            "30 6 * * *",
            "Europe/London",
            "2026-03-28T07:00:00Z",
            "2026-03-29T06:30",
        ),
        (
            "45 23 31 * *",
            "Asia/Kolkata",
            "2026-04-01T00:00:00Z",
            "2026-05-31T23:45",
        ),
        (
            "0 0 1 1 *",
            "America/New_York",
            "2026-11-14T12:00:00Z",
            "2027-01-01T00:00",
        ),
        ("5 4 * * 0", "UTC", "2026-11-14T12:00:00Z", "2026-11-15T04:05"),
        ("15 3 * * *", "Asia/Kolkata", "2026-11-14T12:00:00Z", "2026-11-15T03:15"),
        (
            "0 12 * 6-8 *",
            "Europe/London",
            "2026-09-01T00:00:00Z",
            "2027-06-01T12:00",
        ),
    ];
    for (text, zone_name, after, expected) in table {
        assert_eq!(
            fire_after(text, zone_name, after),
            expected,
            "{text} in {zone_name} after {after}"
        );
    }
}

#[skuld::test]
fn a_skipped_local_time_has_no_fire_that_day() {
    // New York's clocks jump from 02:00 to 03:00 on 2026-03-08, so there is no 02:30 that day.
    assert_eq!(
        fire_after("30 2 * * *", "America/New_York", "2026-03-08T05:00:00Z"),
        "2026-03-09T02:30"
    );
}

/// The model: a local time that happens twice fires at both passes. (What launchd does on a repeated
/// time is not measured.)
#[skuld::test]
fn a_repeated_local_time_fires_at_both_passes() {
    // New York's clocks go from 02:00 EDT back to 01:00 EST on 2026-11-01.
    let zone = zone("America/New_York");
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
        let schedule = schedule(text);
        let mut after = instant("2026-11-01T04:00:00Z");
        for expected in expected {
            let fire = next_fire(&schedule, after, &zone).unwrap();
            assert_is_next(&schedule, &zone, after, fire);
            assert_eq!(timestamp(fire), expected.parse::<Timestamp>().unwrap(), "{text}");
            after = fire;
        }
    }
}

#[skuld::test]
fn a_schedule_that_never_names_a_day_is_none() {
    let zone = zone("UTC");
    let after = instant("2026-11-14T00:00:00Z");
    for text in ["0 0 30 2 *", "0 0 31 4,6,9,11 *"] {
        assert_eq!(next_fire(&schedule(text), after, &zone), None, "{text}");
    }
}

#[skuld::test]
fn feb_29_finds_the_leap_year() {
    assert_eq!(
        fire_after("0 0 29 2 *", "UTC", "2026-11-14T00:00:00Z"),
        "2028-02-29T00:00"
    );
    // 2100 is no leap year: the next one after 2096 is 2104.
    assert_eq!(
        fire_after("0 0 29 2 *", "UTC", "2096-03-01T00:00:00Z"),
        "2104-02-29T00:00"
    );
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
    // Day-of-month starts with `*`: both must match. Odd days that are Fridays: 2026-11-13.
    assert_eq!(
        fire_after("0 12 */2 * 5", "UTC", "2026-11-01T00:00:00Z"),
        "2026-11-13T12:00"
    );
    // Day-of-week starts with `*`: the 13th, whatever it is.
    assert_eq!(
        fire_after("0 12 13 * *", "UTC", "2026-11-01T00:00:00Z"),
        "2026-11-13T12:00"
    );
}

#[skuld::test]
fn strictly_after() {
    let (schedule, zone) = (schedule("0 * * * *"), zone("UTC"));
    for (after, expected) in [
        ("2026-11-14T10:00:00Z", "2026-11-14T11:00:00Z"),
        ("2026-11-14T10:00:00.5Z", "2026-11-14T11:00:00Z"),
        ("2026-11-14T09:59:59.999Z", "2026-11-14T10:00:00Z"),
    ] {
        let fire = next_fire(&schedule, instant(after), &zone).unwrap();
        assert_eq!(timestamp(fire), expected.parse::<Timestamp>().unwrap(), "after {after}");
    }
}

/// A TZif (version 1) file of one zone, UTC+05:00 all year.
fn plus_five() -> Vec<u8> {
    let mut bytes = b"TZif".to_vec();
    bytes.extend([0; 16]); // the version, and the reserved bytes
    // isutcnt, isstdcnt, leapcnt, timecnt, typecnt, charcnt
    for count in [0u32, 0, 0, 0, 1, 4] {
        bytes.extend(count.to_be_bytes());
    }
    bytes.extend(18_000i32.to_be_bytes()); // utoff
    bytes.extend([0, 0]); // not DST, abbreviation at 0
    bytes.extend(b"XYZ\0");
    bytes
}

#[skuld::test]
fn system_zone_ignores_tz(#[fixture(temp_dir)] dir: &Path, #[fixture(env)] env: &EnvGuard) {
    let path = dir.join("localtime");
    std::fs::write(&path, plus_five()).unwrap();
    env.set("TZ", "America/New_York");
    let zone = zone_from(&path).unwrap();
    // 17:00 at +05:00 is 12:00Z; in New York it would be 22:00Z.
    let fire = next_fire(&schedule("0 17 * * *"), instant("2026-11-14T11:00:00Z"), &zone).unwrap();
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
