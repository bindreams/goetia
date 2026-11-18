//! Where the shim writes a failure, and the two channels it reaches.
//!
//! **File.** Once the shim has decoded its spec ([`Log::set_daemon_log`]), a failure goes into the
//! daemon's own log, interleaved with the daemon's output. Before that, and whenever that file
//! cannot be opened, it goes to [`Log::default_log_path`]: an old shim running against a newer blob
//! fails to decode `Spec` before it learns the daemon's `logs:` path, so the path derivable from
//! the service id alone is the only file destination a decode failure can target.
//!
//! **The default file is an install's.** `%ProgramData%\Goetia\logs` and `<id>.log` in it are made, and
//! judged, by the elevated install that registers the shim ([`default_log::ensure`]); the shim, which runs
//! as the daemon's account, makes neither and opens the file for appending alone ([`LogKind::Default`]), once,
//! when it starts ([`Log::hold_default`]): an install may take the account out of the file's permissions (an
//! account change, which SCM does not apply to a running service), and a handle keeps what it was opened with.
//! Where the file is not there, or is a link, or the account cannot append to it, the append fails and the
//! Event Log still gets the line.
//!
//! **The default log is told by its text.** A daemon writes the default log when its `logs:` is unset, or is
//! the default path as written ([`Log::daemon_log`]): an install refuses a `logs:` in the folder of the
//! default logs ([`default_log::refuse_logs_in_folder`]), but a service an earlier version installed may carry
//! that one, and opened as the user's own it would be refused by the file's permissions. No other path is
//! compared, and none is looked up.
//!
//! **Event Log.** Every failure also goes to the Windows Event Log, whose default ACL is far more
//! permissive than a folder's, so it is the more likely of the two to still work exactly when the
//! file does not.

use std::collections::HashSet;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock, PoisonError};

use super::os::{Channels, LogKind, Real, Severity};
use super::service::TABLE_NAME;
use crate::backend::scm::default_log;
use crate::backend::scm::folder::{logs_folder, program_data};
use crate::error::{Chain, Result};

/// The shim's failure log: composes the two channels for one service run. One shim serves one
/// service, so one daemon log path.
pub(crate) struct Log {
    channels: Arc<dyn Channels>,
    /// `None` when the machine's `%ProgramData%` could not be located: there is then no default file.
    default_dir: Option<PathBuf>,
    daemon_log: OnceLock<PathBuf>,
    /// The open faults told, by file, kind of error and what the failed open costs, so that one fault is told once
    /// however often it is met. A shim process makes one launch of its daemon, so what is told is told for it.
    told: Mutex<HashSet<(PathBuf, ErrorKind, &'static str)>>,
}

impl Log {
    /// A log over `channels` whose default log files are `<default_dir>\<id>.log`.
    pub(crate) fn new(channels: Arc<dyn Channels>, default_dir: PathBuf) -> Log {
        Log {
            channels,
            default_dir: Some(default_dir),
            daemon_log: OnceLock::new(),
            told: Mutex::default(),
        }
    }

    /// The production log over `real`, defaulting to `%ProgramData%\Goetia\logs\` ([`program_data`]). `real` is
    /// the shim's one [`Real`], which is also its [`Os`](super::os::Os): the daemon's output and this log's lines
    /// share the handle it holds on the default log.
    pub(crate) fn real(real: Arc<Real>) -> Log {
        Log::from_program_data(real, program_data())
    }

    /// A log over `channels` for the `%ProgramData%` that `found` says. When it was not found there
    /// is no default file, since no other place is the machine's: failures go to the daemon's own log
    /// once [`Log::set_daemon_log`] names one, and to the Event Log. The first Event Log line says
    /// why there is no default file.
    pub(crate) fn from_program_data(channels: Arc<dyn Channels>, found: Result<PathBuf>) -> Log {
        match found {
            Ok(program_data) => Log::new(channels, logs_folder(&program_data)),
            Err(e) => {
                let log = Log {
                    channels,
                    default_dir: None,
                    daemon_log: OnceLock::new(),
                    told: Mutex::default(),
                };
                log.failure(
                    TABLE_NAME,
                    &format!(
                        "no log file: the machine's %ProgramData% could not be located: {}",
                        e.report()
                    ),
                );
                log
            }
        }
    }

