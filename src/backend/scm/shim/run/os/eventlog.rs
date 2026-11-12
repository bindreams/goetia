//! The Windows Event Log write behind [`Channels::event_log`](super::Channels::event_log).

use windows_sys::Win32::System::EventLog::{
    DeregisterEventSource, EVENTLOG_ERROR_TYPE, EVENTLOG_WARNING_TYPE, RegisterEventSourceW, ReportEventW,
};

use windows_sys::core::w;

use super::Severity;
use crate::backend::scm::wide::wide_null_escaped;

/// `EventCreate.exe`'s message 1 is `%1` — a verbatim passthrough of the
/// single insertion string, which is what lets Event Viewer render our
/// message without Goetia shipping a compiled message catalogue.
const EVENT_ID: u32 = 1;

/// Writes `message` as an event of `severity`'s type. Best-effort: never panics or returns an error,
/// since this is itself the failure-reporting path.
///
/// The source is registered at install time (`register_event_source`), since that needs HKLM and
/// the shim may run unprivileged. Unregistered, Event Viewer shows "The operation completed
/// successfully." instead of the message.
pub(super) fn report(severity: Severity, message: &str) {
    let event_type = match severity {
        Severity::Error => EVENTLOG_ERROR_TYPE,
        Severity::Warning => EVENTLOG_WARNING_TYPE,
    };
    // SAFETY: `w!` yields a valid, null-terminated UTF-16 string with static lifetime.
    let handle = unsafe { RegisterEventSourceW(std::ptr::null(), w!("Goetia")) };
    if handle.is_null() {
        return;
    }
    let wide_msg = wide_null_escaped(message);
    let strings = [wide_msg.as_ptr()];
    // SAFETY: `handle` is a live handle just returned by
    // `RegisterEventSourceW`; `strings` holds exactly one valid,
    // null-terminated UTF-16 string pointer, matching `wnumstrings: 1`;
    // no raw data (`lprawdata: null`, `dwdatasize: 0`); no SID
    // attribution (`lpusersid: null`).
    unsafe {
        ReportEventW(
            handle,
            event_type,
            0,
            EVENT_ID,
            std::ptr::null_mut(),
            1,
            0,
            strings.as_ptr(),
            std::ptr::null(),
        );
        DeregisterEventSource(handle);
    }
}
