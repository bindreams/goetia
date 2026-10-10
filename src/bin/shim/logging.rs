//! Log-path resolution and the two failure-reporting channels a
//! version-skewed or permission-denied shim can still reach.
//!
//! **Why two channels.** `%ProgramData%\Goetia\logs\` is not writable by a
//! non-admin service account, so a `user: someuser` daemon can die at boot
//! before it can say why through a file at all — the event log's default
//! ACL is far more permissive, so it is the more likely of the two to still
//! work exactly when the file is not. And an old shim running against a
//! newer blob fails to decode `Spec` before it ever learns the daemon's own
//! `logs:` path, so [`default_log_path`] — derivable from the service id
//! (`argv[1]`) alone, no blob required — is the only file destination a
//! decode failure can ever target.

use std::fs::{self, File, OpenOptions};
use std::io::Write as _;
use std::path::{Path, PathBuf};

/// `%ProgramData%\Goetia\logs\<id>.log` — derivable from `id` alone. This is
/// also the design spec's own OS-default `logs:` path for `type: simple` on
/// Windows when `goetia.yaml` sets none (§2, "`logs` default"), so a daemon
/// that never overrides `logs:` has no fallback/real distinction at all.
pub fn default_log_path(id: &str) -> PathBuf {
    programdata_dir().join("Goetia").join("logs").join(format!("{id}.log"))
}

fn programdata_dir() -> PathBuf {
    std::env::var_os("ProgramData")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(r"C:\ProgramData"))
}

/// Open `path` for append, creating parent directories first.
pub fn open_append(path: &Path) -> std::io::Result<File> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    OpenOptions::new().create(true).append(true).open(path)
}

pub fn append_line(file: &mut File, line: &str) {
    let _ = writeln!(file, "{line}");
}

// Windows Event Log ===================================================================================================

pub mod eventlog {
    use std::ffi::OsStr;
    use std::os::windows::ffi::OsStrExt as _;

    use windows_sys::Win32::System::EventLog::{
        DeregisterEventSource, EVENTLOG_ERROR_TYPE, RegisterEventSourceW, ReportEventW,
    };

    const SOURCE: &str = "Goetia";

    /// `EventCreate.exe`'s message 1 is `%1` — a verbatim passthrough of the
    /// single insertion string, which is what lets Event Viewer render our
    /// message without Goetia shipping a compiled message catalogue.
    const EVENT_ID: u32 = 1;

    /// Best-effort: nothing here panics or bubbles an error, because this is
    /// itself a failure-reporting path — an event log write that could fail
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
    ///
    /// `EVENT_ID` is 1 because registration points `EventMessageFile` at
    /// `EventCreate.exe`, whose message 1 is a bare `%1` passthrough.
    pub fn report_error(message: &str) {
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
                EVENTLOG_ERROR_TYPE,
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

    fn wide_null(s: &str) -> Vec<u16> {
        OsStr::new(s).encode_wide().chain(std::iter::once(0)).collect()
    }
}

/// Write `message` to the id-derived fallback log path and the Windows
/// Event Log — both best-effort, so a decode or spawn failure is never lost
/// just because one of the two channels is itself unavailable (see the
/// module doc comment for why each can independently fail).
///
/// A test build journals the line instead (see `test_hook`), so a unit test never writes the
/// Event Log.
pub fn log_failure(id: &str, message: &str) {
    let line = failure_line(id, message);
    eprintln!("{line}");
    #[cfg(not(test))]
    write_channels(id, &line);
    #[cfg(test)]
    test_hook::report(id, line);
}

/// The line `log_failure` reports `message` as.
pub fn failure_line(id: &str, message: &str) -> String {
    format!("goetia-shim[{id}]: {message}")
}

/// Both production channels, fallback file first. Compiled in every build, so a change to either
/// is type-checked under test even though `log_failure` does not call it there.
fn write_channels(id: &str, line: &str) {
    write_fallback(&default_log_path(id), line);
    eventlog::report_error(line);
}

/// The file channel: append `line` to `path`, creating its directory. Best-effort.
fn write_fallback(path: &Path, line: &str) {
    if let Ok(mut f) = open_append(path) {
        append_line(&mut f, line);
    }
}

// test_hook ===========================================================================================================

/// What a test build's [`super::log_failure`] records instead of writing the fallback file and the Event
/// Log, keyed by daemon id so a test can take the lines of work done on any thread once it has a
/// happens-before with that work.
#[cfg(test)]
pub(crate) mod test_hook {
    use std::collections::BTreeMap;
    use std::sync::{Mutex, PoisonError};

    /// `pub(super)` so `logging_tests` can poison it and prove the accessors recover: one panicking
    /// test must not make every later `log_failure` panic, the waiter thread's included.
    pub(super) static REPORTED: Mutex<BTreeMap<String, Vec<String>>> = Mutex::new(BTreeMap::new());

    pub(super) fn report(id: &str, line: String) {
        REPORTED
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .entry(id.to_string())
            .or_default()
            .push(line);
    }

    /// Remove and return every line reported under `id`, oldest first.
    pub(crate) fn take(id: &str) -> Vec<String> {
        REPORTED
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(id)
            .unwrap_or_default()
    }
}

#[cfg(test)]
#[path = "logging_tests.rs"]
mod logging_tests;
