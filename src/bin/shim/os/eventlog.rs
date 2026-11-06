//! The Windows Event Log write behind [`Channels::event_log`](super::Channels::event_log).

use windows_sys::Win32::System::EventLog::{
    DeregisterEventSource, EVENTLOG_ERROR_TYPE, EVENTLOG_WARNING_TYPE, RegisterEventSourceW, ReportEventW,
};

use super::{Severity, wide_null};

const SOURCE: &str = "Goetia";

/// `EventCreate.exe`'s message 1 is `%1` — a verbatim passthrough of the
/// single insertion string, which is what lets Event Viewer render our
/// message without Goetia shipping a compiled message catalogue.
const EVENT_ID: u32 = 1;

/// Writes `message` as an event of `severity`'s type. Best-effort: nothing here panics or
/// bubbles an error, because this is itself a failure-reporting path — an event log write that could fail
/// the caller would just relocate the "cannot report why" problem
/// rather than solve it.
///
/// Source registration happens at install time (see the SCM backend's
/// `register_event_source`), because it writes under `HKLM` and the shim
/// may be running as an unprivileged account. Without it, `ReportEventW`
/// still records the insertion string, but Event Viewer renders
/// "The operation completed successfully." instead of the message — which
/// defeats the whole purpose of this path, since its only reader is an
/// administrator asking why a daemon died at boot.
pub(super) fn report(severity: Severity, message: &str) {
    let event_type = match severity {
        Severity::Error => EVENTLOG_ERROR_TYPE,
        Severity::Warning => EVENTLOG_WARNING_TYPE,
    };
    let wide_source = wide_null(SOURCE);
    // SAFETY: `wide_source` is a valid, null-terminated UTF-16 string,
    // and `RegisterEventSourceW` does not retain it past this call.
    let handle = unsafe { RegisterEventSourceW(std::ptr::null(), wide_source.as_ptr()) };
    if handle.is_null() {
        return;
    }
    let wide_msg = wide_null(message);
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
