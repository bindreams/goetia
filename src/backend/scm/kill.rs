//! `kill` for a service, which SCM has no call for: there is `ControlService(STOP)` and nothing that
//! ends a service's process as a failure. The service's process is read from SCM and terminated
//! directly, and what makes that safe is a **held handle**: the process is opened and kept open, SCM
//! is asked **again** whether it is still the service's, and only then is it terminated through that
//! same handle. While the handle is open its pid cannot be reused, so a pid SCM confirms after the
//! open is the process held, never a stranger that took the number.
//!
//! A pure state machine over [`KillTarget`], unit-tested with a script in `kill_tests.rs`;
//! [`SystemKillTarget`] drives the real SCM and `OpenProcess`/`TerminateProcess`.
#![cfg_attr(not(windows), allow(dead_code))]

use std::io;

/// What a kill did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Killed {
    /// The service's process was terminated.
    Terminated,
    /// SCM reports the service with no process.
    NotRunning,
}

/// What asking a held process to terminate came to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Termination {
    /// The termination was delivered.
    Terminated,
    /// The process had already exited, so nothing was terminated.
    AlreadyExited,
}

/// The three things [`kill_service`] needs of the world, so its ordering is testable without one.
pub trait KillTarget {
    /// An opened process. Holding it keeps its pid from being reused; dropping it lets go.
    type Process;

    /// The pid SCM reports for the service's process, in whatever state, or `None` when it reports
    /// none.
    fn service_pid(&mut self) -> io::Result<Option<u32>>;

    /// Open the process holding `pid`; `None` if there is none.
    fn open(&mut self, pid: u32) -> io::Result<Option<Self::Process>>;

    /// Terminate `process`, or report that it had already exited.
    fn terminate(&mut self, process: &Self::Process) -> io::Result<Termination>;
}

/// Terminate the service's process, or report that it has none.
///
/// Loops only when the service's state changed under it: the process was gone before it could be
/// opened or terminated, or SCM no longer vouches for the pid it was opened by. Each round re-reads
/// SCM, which settles on the new process or on none, so the loop follows the service rather than a
/// count. Only a termination that was delivered is [`Killed::Terminated`].
pub fn kill_service<T: KillTarget>(target: &mut T) -> io::Result<Killed> {
    loop {
        let Some(pid) = target.service_pid()? else {
            return Ok(Killed::NotRunning);
        };
        let process = match target.open(pid) {
            Ok(Some(process)) => process,
            Ok(None) => continue,
            // A pid that cannot be opened is a refusal only while SCM still says it is the service's;
            // once SCM has moved on, it was a process that exited, or somebody else's.
            Err(e) if target.service_pid()? == Some(pid) => return Err(e),
            Err(_) => continue,
        };
        if target.service_pid()? != Some(pid) {
            continue;
        }
        match target.terminate(&process)? {
            Termination::Terminated => return Ok(Killed::Terminated),
            // SCM has not yet noticed the exit; it will, and until then this re-reads it.
            Termination::AlreadyExited => std::thread::yield_now(),
        }
    }
}

#[cfg(windows)]
mod privilege;

#[cfg(windows)]
pub use system::SystemKillTarget;

