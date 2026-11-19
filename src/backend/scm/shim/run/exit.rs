//! How the shim's process ends: once, with the code it reported to SCM, by [`end_process`].
//!
//! The service thread (made by SCM, not by us) records what it reports ([`stopping`]) before it
//! reports `STOPPED` and returns. The dispatcher on the main thread then returns, and the main thread
//! ends the process with the record ([`end_when_dispatched`]). The service thread ends the process
//! itself only where the dispatcher cannot be trusted to return.

use std::sync::{Arc, OnceLock};

use windows_sys::Win32::System::Threading::{GetCurrentProcess, TerminateProcess};

use crate::backend::scm::exit_status::Reported;
use crate::error::Chain;

use super::logging::Log;
use super::service::TABLE_NAME;

/// What the service thread reports to SCM, for the main thread's exit.
static EXIT: OnceLock<Reported> = OnceLock::new();

static RUN: OnceLock<Run> = OnceLock::new();

/// The log the shim writes through and the id it logs under, once the id is known. The log holds the handle on
/// the default log, so the failure line reaches the log even when an install has taken the account out of its
/// permissions; it is kept before the id is, so that an end that fails while the id is unusable (SCM named no
/// usable service) reuses it and does not build a second log that tells the machine's faults again.
struct Run {
    log: Arc<Log>,
    id: OnceLock<String>,
}

impl Run {
    fn new(log: Arc<Log>) -> Run {
        Run {
            log,
            id: OnceLock::new(),
        }
    }

    fn name(&self, id: &str) {
        let first = self.id.set(id.to_string()).is_ok();
        debug_assert!(first, "the run was named twice");
    }
}

/// Keep `log`, the one the shim writes through, for [`end_process`]. The shim's entry (`service::service_main`,
/// `task::run`) calls it once, before anything can fail.
pub(super) fn keep_log(log: Arc<Log>) {
    let first = RUN.set(Run::new(log)).is_ok();
    debug_assert!(first, "the run's log was kept twice");
}

/// Name the run `id`, which [`end_process`] logs a failure under from now on. Called once, when the id is known.
pub(super) fn name_run(id: &str) {
    if let Some(run) = RUN.get() {
        run.name(id);
    }
}

/// Record what the service thread is about to report to SCM. Once per process, before `STOPPED`.
pub(super) fn stopping(reported: Reported) {
    let first = EXIT.set(reported).is_ok();
    debug_assert!(first, "the service thread reported twice");
}

/// End the process with exactly `code`, from any thread. `TerminateProcess` runs no DLL detach and no
/// thread-local destructor, unlike `ExitProcess`, which cuts other threads off first: one cut off
/// inside the allocator can hang the exit. The log buffers nothing (each line is one write to a file handle),
/// so only a write in flight on a thread nobody joined (the `DaemonMayRun` waiter) can be cut. If it fails, it is
/// logged ([`end_failure`]) and `process::exit` is all that is left.
pub(super) fn end_process(code: i32) -> ! {
    // SAFETY: the pseudo handle of the current process is always valid.
    if unsafe { TerminateProcess(GetCurrentProcess(), code as u32) } == 0 {
        end_failure_in(RUN.get(), code, &std::io::Error::last_os_error());
    }
    std::process::exit(code)
}

/// The line for an end that failed with `e`, into the log of `run` under its id, or under [`TABLE_NAME`] while it is
/// not named. With no run (the dispatcher failed to connect: no log was kept, and no handle held) a log of its own.
fn end_failure_in(run: Option<&Run>, code: i32, e: &std::io::Error) {
    match run {
        Some(run) => end_failure(&run.log, run.id.get().map_or(TABLE_NAME, String::as_str), code, e),
        None => end_failure(&Log::real(Arc::default()), TABLE_NAME, code, e),
    }
}

fn end_failure(log: &Log, id: &str, code: i32, e: &std::io::Error) {
    log.failure(id, &format!("end the shim's process with code {code}: {}", Chain(e)));
}

/// The main thread's end, once the dispatcher has returned: [`end_process`] with what the service
/// thread recorded, or clean when no service ran.
pub(super) fn end_when_dispatched() -> ! {
    end_process(EXIT.get().copied().unwrap_or(Reported::Clean).process_exit_code())
}

#[cfg(test)]
#[path = "exit_tests.rs"]
mod exit_tests;
