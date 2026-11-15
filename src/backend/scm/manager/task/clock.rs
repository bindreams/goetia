//! Task Scheduler's dates are local civil times; goetia's are instants. These are the two
//! conversions, through the OS's own time zone rules.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use windows::Win32::Foundation::{FILETIME, SYSTEMTIME};
use windows::Win32::System::Time::{
    FileTimeToSystemTime, SystemTimeToFileTime, SystemTimeToTzSpecificLocalTime, TzSpecificLocalTimeToSystemTime,
};

/// 100 ns ticks from 1601-01-01, the epoch of a `FILETIME`, to 1970-01-01.
const UNIX_EPOCH_TICKS: u64 = 116_444_736_000_000_000;

/// The instant a `FILETIME` names.
fn instant_of(file_time: FILETIME) -> SystemTime {
    let ticks = u64::from(file_time.dwHighDateTime) << 32 | u64::from(file_time.dwLowDateTime);
    match ticks.checked_sub(UNIX_EPOCH_TICKS) {
        Some(after) => UNIX_EPOCH + Duration::from_nanos(after.saturating_mul(100)),
        None => UNIX_EPOCH - Duration::from_nanos((UNIX_EPOCH_TICKS - ticks).saturating_mul(100)),
    }
}

/// The `FILETIME` of `time`, or `None` before 1601 or past what one holds.
fn file_time_of(time: SystemTime) -> Option<FILETIME> {
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
    let mut utc = SYSTEMTIME::default();
    let mut file_time = FILETIME::default();
    // SAFETY: each pointer is to a live value of the type the call writes or reads.
    unsafe {
        TzSpecificLocalTimeToSystemTime(None, local, &mut utc).ok()?;
        SystemTimeToFileTime(&utc, &mut file_time).ok()?;
    }
    Some(instant_of(file_time))
}

/// The machine's civil time at `time`, or `None` for an instant the OS cannot place.
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
