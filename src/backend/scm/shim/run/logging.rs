//! Where the shim writes a failure, and the two channels it reaches.
//!
//! **File.** Once the shim has decoded its spec ([`Log::set_daemon_log`]), a failure goes into the
//! daemon's own log, interleaved with the daemon's output. Before that, and whenever that file
//! cannot be opened, it goes to [`Log::default_log_path`]: an old shim running against a newer blob
//! fails to decode `Spec` before it learns the daemon's `logs:` path, so the path derivable from
//! the service id alone is the only file destination a decode failure can target.
//!
//! **The default file's folder is an install's.** `%ProgramData%\Goetia\logs` is made, and judged, by
//! the elevated install that registers the shim ([`make_goetia_folders`](crate::backend::scm::folder::make_goetia_folders)); the shim, which runs as the
//! daemon's account, makes no folder for its default log ([`Parents::Never`]). Where the folder is not
//! there, or the account cannot write in it, the append fails and the Event Log still gets the line.
//!
//! **Event Log.** Every failure also goes to the Windows Event Log, whose default ACL is far more
//! permissive than a folder's, so it is the more likely of the two to still work exactly when the
//! file does not.

use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

use super::os::{Channels, Parents, Real, Severity};
use super::service::TABLE_NAME;
use crate::backend::scm::folder::{logs_folder, program_data};
use crate::error::Result;

/// The shim's failure log: composes the two channels for one service run. One shim serves one
/// service, so one daemon log path.
pub(crate) struct Log {
    channels: Arc<dyn Channels>,
    /// `None` when the machine's `%ProgramData%` could not be located: there is then no default file.
    default_dir: Option<PathBuf>,
    daemon_log: OnceLock<PathBuf>,
}

impl Log {
    /// A log over `channels` whose default log files are `<default_dir>\<id>.log`.
    pub(crate) fn new(channels: Arc<dyn Channels>, default_dir: PathBuf) -> Log {
        Log {
            channels,
            default_dir: Some(default_dir),
            daemon_log: OnceLock::new(),
        }
    }

    /// The production log: the real channels, defaulting to `%ProgramData%\Goetia\logs\`
    /// ([`program_data`]).
    pub(crate) fn real() -> Log {
        Log::from_program_data(Arc::new(Real), program_data())
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
    /// `goetia.yaml` sets no `logs:` ([`Log::daemon_log_path`]). `None` when the machine's
    /// `%ProgramData%` could not be located.
    pub(crate) fn default_log_path(&self, id: &str) -> Option<PathBuf> {
        self.default_dir.as_ref().map(|dir| dir.join(format!("{id}.log")))
    }

    /// The daemon's log: `logs` from its spec, or [`Log::default_log_path`] when unset.
    pub(crate) fn daemon_log_path(&self, id: &str, logs: Option<&Path>) -> Option<PathBuf> {
        logs.map(Path::to_path_buf).or_else(|| self.default_log_path(id))
    }

    /// Whether the folders above the log `path` of daemon `id` are made when it is opened: not for the
    /// default log, whose folder an install made, and yes for any other, which is the user's own.
    pub(crate) fn parents_of(&self, id: &str, path: &Path) -> Parents {
        match self.default_log_path(id) {
            Some(default) if default == path => Parents::Never,
            Some(_) | None => Parents::Create,
        }
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
        let daemon_log = self.daemon_log.get().map(PathBuf::as_path);
        let files = match self.default_log_path(id) {
            Some(default) => log_destinations(daemon_log, default),
            None => daemon_log
                .map(|path| (path.to_path_buf(), Parents::Create))
                .into_iter()
                .collect(),
        };
        let _ = files
            .iter()
            .any(|(path, parents)| self.channels.append(path, &line, *parents).is_ok());
        self.channels.event_log(severity, &line);
    }
}

fn failure_line(id: &str, message: &str) -> String {
    format!("goetia-shim[{id}]: {message}")
}

/// The files to try for a line, in order, with whether each one's folders are made: the daemon's log,
/// then the default, which has none made for it. When they are the same path, it is tried once, as the
/// default.
fn log_destinations(daemon_log: Option<&Path>, default: PathBuf) -> Vec<(PathBuf, Parents)> {
    match daemon_log {
        Some(path) if path != default => vec![(path.to_path_buf(), Parents::Create), (default, Parents::Never)],
        Some(_) | None => vec![(default, Parents::Never)],
    }
}

#[cfg(test)]
#[path = "logging_tests.rs"]
mod logging_tests;
