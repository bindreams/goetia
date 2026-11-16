//! Task Scheduler's dates are local civil times; goetia's are instants. These are the two
//! conversions, through the OS's own time zone rules.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use windows::Win32::Foundation::{FILETIME, SYSTEMTIME};
#[cfg(any(test, feature = "testing"))]
use windows::Win32::System::Time::{FileTimeToSystemTime, SystemTimeToTzSpecificLocalTime};
use windows::Win32::System::Time::{SystemTimeToFileTime, TIME_ZONE_INFORMATION, TzSpecificLocalTimeToSystemTime};

/// 100 ns ticks from 1601-01-01, the epoch of a `FILETIME`, to 1970-01-01.
const UNIX_EPOCH_TICKS: u64 = 116_444_736_000_000_000;
const TICKS_PER_SECOND: u64 = 10_000_000;

/// The span of `ticks` 100 ns ticks, exact for every count a `u64` holds.
fn span_of(ticks: u64) -> Duration {
    Duration::new(ticks / TICKS_PER_SECOND, (ticks % TICKS_PER_SECOND) as u32 * 100)
}

/// The instant a `FILETIME` names, or `None` past what a `SystemTime` holds.
fn instant_of(file_time: FILETIME) -> Option<SystemTime> {
    let ticks = u64::from(file_time.dwHighDateTime) << 32 | u64::from(file_time.dwLowDateTime);
    match ticks.checked_sub(UNIX_EPOCH_TICKS) {
        Some(after) => UNIX_EPOCH.checked_add(span_of(after)),
        None => UNIX_EPOCH.checked_sub(span_of(UNIX_EPOCH_TICKS - ticks)),
    }
}

/// The `FILETIME` of `time`, or `None` before 1601 or past what one holds.
pub(crate) fn file_time_of(time: SystemTime) -> Option<FILETIME> {
    let ticks = match time.duration_since(UNIX_EPOCH) {
        Ok(after) => UNIX_EPOCH_TICKS.checked_add(u64::try_from(after.as_nanos() / 100).ok()?)?,
        Err(before) => UNIX_EPOCH_TICKS.checked_sub(u64::try_from(before.duration().as_nanos() / 100).ok()?)?,
    };
    Some(FILETIME {
        dwLowDateTime: ticks as u32,
        dwHighDateTime: (ticks >> 32) as u32,
    })
}

/// The instant the machine's civil time `local` is, or `None` for a time the OS cannot place.
pub(super) fn local_to_system(local: &SYSTEMTIME) -> Option<SystemTime> {
    local_to_system_in(None, local)
}

/// [`local_to_system`] in `zone`, or in the machine's zone for `None`.
pub(super) fn local_to_system_in(zone: Option<&TIME_ZONE_INFORMATION>, local: &SYSTEMTIME) -> Option<SystemTime> {
    let mut utc = SYSTEMTIME::default();
    let mut file_time = FILETIME::default();
    // SAFETY: each pointer is to a live value of the type the call writes or reads.
    unsafe {
        TzSpecificLocalTimeToSystemTime(zone.map(std::ptr::from_ref), local, &mut utc).ok()?;
        SystemTimeToFileTime(&utc, &mut file_time).ok()?;
    }
    instant_of(file_time)
}

/// The machine's civil time at `time`, or `None` for an instant the OS cannot place. Only `run_times`,
/// which production never asks, reads it.
#[cfg(any(test, feature = "testing"))]
pub(super) fn system_to_local(time: SystemTime) -> Option<SYSTEMTIME> {
    let file_time = file_time_of(time)?;
    let mut utc = SYSTEMTIME::default();
    let mut local = SYSTEMTIME::default();
    // SAFETY: as in `local_to_system`.
    unsafe {
        FileTimeToSystemTime(&file_time, &mut utc).ok()?;
        SystemTimeToTzSpecificLocalTime(None, &utc, &mut local).ok()?;
    }
    Some(local)
}

#[cfg(test)]
#[path = "clock_tests.rs"]
mod clock_tests;
