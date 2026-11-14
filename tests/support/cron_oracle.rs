//! A brute-force reading of a cron schedule: the first minute after a moment that the schedule's
//! resolved fields name, found by looking at every minute in turn. It shares nothing with a
//! backend's rendering of the schedule, so a backend's `next_run` is checked against it.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use goetia::spec::{DayRule, Schedule};

const MINUTE: u64 = 60;

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

fn names(schedule: &Schedule, epoch_secs: u64) -> bool {
    let [minute, hour, day, month, weekday] = schedule.fields();
    let time = libc::time_t::try_from(epoch_secs).expect("a time_t");
    // SAFETY: an all-zero `tm` is a valid value, and `localtime_r` fills the one it is given.
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    // SAFETY: both pointers are valid for the call.
    let filled = unsafe { libc::localtime_r(&time, &mut tm) };
    assert!(!filled.is_null(), "localtime_r failed for {epoch_secs}");
    let has = |field: &goetia::spec::CronField, value: libc::c_int| {
        u8::try_from(value).is_ok_and(|value| field.values.contains(&value))
    };
    let day_ok = match schedule.day_rule() {
        DayRule::And => has(day, tm.tm_mday) && has(weekday, tm.tm_wday),
        DayRule::Or => has(day, tm.tm_mday) || has(weekday, tm.tm_wday),
    };
    has(minute, tm.tm_min) && has(hour, tm.tm_hour) && has(month, tm.tm_mon + 1) && day_ok
}
