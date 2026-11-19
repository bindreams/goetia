//! A brute-force reading of a cron schedule: the first minute after a moment (or the latest at or before
//! it) that the schedule's resolved fields name, found by looking at every minute in turn with libc's
//! local time (on Linux, the zone systemd reads, `/etc/localtime`) and `goetia::testing::cron_matches`. It shares nothing with a backend's rendering of the
//! schedule, so a backend's `next_run` is checked against it.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use goetia::manager::TaskStatus;
use goetia::spec::{HashSeed, Id, Schedule};

const MINUTE: u64 = 60;

/// What a schedule's `next_run` may read for a timer armed anywhere in `[before, after]`: the next
/// minute after `before`, the next after `after` (`after` is taken once `next_run` has been read, so a
/// read that comes after the timer fired is still inside the window), and the latest minute at or
/// before `before`.
///
/// The last is for a lane whose wall clock steps back: systemd computes `NextElapseUSecRealtime` from
/// its own clock read, which may lie before `before` by less than one cron period (the one assumption:
/// a step is minutes, not days). If the step crosses a fire time P, systemd correctly reports P, which
/// is not after `before`. Accepting it loses nothing here. goetia reads the property and does not
/// compute it. A timer that never fired reads none or the epoch for a wrong property, and a
/// rendering bug yields a future time from another calendar; both are still caught. (launchd's
/// `next_run` is computed by goetia from the clock it reads, so the first is never needed there.)
pub fn expected_next_runs(text: &str, id: &Id, before: SystemTime, after: SystemTime) -> [SystemTime; 3] {
    let schedule = parse(text, id);
    let [next_before, next_after] = strict_next_runs(&schedule, before, after);
    [latest_minute_at_or_before(&schedule, before), next_before, next_after]
}

/// The next minute after `before` and the next after `after`: what `next_run` may read when the clock
/// does not step back.
fn strict_next_runs(schedule: &Schedule, before: SystemTime, after: SystemTime) -> [SystemTime; 2] {
    [next_minute_after(schedule, before), next_minute_after(schedule, after)]
}

fn parse(text: &str, id: &Id) -> Schedule {
    let seed = HashSeed {
        hostname: "testing",
        id,
    };
    Schedule::parse(text, &seed).expect("a schedule")
}

/// `next_run` is one of [`expected_next_runs`]: for systemd, whose clock may step back.
pub fn assert_next_run(status: &TaskStatus, text: &str, id: &Id, before: SystemTime, after: SystemTime) {
    assert_next_run_time(status.next_run, text, id, before, after);
}

/// [`assert_next_run`] for a time read from somewhere other than a [`TaskStatus`] (the CLI's document).
pub fn assert_next_run_time(next_run: Option<SystemTime>, text: &str, id: &Id, before: SystemTime, after: SystemTime) {
    assert_in(next_run, text, &expected_next_runs(text, id, before, after));
}

/// `next_run` is the next minute after `before` or the next after `after`, and never the minute at or
/// before `before`: for launchd, whose `next_run` goetia computes from the clock it reads.
pub fn assert_next_run_strict(status: &TaskStatus, text: &str, id: &Id, before: SystemTime, after: SystemTime) {
    assert_in(
        status.next_run,
        text,
        &strict_next_runs(&parse(text, id), before, after),
    );
}

fn assert_in(next_run: Option<SystemTime>, text: &str, expected: &[SystemTime]) {
    assert!(
        next_run.is_some_and(|next| expected.contains(&next)),
        "`{text}`: next_run {next_run:?}, the cron says {expected:?}"
    );
}

/// The first whole minute (local time, as the manager reads `OnCalendar`) strictly after `after`
/// that `schedule` names. Every schedule a caller gives has one: the search ends there.
pub fn next_minute_after(schedule: &Schedule, after: SystemTime) -> SystemTime {
    let secs = after
        .duration_since(UNIX_EPOCH)
        .expect("a time after the epoch")
        .as_secs();
    let mut minute = (secs / MINUTE + 1) * MINUTE;
    while !names(schedule, minute) {
        minute += MINUTE;
    }
    UNIX_EPOCH + Duration::from_secs(minute)
}

/// The latest whole minute at or before `at` that `schedule` names: the mirror of
/// [`next_minute_after`], and the minute containing `at` counts. The search ends at a match.
pub fn latest_minute_at_or_before(schedule: &Schedule, at: SystemTime) -> SystemTime {
    let secs = at.duration_since(UNIX_EPOCH).expect("a time after the epoch").as_secs();
    let mut minute = secs / MINUTE * MINUTE;
    while !names(schedule, minute) {
        minute -= MINUTE;
    }
    UNIX_EPOCH + Duration::from_secs(minute)
}

fn names(schedule: &Schedule, epoch_secs: u64) -> bool {
    let [minute, hour, day, month, weekday] = local_fields(epoch_secs);
    goetia::testing::cron_matches(schedule, minute, hour, day, month, weekday)
}

/// The zone systemd reads `OnCalendar` in: `/etc/localtime`, whatever `TZ` the test process has; UTC
/// where there is no such file, as systemd has it.
#[cfg(target_os = "linux")]
pub fn manager_zone() -> &'static jiff::tz::TimeZone {
    static ZONE: std::sync::OnceLock<jiff::tz::TimeZone> = std::sync::OnceLock::new();
    ZONE.get_or_init(|| match std::fs::read("/etc/localtime") {
        Ok(bytes) => jiff::tz::TimeZone::tzif("/etc/localtime", &bytes).expect("/etc/localtime is a tzif file"),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => jiff::tz::TimeZone::UTC,
        Err(e) => panic!("read /etc/localtime: {e}"),
    })
}

/// `[minute, hour, day, month, weekday (Sunday is 0)]` of `epoch_secs` in the manager's local time.
#[cfg(target_os = "linux")]
fn local_fields(epoch_secs: u64) -> [u8; 5] {
    let secs = i64::try_from(epoch_secs).expect("an i64 time");
    let at = manager_zone().to_datetime(jiff::Timestamp::from_second(secs).expect("a time jiff holds"));
    [
        u8::try_from(at.minute()).expect("a minute"),
        u8::try_from(at.hour()).expect("an hour"),
        u8::try_from(at.day()).expect("a day"),
        u8::try_from(at.month()).expect("a month"),
        u8::try_from(at.weekday().to_sunday_zero_offset()).expect("a weekday"),
    ]
}

/// The same fields in the process's local time, as libc reads it (`TZ`, then `/etc/localtime`).
#[cfg(not(target_os = "linux"))]
fn local_fields(epoch_secs: u64) -> [u8; 5] {
    let time = libc::time_t::try_from(epoch_secs).expect("a time_t");
    // SAFETY: an all-zero `tm` is a valid value, and `localtime_r` fills the one it is given.
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    // SAFETY: both pointers are valid for the call.
    let filled = unsafe { libc::localtime_r(&time, &mut tm) };
    assert!(!filled.is_null(), "localtime_r failed for {epoch_secs}");
    [tm.tm_min, tm.tm_hour, tm.tm_mday, tm.tm_mon + 1, tm.tm_wday].map(|field| u8::try_from(field).expect("a tm field"))
}
