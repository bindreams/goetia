//! Where the shim writes a failure, and the two channels it reaches.
//!
//! **File.** Once the shim has decoded its spec ([`Log::set_daemon_log`]), a failure goes into the
//! daemon's own log, interleaved with the daemon's output. Before that, and whenever that file
//! cannot be opened, it goes to [`Log::default_log_path`]: an old shim running against a newer blob
//! fails to decode `Spec` before it learns the daemon's `logs:` path, so the path derivable from
//! the service id (`argv[1]`) alone is the only file destination a decode failure can target.
//!
//! **Event Log.** Every failure also goes to the Windows Event Log. `%ProgramData%\Goetia\logs\` is
//! not writable by a non-admin service account, so a `user: someuser` daemon can die at boot
//! before it can say why through a file at all; the event log's default ACL is far more
//! permissive, so it is the more likely of the two to still work exactly when the file is not.

use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

use crate::os::{Channels, Real};

/// The shim's failure log: composes the two channels for one service run. One shim serves one
/// service, so one daemon log path.
pub(crate) struct Log {
    channels: Arc<dyn Channels>,
    default_dir: PathBuf,
    daemon_log: OnceLock<PathBuf>,
}

impl Log {
    /// A log over `channels` whose default log files are `<default_dir>\<id>.log`.
    pub(crate) fn new(channels: Arc<dyn Channels>, default_dir: PathBuf) -> Log {
        Log {
            channels,
            default_dir,
            daemon_log: OnceLock::new(),
        }
    }

    /// The production log: the real channels, defaulting to `%ProgramData%\Goetia\logs\`.
    pub(crate) fn real() -> Log {
        Log::new(Arc::new(Real), programdata_dir().join("Goetia").join("logs"))
    }

    /// `<default dir>\<id>.log` — derivable from `id` alone. This is where a failure goes before
    /// the spec is decoded or when the daemon's log cannot be opened, and the daemon's own log when
    /// `goetia.yaml` sets no `logs:` ([`Log::daemon_log_path`]).
    pub(crate) fn default_log_path(&self, id: &str) -> PathBuf {
        self.default_dir.join(format!("{id}.log"))
    }

    /// The daemon's log: `logs` from its spec, or [`Log::default_log_path`] when unset.
    pub(crate) fn daemon_log_path(&self, id: &str, logs: Option<&Path>) -> PathBuf {
        logs.map_or_else(|| self.default_log_path(id), Path::to_path_buf)
    }

    /// From now on, failures go to `path` (see [`Log::daemon_log_path`]) instead of the default.
    /// Call right after the spec decodes. A second call must carry the same path; a different one
    /// is a caller bug, debug-asserted.
    pub(crate) fn set_daemon_log(&self, path: PathBuf) {
        if let Err(second) = self.daemon_log.set(path) {
            debug_assert_eq!(
                Some(&second),
                self.daemon_log.get(),
                "a second `set_daemon_log` must carry the same path"
            );
        }
    }

    /// Write `message` to the log file and the Windows Event Log as an error — both best-effort,
    /// so a decode or spawn failure is never lost just because one of the two channels is itself
    /// unavailable.
    pub(crate) fn failure(&self, id: &str, message: &str) {
        self.report(id, Severity::Error, message);
    }

    /// [`Log::failure`] for a condition that does not stop the daemon from running: the Event Log
    /// entry is a warning.
    pub(crate) fn warning(&self, id: &str, message: &str) {
        self.report(id, Severity::Warning, message);
    }

    fn report(&self, id: &str, severity: Severity, message: &str) {
        let line = failure_line(id, message);
        eprintln!("{line}");
        let default = self.default_log_path(id);
        let _ = log_destinations(self.daemon_log.get().map(PathBuf::as_path), default)
            .iter()
            .any(|path| self.channels.append(path, &line).is_ok());
        self.channels.event_log(severity, &line);
    }
}

fn programdata_dir() -> PathBuf {
    std::env::var_os("ProgramData")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(r"C:\ProgramData"))
}

// Windows Event Log ===================================================================================================

pub mod eventlog {
    use std::ffi::OsStr;
    use std::os::windows::ffi::OsStrExt as _;

    use windows_sys::Win32::System::EventLog::{
        DeregisterEventSource, EVENTLOG_ERROR_TYPE, EVENTLOG_WARNING_TYPE, RegisterEventSourceW, ReportEventW,
    };

    use super::Severity;

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
    ///
    /// `EVENT_ID` is 1 because registration points `EventMessageFile` at
    /// `EventCreate.exe`, whose message 1 is a bare `%1` passthrough.
    pub fn report(severity: Severity, message: &str) {
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

    pub(crate) fn wide_null(s: &str) -> Vec<u16> {
        OsStr::new(s).encode_wide().chain(std::iter::once(0)).collect()
    }
}

/// How a line is reported to the Event Log.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    Error,
    /// The daemon runs, but something about its registration needs the administrator.
    Warning,
}

fn failure_line(id: &str, message: &str) -> String {
    format!("goetia-shim[{id}]: {message}")
}

/// The files to try for a line, in order: the daemon's log, then the default. When they are the
/// same path, it is tried once.
fn log_destinations(daemon_log: Option<&Path>, default: PathBuf) -> Vec<PathBuf> {
    match daemon_log {
        Some(path) if path != default => vec![path.to_path_buf(), default],
        Some(_) | None => vec![default],
    }
}

#[cfg(test)]
#[path = "logging_tests.rs"]
mod logging_tests;