    /// `<default dir>\<id>.log` — derivable from `id` alone. This is where a failure goes before
    /// the spec is decoded or when the daemon's log cannot be opened, and the daemon's own log when
    /// `goetia.yaml` sets no `logs:` ([`Log::daemon_log`]). `None` when the machine's
    /// `%ProgramData%` could not be located.
    pub(crate) fn default_log_path(&self, id: &str) -> Option<PathBuf> {
        self.default_dir.as_ref().map(|dir| default_log::path_in(dir, id))
    }

    /// Opens the default log of daemon `id` and holds it for the rest of the shim's run
    /// ([`Channels::hold_default`]). Call it once, when the shim starts and
    /// knows its `id`, before anything an install can change. A log that cannot be held is told to the Event
    /// Log, once, with the error: the shim goes on. What the opens that follow meet is theirs to tell.
    pub(crate) fn hold_default(&self, id: &str) {
        let Some(path) = self.default_log_path(id) else {
            return;
        };
        if let Err(e) = self.channels.hold_default(&path) {
            let line = failure_line(id, &format!("hold the default log {}: {}", path.display(), Chain(&e)));
            eprintln!("{line}");
            self.channels.event_log(Severity::Error, &line);
        }
    }

    /// Tells that the log file `path` could not be opened for the daemon's output, with `e`, unless that was told
    /// already (the same file, the same kind of error, the same open): the marker's open of the same file failing
    /// alike is another fact, with its own cost ([`Log::marker_open_failed`]).
    pub(crate) fn open_failed(&self, id: &str, path: &Path, e: &std::io::Error) {
        self.tell_open_failure(
            id,
            path,
            e,
            "this daemon's output will not be captured; it is still being run",
        );
    }

    /// [`Log::open_failed`] for the open of the run's marker, which has its own consequence.
    pub(crate) fn marker_open_failed(&self, id: &str, path: &Path, e: &std::io::Error) {
        self.tell_open_failure(
            id,
            path,
            e,
            "the run marker could not be written; the program is still run",
        );
    }

    fn tell_open_failure(&self, id: &str, path: &Path, e: &std::io::Error, consequence: &'static str) {
        let first = self.told.lock().unwrap_or_else(PoisonError::into_inner).insert((
            path.to_path_buf(),
            e.kind(),
            consequence,
        ));
        if first {
            self.failure(id, &log_open_failed(path, e, consequence));
        }
    }

    /// The daemon's log and its kind: `logs` from its spec, which is the user's own, unless it is the default
    /// log's path as written; and, when unset, [`Log::default_log_path`], the default log (see the module).
    pub(crate) fn daemon_log(&self, id: &str, logs: Option<&Path>) -> Option<(PathBuf, LogKind)> {
        let default = self.default_log_path(id);
        match logs {
            Some(path) if default.as_deref() != Some(path) => Some((path.to_path_buf(), LogKind::Own)),
            _ => default.map(|path| (path, LogKind::Default)),
        }
    }

    /// From now on, failures go to `path`, the daemon's own log ([`Log::daemon_log`] says `Own`), before the
    /// default. Call right after the spec decodes, and only for an own log: with none, the default log is the
    /// daemon's and is always tried. A second call must carry the same path; a different one is a caller bug,
    /// debug-asserted.
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
        let daemon_log = self.daemon_log.get().map(PathBuf::as_path);
        let files = match self.default_log_path(id) {
            Some(default) => log_destinations(daemon_log, default),
            None => daemon_log
                .map(|path| (path.to_path_buf(), LogKind::Own))
                .into_iter()
                .collect(),
        };
        let _ = files
            .iter()
            .any(|(path, kind)| self.channels.append(path, &line, *kind).is_ok());
        self.channels.event_log(severity, &line);
    }
}

fn failure_line(id: &str, message: &str) -> String {
    format!("goetia-shim[{id}]: {message}")
}

/// An open of the log file `path` that failed with `e`, and what `consequence` says it costs.
fn log_open_failed(path: &Path, e: &std::io::Error, consequence: &str) -> String {
    format!("open log file {}: {} ({consequence})", path.display(), Chain(e))
}

/// The files to try for a line, in order, with the kind of each: the daemon's own log when its spec sets
/// `logs:`, then the default.
fn log_destinations(daemon_log: Option<&Path>, default: PathBuf) -> Vec<(PathBuf, LogKind)> {
    match daemon_log {
        Some(path) => vec![(path.to_path_buf(), LogKind::Own), (default, LogKind::Default)],
        None => vec![(default, LogKind::Default)],
    }
}

#[cfg(test)]
#[path = "logging_tests.rs"]
mod logging_tests;
