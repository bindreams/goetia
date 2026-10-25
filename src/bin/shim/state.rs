//! What the shim writes to and reads from its state file
//! (`goetia::backend::scm::state_file`): the failure it is about to report, the daemon it could
//! not kill, and, as it starts, whether a daemon from an earlier incomplete stop still runs.
//!
//! Every error here is logged and none stops the daemon from running, except a live survivor:
//! a file the shim cannot write costs `goetia daemon stop` its view of a queued restart, which is
//! worse than a missing file but not worse than a service that does not run. The logs say so.

use std::path::PathBuf;

use cosca::identity::ProcessId;
use goetia::backend::scm::exit_status::{ERROR_SERVICE_ALREADY_RUNNING, Reported};
use goetia::backend::scm::state_file::{self, Begin, Clock, FailureRecord, RestartDue, Survivor, host};

use crate::logging;

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
            due: Box::new(move || host::restart_due(&service).map_err(|e| e.to_string())),
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
    let restart = match (store.due)() {
        Ok(restart) => restart,
        Err(why) => {
            logging::log_warning(id, &failure_actions_unreadable(&why));
            RestartDue::Unknown
        }
    };
    let failure = FailureRecord::at((store.clock)(), restart);
    if let Err(e) = state_file::note_failure(&store.path, failure) {
        logging::log_failure(id, &failure_not_recorded(store, &e));
    }
}

/// Record the daemon an incomplete stop could not kill, before the stop is reported.
pub fn record_survivor(id: &str, store: &Store, daemon: ProcessId) {
    let recorded = daemon
        .to_record()
        .map_err(|e| e.to_string())
        .and_then(|record| state_file::note_survivor(&store.path, &record).map_err(|e| e.to_string()));
    if let Err(why) = recorded {
        logging::log_failure(id, &survivor_not_recorded(store, &why));
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
        "not starting the daemon: process {pid} is the daemon an earlier stop could not kill, and {state}; a second one would run beside it. Stop that process, then start the service. If it is not running, delete the state file {}",
        store.path.display()
    )
}

fn begin_failed(store: &Store, e: &std::io::Error) -> String {
    format!(
        "could not read the state file {}: {e}; whether a daemon survived an earlier stop is unknown, so the daemon is being started",
        store.path.display()
    )
}

fn failure_actions_unreadable(why: &str) -> String {
    format!(
        "could not read this service's failure actions ({why}): the failure is recorded with an unknown restart delay, so `goetia daemon stop` refuses until the service next starts"
    )
}

fn failure_not_recorded(store: &Store, e: &std::io::Error) -> String {
    format!(
        "could not record this failure in {}: {e}; `goetia daemon stop` cannot see that SCM has a restart queued. Run `goetia daemon install` again",
        store.path.display()
    )
}

fn survivor_not_recorded(store: &Store, why: &str) -> String {
    format!(
        "could not record the surviving daemon in {}: {why}; a start would not be refused while it runs",
        store.path.display()
    )
}

#[cfg(test)]
#[path = "state_tests.rs"]
mod state_tests;
