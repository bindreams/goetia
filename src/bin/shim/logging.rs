//! Where the shim writes a failure, and the two channels it reaches.
//!
//! **File.** Once the shim has decoded its spec ([`set_daemon_log`]), a failure goes into the
//! daemon's own log, interleaved with the daemon's output. Before that, and whenever that file
//! cannot be opened, it goes to [`default_log_path`]: an old shim running against a newer blob
//! fails to decode `Spec` before it learns the daemon's `logs:` path, so the path derivable from
//! the service id (`argv[1]`) alone is the only file destination a decode failure can target.
//!
//! **Event Log.** Every failure also goes to the Windows Event Log. `%ProgramData%\Goetia\logs\` is
//! not writable by a non-admin service account, so a `user: someuser` daemon can die at boot
//! before it can say why through a file at all; the event log's default ACL is far more
//! permissive, so it is the more likely of the two to still work exactly when the file is not.

use std::fs::{self, File, OpenOptions};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

/// `%ProgramData%\Goetia\logs\<id>.log` — derivable from `id` alone. This is where a failure goes
/// before the spec is decoded or when the daemon's log cannot be opened, and the daemon's own log
/// when `goetia.yaml` sets no `logs:` ([`daemon_log_path`]).
pub fn default_log_path(id: &str) -> PathBuf {
    programdata_dir().join("Goetia").join("logs").join(format!("{id}.log"))
}

/// The daemon's log: `logs` from its spec, or [`default_log_path`] when unset.
pub fn daemon_log_path(id: &str, logs: Option<&Path>) -> PathBuf {
    logs.map_or_else(|| default_log_path(id), Path::to_path_buf)
}

/// The daemon's log, once the spec is decoded. One shim serves one service, so one path.
static DAEMON_LOG: OnceLock<PathBuf> = OnceLock::new();

/// From now on, failures go to `path` (see [`daemon_log_path`]) instead of the default. Call once,
/// right after the spec decodes; a later call changes nothing.
pub fn set_daemon_log(path: PathBuf) {
    let _ = DAEMON_LOG.set(path);
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

/// Writes `line` and a newline in one `write`, so output another handle appends in between cannot
/// split the line.
pub fn append_line(out: &mut impl std::io::Write, line: &str) -> std::io::Result<()> {
    out.write_all(format!("{line}\n").as_bytes())
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

/// Write `message` to the log file and the Windows Event Log as an error — both best-effort,
/// so a decode or spawn failure is never lost just because one of the two channels is itself
/// unavailable (see the module doc comment for why each can independently fail).
///
/// A test build journals the line instead; see `test_hook`.
pub fn log_failure(id: &str, message: &str) {
    log(id, Severity::Error, message);
}

/// [`log_failure`] for a condition that does not stop the daemon from running: the Event Log entry
/// is a warning.
pub fn log_warning(id: &str, message: &str) {
    log(id, Severity::Warning, message);
}

fn log(id: &str, severity: Severity, message: &str) {
    let line = failure_line(id, message);
    eprintln!("{line}");
    #[cfg(not(test))]
    write_channels(
        id,
        &line,
        default_log_path,
        DAEMON_LOG.get().map(PathBuf::as_path),
        |line| eventlog::report(severity, line),
    );
    #[cfg(test)]
    test_hook::report(id, severity, line);
}

pub fn failure_line(id: &str, message: &str) -> String {
    format!("goetia-shim[{id}]: {message}")
}

/// Compiled in every build so test builds still type-check the production channels. Takes the
/// default path derivation, the daemon's log and the Event Log write as parameters so a test can
/// drive the real composition against temp paths and a stub.
fn write_channels(
    id: &str,
    line: &str,
    default_log: impl FnOnce(&str) -> PathBuf,
    daemon_log: Option<&Path>,
    event_log: impl FnOnce(&str),
) {
    let _ = log_destinations(daemon_log, default_log(id))
        .iter()
        .any(|path| write_line(path, line));
    event_log(line);
}

/// The files to try for a line, in order: the daemon's log, then the default. When they are the
/// same file (Windows paths compare case-insensitively), it is tried once.
fn log_destinations(daemon_log: Option<&Path>, default: PathBuf) -> Vec<PathBuf> {
    let same = |a: &Path, b: &Path| a.to_string_lossy().to_lowercase() == b.to_string_lossy().to_lowercase();
    match daemon_log {
        Some(path) if !same(path, &default) => vec![path.to_path_buf(), default],
        Some(_) | None => vec![default],
    }
}

/// Whether `line` was written. Best-effort: a failure to open or write is reported, not raised.
fn write_line(path: &Path, line: &str) -> bool {
    open_append(path).and_then(|mut f| append_line(&mut f, line)).is_ok()
}

// test_hook ===========================================================================================================

/// Lines a test build's `log_failure` and `log_warning` record instead of writing, keyed by daemon
/// id so a test can take lines logged on any thread once it has a happens-before with them.
#[cfg(test)]
pub(crate) mod test_hook {
    use std::collections::BTreeMap;
    use std::sync::{Mutex, PoisonError};

    use super::Severity;

    type Journal = Mutex<BTreeMap<String, Vec<String>>>;

    /// Poison is recovered from: one panicking test must not make later log calls panic.
    static ERRORS: Journal = Mutex::new(BTreeMap::new());
    static WARNINGS: Journal = Mutex::new(BTreeMap::new());

    fn journal(severity: Severity) -> &'static Journal {
        match severity {
            Severity::Error => &ERRORS,
            Severity::Warning => &WARNINGS,
        }
    }

    pub(super) fn report(id: &str, severity: Severity, line: String) {
        journal(severity)
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .entry(id.to_string())
            .or_default()
            .push(line);
    }

    /// `id`'s error lines, oldest first, removed.
    pub(crate) fn take(id: &str) -> Vec<String> {
        take_of(Severity::Error, id)
    }

    /// `id`'s warning lines, oldest first, removed.
    pub(crate) fn take_warnings(id: &str) -> Vec<String> {
        take_of(Severity::Warning, id)
    }

    fn take_of(severity: Severity, id: &str) -> Vec<String> {
        journal(severity)
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(id)
            .unwrap_or_default()
    }

    /// `id`'s lines of both severities, oldest first within each, left in place.
    pub(crate) fn peek(id: &str) -> Vec<String> {
        [Severity::Error, Severity::Warning]
            .into_iter()
            .flat_map(|severity| {
                journal(severity)
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .get(id)
                    .cloned()
                    .unwrap_or_default()
            })
            .collect()
    }
}

#[cfg(test)]
#[path = "logging_tests.rs"]
mod logging_tests;
