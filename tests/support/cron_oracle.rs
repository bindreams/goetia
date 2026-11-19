//! A brute-force reading of a cron schedule: the first minute after a moment (or the latest at or before
//! it) that the schedule's resolved fields name, found by looking at every minute in turn with libc's
//! local time and `goetia::testing::cron_matches`. It shares nothing with a backend's rendering of the
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
    assert_next_run_strict_time(status.next_run, text, id, before, after);
}

/// [`assert_next_run_strict`] for a time read from somewhere other than a [`TaskStatus`].
pub fn assert_next_run_strict_time(
    next_run: Option<SystemTime>,
    text: &str,
    id: &Id,
    before: SystemTime,
    after: SystemTime,
) {
    assert_in(next_run, text, &strict_next_runs(&parse(text, id), before, after));
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
    let time = libc::time_t::try_from(epoch_secs).expect("a time_t");
    // SAFETY: an all-zero `tm` is a valid value, and `localtime_r` fills the one it is given.
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    // SAFETY: both pointers are valid for the call.
    let filled = unsafe { libc::localtime_r(&time, &mut tm) };
    assert!(!filled.is_null(), "localtime_r failed for {epoch_secs}");
    let [Ok(minute), Ok(hour), Ok(day), Ok(month), Ok(weekday)] =
        [tm.tm_min, tm.tm_hour, tm.tm_mday, tm.tm_mon + 1, tm.tm_wday].map(u8::try_from)
    else {
        return false;
    };
    goetia::testing::cron_matches(schedule, minute, hour, day, month, weekday)
}
