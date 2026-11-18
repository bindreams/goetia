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

/// The log of the run and the id it logs under, for [`end_process`]. The log holds the handle on the default
/// log, so the failure line reaches the log even when an install has taken the account out of its permissions.
static RUN: OnceLock<(Arc<Log>, String)> = OnceLock::new();

/// Keep `log`, the one the run `id` writes through, for [`end_process`]. Only the run's start calls it
/// (`service::run_service`, `task::run`), once, as the id is known.
pub(super) fn keep_run(log: Arc<Log>, id: &str) {
    let _ = RUN.set((log, id.to_string()));
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
        let e = std::io::Error::last_os_error();
        match RUN.get() {
            Some((log, id)) => end_failure(log, id, code, &e),
            // No run began (the dispatcher failed to connect, or SCM named no service): no id, and no handle held.
            None => end_failure(&Log::real(Arc::default()), TABLE_NAME, code, &e),
        }
    }
    std::process::exit(code)
}

/// The line for an end that failed with `e`: under the run's `id`, into `log`, the run's.
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
