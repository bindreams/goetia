//! How the shim's process ends: once, with the code it reported to SCM.
//!
//! The service thread (made by SCM, not by us) reports `STOPPED` and returns, and the dispatcher on
//! the main thread then returns, so the main thread ends the process ([`end_when_dispatched`]). It
//! first waits for the service thread to end, which has its own cleanup after `ServiceMain` returns
//! (SCM's, and Rust's thread-local destructors) that `ExitProcess` must not cut off.
//!
//! The process-wide gate covers every other thread of ours that can be mid-work when the process
//! ends: the control handler (on the dispatcher thread, while the service thread exits on a path that
//! cannot report) and the waiter thread, which is never joined when its daemon could not be killed.
//! Their work runs under [`gated`]; [`end_process`] takes the gate and never releases it, so by then
//! none of it is running, and none of it starts.

use std::io;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::sync::{Mutex, OnceLock, PoisonError};

use windows_sys::Win32::Foundation::{DuplicateHandle, WAIT_OBJECT_0};
use windows_sys::Win32::Storage::FileSystem::SYNCHRONIZE;
use windows_sys::Win32::System::Threading::{GetCurrentProcess, GetCurrentThread, INFINITE, WaitForSingleObject};

use crate::backend::scm::exit_status::Reported;

/// What the service thread reported to SCM, for the main thread's exit.
static EXIT: OnceLock<Reported> = OnceLock::new();

/// The service thread, for the main thread to wait for.
static SERVICE_THREAD: OnceLock<OwnedHandle> = OnceLock::new();

static GATE: Mutex<()> = Mutex::new(());

/// Record what the service thread reports to SCM. Once per process.
pub(super) fn record(reported: Reported) {
    let first = EXIT.set(reported).is_ok();
    debug_assert!(first, "the service thread reported twice");
}

/// Keep a handle to the calling thread, the service thread, for [`end_when_dispatched`] to wait on.
/// Called before `STOPPED` is reported. Once per process.
pub(super) fn register_service_thread() -> io::Result<()> {
    let mut handle = std::ptr::null_mut();
    // SAFETY: the pseudo handles are valid for this call, and `handle` is a valid out-pointer.
    let ok = unsafe {
        DuplicateHandle(
            GetCurrentProcess(),
            GetCurrentThread(),
            GetCurrentProcess(),
            &mut handle,
            SYNCHRONIZE,
            0,
            0,
        )
    };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: a valid handle `DuplicateHandle` just returned, which nothing else owns.
    let handle = unsafe { OwnedHandle::from_raw_handle(handle) };
    let first = SERVICE_THREAD.set(handle).is_ok();
    debug_assert!(first, "the service thread registered twice");
    Ok(())
}

/// Run `work` with the gate held, so the process does not end under it. Where the process is
/// already ending the gate is never granted and this never returns.
pub(super) fn gated<R>(work: impl FnOnce() -> R) -> R {
    let _held = GATE.lock().unwrap_or_else(PoisonError::into_inner);
    work()
}

/// End the process with `code`, from any thread. Takes the gate first and keeps it, so nothing
/// gated is running and nothing gated starts.
pub(super) fn end_process(code: i32) -> ! {
    std::mem::forget(GATE.lock().unwrap_or_else(PoisonError::into_inner));
    std::process::exit(code)
}

/// The main thread's end, once the dispatcher has returned: wait for the service thread, which is
/// done with everything but its own teardown, then end the process with what it reported (clean
/// when no service ran). The wait has no bound: the thread ending is the event.
pub(super) fn end_when_dispatched() -> ! {
    if let Some(thread) = SERVICE_THREAD.get() {
        // SAFETY: `thread` is a live handle with SYNCHRONIZE access.
        let waited = unsafe { WaitForSingleObject(thread.as_raw_handle(), INFINITE) };
        if waited != WAIT_OBJECT_0 {
            eprintln!(
                "goetia-shim: wait for the service thread to end returned {waited:#x}: {}; exiting anyway",
                io::Error::last_os_error()
            );
        }
    }
    end_process(EXIT.get().copied().unwrap_or(Reported::Clean).process_exit_code())
}