#[cfg(windows)]
mod system {
    use std::io;
    use std::mem::size_of;
    use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};

    use windows_service::service::Service;
    use windows_sys::Win32::Foundation::{ERROR_ACCESS_DENIED, ERROR_INVALID_PARAMETER, WAIT_OBJECT_0, WAIT_TIMEOUT};
    use windows_sys::Win32::System::Services::{QueryServiceStatusEx, SC_STATUS_PROCESS_INFO, SERVICE_STATUS_PROCESS};
    use windows_sys::Win32::System::Threading::{
        INFINITE, OpenProcess, PROCESS_SYNCHRONIZE, PROCESS_TERMINATE, TerminateProcess, WaitForSingleObject,
    };

    use super::privilege::DebugPrivilege;
    use super::{KillTarget, Termination};

    /// The real SCM service; the process is a Win32 handle.
    pub struct SystemKillTarget {
        pub service: Service,
        /// The daemon's id, for the notice printed when a kill has to wait.
        pub id: String,
    }

    /// An opened service process and the pid it was opened by.
    pub struct Held {
        handle: OwnedHandle,
        pid: u32,
    }

    /// The pid in `status`, in any state that has a process. `windows-service`'s `query_status`
    /// reports one only in `Running`, which hides the process of a service stuck starting or stopping,
    /// the case `kill` is for; SCM reports `0` when there is none.
    pub(super) fn process_of(status: &SERVICE_STATUS_PROCESS) -> Option<u32> {
        Some(status.dwProcessId).filter(|&pid| pid != 0)
    }

    impl KillTarget for SystemKillTarget {
        /// Opened with the rights to terminate it and to wait for its exit.
        type Process = Held;

        fn service_pid(&mut self) -> io::Result<Option<u32>> {
            // SAFETY: an all-zero `SERVICE_STATUS_PROCESS` is valid; the call fills it, and is given
            // its exact size.
            let mut status: SERVICE_STATUS_PROCESS = unsafe { std::mem::zeroed() };
            let mut needed = 0;
            let ok = unsafe {
                QueryServiceStatusEx(
                    self.service.raw_handle(),
                    SC_STATUS_PROCESS_INFO,
                    (&raw mut status).cast(),
                    size_of::<SERVICE_STATUS_PROCESS>() as u32,
                    &mut needed,
                )
            };
            if ok == 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(process_of(&status))
        }

        fn open(&mut self, pid: u32) -> io::Result<Option<Held>> {
            Ok(open_process(pid)?.map(|handle| Held { handle, pid }))
        }

        fn terminate(&mut self, process: &Held) -> io::Result<Termination> {
            terminate_process(&process.handle, || {
                eprintln!(
                    "{}: process {} is already terminating; waiting for it to exit",
                    self.id, process.pid
                );
            })
        }
    }

    /// Open `pid` with the rights to terminate it and to wait for its exit, with the caller's
    /// `SeDebugPrivilege` enabled for the open, as without it a `LocalService` service's process is
    /// refused. `None` if no process has that pid.
    pub(super) fn open_process(pid: u32) -> io::Result<Option<OwnedHandle>> {
        let _debug = DebugPrivilege::enable();
        // SAFETY: no pointers; a null return is the failure.
        let handle = unsafe { OpenProcess(PROCESS_TERMINATE | PROCESS_SYNCHRONIZE, 0, pid) };
        if handle.is_null() {
            let e = io::Error::last_os_error();
            // No process has this pid: it was gone.
            return if e.raw_os_error() == Some(ERROR_INVALID_PARAMETER as i32) {
                Ok(None)
            } else {
                Err(e)
            };
        }
        // SAFETY: a valid handle `OpenProcess` just returned and nothing else owns.
        Ok(Some(unsafe { OwnedHandle::from_raw_handle(handle) }))
    }

    /// Terminate the process behind `process`, an [`open_process`] handle.
    ///
    /// `on_wait` runs once, just before blocking on a process that is terminating but has not yet
    /// exited, so a caller can tell a human why nothing returns.
    pub(super) fn terminate_process(process: &OwnedHandle, on_wait: impl FnOnce()) -> io::Result<Termination> {
        // Exit code `1`: SCM sees a failure, the same as for a crash.
        // SAFETY: `process` is a live handle with `PROCESS_TERMINATE`.
        if unsafe { TerminateProcess(process.as_raw_handle(), 1) } != 0 {
            return Ok(Termination::Terminated);
        }
        let e = io::Error::last_os_error();
        // Microsoft documents that "after a process has terminated, calls to `TerminateProcess` with
        // open handles to the process fail with `ERROR_ACCESS_DENIED`". Measured by the real-process
        // test: a second terminate right after the first is denied this way while the process handle
        // is not yet signalled. So a denial on a handle opened with `PROCESS_TERMINATE` is taken to
        // mean the process is terminating, and the exit is awaited on the handle.
        if e.raw_os_error() != Some(ERROR_ACCESS_DENIED as i32) {
            return Err(e);
        }
        // A zero-timeout poll reads the state; it is not a wait. Already signalled: nothing to wait for.
        // SAFETY: as above; `PROCESS_SYNCHRONIZE` was requested.
        match unsafe { WaitForSingleObject(process.as_raw_handle(), 0) } {
            WAIT_OBJECT_0 => return Ok(Termination::AlreadyExited),
            WAIT_TIMEOUT => {}
            _ => return Err(io::Error::last_os_error()),
        }
        // Unbounded on purpose: the process's exit is the event, and there is no outcome to report
        // before it. A process wedged in a driver request is never signalled, so say why this blocks.
        on_wait();
        // SAFETY: as above.
        match unsafe { WaitForSingleObject(process.as_raw_handle(), INFINITE) } {
            WAIT_OBJECT_0 => Ok(Termination::AlreadyExited),
            _ => Err(io::Error::last_os_error()),
        }
    }
}

#[cfg(test)]
#[path = "kill_tests.rs"]
mod kill_tests;

#[cfg(all(test, windows))]
#[path = "kill_system_tests.rs"]
mod kill_system_tests;
