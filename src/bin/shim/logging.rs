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
use std::path::PathBuf;

/// `%ProgramData%\Goetia\logs\<id>.log` — derivable from `id` alone. This is
/// also the design spec's own OS-default `logs:` path for `type: simple` on
/// Windows when `goetia.yaml` sets none (§2, "`logs` default"), so a daemon
/// that never overrides `logs:` has no fallback/real distinction at all —
/// see `log_failure`'s doc comment for why that coincidence is load-bearing
/// rather than accidental.
pub fn default_log_path(id: &str) -> PathBuf {
    programdata_dir().join("Goetia").join("logs").join(format!("{id}.log"))
}

fn programdata_dir() -> PathBuf {
    std::env::var_os("ProgramData")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(r"C:\ProgramData"))
}

/// Open `path` for append, creating parent directories first.
pub fn open_append(path: &std::path::Path) -> std::io::Result<File> {
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
pub fn log_failure(id: &str, message: &str) {
    let line = format!("goetia-shim[{id}]: {message}");
    eprintln!("{line}");
    if let Ok(mut f) = open_append(&default_log_path(id)) {
        append_line(&mut f, &line);
    }
    eventlog::report_error(&line);
}
