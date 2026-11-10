//! How the shim's process ends: once, with the code it reported to SCM, by [`end_process`].
//!
//! The service thread (made by SCM, not by us) records what it reports ([`stopping`]) before it
//! reports `STOPPED` and returns. The dispatcher on the main thread then returns, and the main thread
//! ends the process with the record ([`end_when_dispatched`]). The service thread ends the process
//! itself only where the dispatcher cannot be trusted to return.

use std::sync::OnceLock;

use windows_sys::Win32::System::Threading::{GetCurrentProcess, TerminateProcess};

use crate::backend::scm::exit_status::Reported;

/// What the service thread reports to SCM, for the main thread's exit.
static EXIT: OnceLock<Reported> = OnceLock::new();

/// Record what the service thread is about to report to SCM. Once per process, before `STOPPED`.
pub(super) fn stopping(reported: Reported) {
    let first = EXIT.set(reported).is_ok();
    debug_assert!(first, "the service thread reported twice");
}

/// End the process with exactly `code`, from any thread. `TerminateProcess` runs no DLL detach and no
/// thread-local destructor, unlike `ExitProcess`, which cuts other threads off first: one cut off
/// inside the allocator can hang the exit. Nothing is lost to it: the log opens and closes its file
/// per line, and the daemon writes its own output. If it fails, `process::exit` is all that is left.
pub(super) fn end_process(code: i32) -> ! {
    // SAFETY: the pseudo handle of the current process is always valid.
    unsafe { TerminateProcess(GetCurrentProcess(), code as u32) };
    std::process::exit(code)
}

/// The main thread's end, once the dispatcher has returned: what the service thread recorded, or
/// clean when no service ran.
pub(super) fn end_when_dispatched() -> ! {
    end_process(EXIT.get().copied().unwrap_or(Reported::Clean).process_exit_code())
}

#[cfg(test)]
#[path = "exit_tests.rs"]
mod exit_tests;
