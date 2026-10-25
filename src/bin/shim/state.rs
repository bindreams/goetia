//! What the shim writes to and reads from its state file
//! (`goetia::backend::scm::state_file`): the failure it is about to report, the daemon it has
//! launched, and, as it starts, whether the daemon of an earlier run still runs.
//!
//! An error here is logged, and stops nothing, in two cases: the failure record, whose loss costs
//! `goetia daemon stop` its view of a queued restart, which is worse than a missing file but not
//! worse than a service that does not run; and the end of a daemon, whose record then names a
//! process that has ended. Two stop the run: a live daemon of an earlier run, and a launch that
//! cannot be recorded, since a daemon goetia cannot see is one it could start a second beside.

use std::path::PathBuf;

use cosca::identity::ProcessId;
use goetia::backend::scm::exit_status::{ERROR_SERVICE_ALREADY_RUNNING, Reported};
use goetia::backend::scm::state_file::{self, Begin, Cause, Clock, NewFailure, RestartDue, Survivor, host};

use crate::logging;
use crate::service::os_error_of;

/// Where the state file is, what time it is, and what the failure actions say.
pub struct Store {
    path: PathBuf,
    clock: Box<dyn Fn() -> Clock>,
    /// The restart a failure queues now; `Err` carries why the failure actions could not be read.
    due: Box<dyn Fn() -> Result<RestartDue, String>>,
}

impl Store {
    /// The state file of service `id` on this host.
    pub fn for_service(id: &str) -> Self {
        let service = id.to_string();
        Self {
            path: host::path(id),
            clock: Box::new(host::clock),
            due: Box::new(move || {
                host::failure_actions(&service)
                    .map(|actions| actions.after(Cause::Reported))
                    .map_err(|e| e.to_string())
            }),
        }
    }

    #[cfg(test)]
    pub fn at(path: PathBuf, now: Clock, due: Result<RestartDue, String>) -> Self {
        Self {
            path,
            clock: Box::new(move || now),
            due: Box::new(move || due.clone()),
        }
    }
}

/// Settle the state file as a run starts. `Err` is what to report instead of launching: a daemon an
/// earlier stop could not kill is still running.
pub fn begin(id: &str, store: &Store) -> Result<(), Reported> {
    match state_file::begin_run(&store.path, (store.clock)()) {
        Ok(Begin::Proceed) => Ok(()),
        Ok(Begin::Reset(why)) => {
            logging::log_warning(id, &reset(store, &why));
            Ok(())
        }
        Ok(Begin::Blocked { pid, survivor }) => {
            logging::log_failure(id, &blocked(store, pid, survivor));
            Err(Reported::shim_failure(Some(ERROR_SERVICE_ALREADY_RUNNING as i32)))
        }
        Err(e) => {
            logging::log_warning(id, &begin_failed(store, &e));
            Ok(())
        }
    }
}

/// Record a failure the shim is about to report, if SCM will treat it as one: a non-zero Win32
/// code. Taken as late as possible before the report, so the time recorded is never after the
/// moment SCM queues its restart.
pub fn record_failure(id: &str, store: &Store, reported: Reported) {
    if reported.scm_codes().0 == 0 {
        return;
    }
    let (live, unreadable) = match (store.due)() {
        Ok(restart) => (Some(restart), None),
        Err(why) => (None, Some(why)),
    };
    let failure = NewFailure {
        now: (store.clock)(),
        cause: Cause::Reported,
        live,
    };
    match state_file::note_failure_now(&store.path, &failure) {
        Ok(noted) => {
            if let Some(why) = unreadable {
                logging::log_warning(id, &failure_actions_unreadable(&why, noted.restart));
            }
        }
        Err(e) => logging::log_failure(id, &failure_not_recorded(store, &e)),
    }
}

/// Record the daemon the shim has just launched, before the service is reported running. `Err`
/// carries the OS error code of why not (logged here): the caller must not run the daemon.
pub fn record_launch(id: &str, store: &Store, daemon: ProcessId) -> Result<(), Option<i32>> {
    let record = daemon.to_record().map_err(|e| {
        logging::log_failure(id, &launch_not_recorded(store, &e.to_string()));
        os_error_of(&e)
    })?;
    state_file::note_launch(&store.path, &record).map_err(|e| {
        logging::log_failure(id, &launch_not_recorded(store, &e.to_string()));
        e.raw_os_error()
    })
}

/// Forget the daemon, which is known to have ended, before the service is reported stopped.
pub fn forget_launch(id: &str, store: &Store, daemon: ProcessId) {
    let forgotten = daemon
        .to_record()
        .map_err(|e| e.to_string())
        .and_then(|record| state_file::forget_launch(&store.path, &record).map_err(|e| e.to_string()));
    if let Err(why) = forgotten {
        logging::log_warning(id, &launch_not_forgotten(store, &why));
    }
}

// Failure reports -----------------------------------------------------------------------------------------------------

fn reset(store: &Store, why: &str) -> String {
    format!(
        "the state file {} could not be read and was reset: {why}",
        store.path.display()
    )
}

fn blocked(store: &Store, pid: u32, survivor: Survivor) -> String {
    let state = match survivor {
        Survivor::Alive => "it is still running",
        Survivor::Unknown | Survivor::Gone => "it could not be told not to be running",
    };
    format!(
        "not starting the daemon: process {pid} is the daemon an earlier run launched, and {state}; a second one would run beside it. Stop that process, then start the service. If it is not running, delete the state file {}",
        store.path.display()
    )
}

fn begin_failed(store: &Store, e: &std::io::Error) -> String {
    format!(
        "could not read the state file {}: {e}; whether a daemon survived an earlier stop is unknown, so the daemon is being started",
        store.path.display()
    )
}

/// The warning for failure actions that could not be read, and what the failure was recorded with
/// instead: the restart `install` configured, or, with none, an unknown delay.
fn failure_actions_unreadable(why: &str, recorded: RestartDue) -> String {
    let instead = match recorded {
        RestartDue::Unknown => {
            "an unknown restart delay, so `goetia daemon stop` refuses until the host next boots or the service is uninstalled"
        }
        _ => "the restart `goetia daemon install` configured",
    };
    format!("could not read this service's failure actions ({why}): the failure is recorded with {instead}")
}

fn failure_not_recorded(store: &Store, e: &std::io::Error) -> String {
    format!(
        "could not record this failure in {}: {e}; `goetia daemon stop` cannot see that SCM has a restart queued. Run `goetia daemon install` again",
        store.path.display()
    )
}

fn launch_not_recorded(store: &Store, why: &str) -> String {
    format!(
        "could not record the daemon in {}: {why}; the daemon is stopped rather than run where goetia cannot see it, and the service fails. If this service was registered by an older goetia, run `goetia daemon install` again",
        store.path.display()
    )
}

fn launch_not_forgotten(store: &Store, why: &str) -> String {
    format!(
        "could not clear the daemon from {}: {why}; it names a process that has ended, which goetia reads as gone",
        store.path.display()
    )
}

#[cfg(test)]
#[path = "state_tests.rs"]
mod state_tests;
