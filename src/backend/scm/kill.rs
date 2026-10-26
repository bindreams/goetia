//! `kill` for a service, which SCM has no call for: there is `ControlService(STOP)` and nothing that
//! ends a service's process as a failure. The service's process is read from SCM and terminated
//! directly, and what makes that safe is a **held handle**: the process is opened and kept open, SCM
//! is asked **again** whether it is still the service's, and only then is it terminated through that
//! same handle. While the handle is open its pid cannot be reused, so a pid SCM confirms after the
//! open is the process held, never a stranger that took the number.
//!
//! That last step rests on a premise about SCM, which Microsoft does not document: a pid SCM still
//! reports cannot have been freed and reissued to a stranger. This relies on SCM holding its own
//! handle to the service's process for as long as it reports that process's pid. SCM learns of the
//! exit through that handle, so the process object outlives the exit until SCM has handled it, and
//! measured, it records `STOPPED` with pid 0 before letting go: 250 terminated services, 13,546
//! tight-loop samples of `QueryServiceStatusEx` and `OpenProcess`, and not one reported pid whose
//! process was gone (windows-latest, run 40119307045).
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

    /// Block until `process` has exited. Unbounded: the exit is the event.
    fn await_exit(&mut self, process: &Self::Process) -> io::Result<()>;

    /// Register to be woken when SCM records the service `STOPPED`. A registration made while the
    /// service is already `STOPPED` wakes at once; one made on any other state does not wake until
    /// the next entry into `STOPPED`.
    fn arm_release(&mut self) -> io::Result<()>;

    /// Block until the registration [`Self::arm_release`] made wakes. Unbounded.
    fn await_release(&mut self) -> io::Result<()>;
}

/// Terminate the service's process, or report that it has none.
///
/// Loops only when the service's state changed under it: the process was gone before it could be
/// opened or terminated, or SCM no longer vouches for the pid it was opened by. Each round re-reads
/// SCM, which settles on the new process or on none, so the loop follows the service rather than a
/// count. Only a termination that was delivered is [`Killed::Terminated`], and only once SCM has
/// recorded the exit: the process has exited and SCM no longer reports its pid.
pub fn kill_service<T: KillTarget>(target: &mut T) -> io::Result<Killed> {
    loop {
        let Some(pid) = target.service_pid()? else {
            return Ok(Killed::NotRunning);
        };
        let process = match target.open(pid) {
            Ok(Some(process)) => process,
            // The process is gone and SCM has not yet recorded it; wait for SCM to move off the pid.
            Ok(None) => {
                await_scm_released(target, pid)?;
                continue;
            }
            // A pid that cannot be opened is a refusal only while SCM still says it is the service's.
            Err(e) => match target.service_pid() {
                Ok(Some(now)) if now == pid => return Err(e),
                // SCM has moved on: the pid was a process that exited, or somebody else's, and `e`
                // is superseded by what the next read finds.
                Ok(_) => continue,
                // Cannot tell a refusal from a stale pid; both errors are the caller's to see.
                Err(read) => {
                    return Err(io::Error::new(
                        read.kind(),
                        format!("{read} (after the service's process could not be opened: {e})"),
                    ));
                }
            },
        };
        if target.service_pid()? != Some(pid) {
            continue;
        }
        match target.terminate(&process)? {
            Termination::Terminated => {
                target.await_exit(&process)?;
                await_scm_released(target, pid)?;
                return Ok(Killed::Terminated);
            }
            // As for `Ok(None)` above: gone, and not yet recorded by SCM.
            Termination::AlreadyExited => await_scm_released(target, pid)?,
        }
    }
}

/// Block until SCM no longer reports `pid` as the service's process: it shows none, or a different
/// one (a recovery restart).
///
/// SCM learns of the exit after the process object is signalled, so a read straight after the
/// termination still shows the pid with the service `RUNNING`. The wake is the service entering
/// `STOPPED`, which SCM records, with pid 0, before it lets go of the process. SCM wakes a
/// registration at once only when the service is already in the wanted state, so the read follows
/// every arm: a release that landed between an earlier read and the arm would never wake it.
fn await_scm_released<T: KillTarget>(target: &mut T, pid: u32) -> io::Result<()> {
    loop {
        target.arm_release()?;
        if target.service_pid()? != Some(pid) {
            return Ok(());
        }
        target.await_release()?;
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

    use crate::backend::scm::wait::{ScmActor, SystemScmActor, Waited, WantState};
    use crate::manager::budget::Budget;

    use super::privilege::DebugPrivilege;
    use super::{KillTarget, Termination};

    /// The real SCM service; the process is a Win32 handle.
    pub struct SystemKillTarget {
        service: Service,
        /// The daemon's id: the service's name, and the label of the notice printed when a kill has
        /// to wait.
        id: String,
        /// Opened by the first [`KillTarget::arm_release`], as a kill that finds SCM already
        /// released never needs it.
        notifier: Option<SystemScmActor>,
    }

    impl SystemKillTarget {
        pub fn new(service: Service, id: String) -> Self {
            Self {
                service,
                id,
                notifier: None,
            }
        }

        fn notice(&self, process: &Held) {
            eprintln!("{}: waiting for process {} to exit", self.id, process.pid);
        }

        fn notifier(&mut self) -> io::Result<&mut SystemScmActor> {
            match &mut self.notifier {
                Some(actor) => Ok(actor),
                empty => Ok(empty.insert(SystemScmActor::open(&self.id)?)),
            }
        }
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
            terminate_process(&process.handle, || self.notice(process))
        }

        fn await_exit(&mut self, process: &Held) -> io::Result<()> {
            wait_exit(&process.handle, || self.notice(process))
        }

        fn arm_release(&mut self) -> io::Result<()> {
            // Unbounded, so the arm cannot expire.
            match self
                .notifier()?
                .arm(WantState::StoppedOnly, Budget::Unbounded.start())?
            {
                Waited::Confirmed => Ok(()),
                Waited::Expired => unreachable!("an unbounded deadline does not expire"),
            }
        }

        fn await_release(&mut self) -> io::Result<()> {
            match self.notifier()?.wait_callback(Budget::Unbounded.start())? {
                // Whatever woke it, the caller re-reads SCM.
                Some(_) => Ok(()),
                None => unreachable!("an unbounded deadline does not expire"),
            }
        }
    }

    /// Open `pid` with the rights to terminate it and to wait for its exit, with the caller's
    /// `SeDebugPrivilege` enabled for the open, as without it a `LocalService` service's process is
    /// refused. `None` if no process has that pid.
    pub(super) fn open_process(pid: u32) -> io::Result<Option<OwnedHandle>> {
        let debug = DebugPrivilege::enable();
        // SAFETY: no pointers; a null return is the failure.
        let handle = unsafe { OpenProcess(PROCESS_TERMINATE | PROCESS_SYNCHRONIZE, 0, pid) };
        if handle.is_null() {
            let e = io::Error::last_os_error();
            return if e.raw_os_error() == Some(ERROR_INVALID_PARAMETER as i32) {
                Ok(None)
            } else {
                Err(debug.explain_denial(e))
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
        // `TerminateProcess` on a process that is already terminating fails with `ERROR_ACCESS_DENIED`
        // while its handle is not yet signalled. A denial on a handle opened with `PROCESS_TERMINATE`
        // is therefore taken to mean 'terminating', and the exit is awaited on the handle.
        if e.raw_os_error() != Some(ERROR_ACCESS_DENIED as i32) {
            return Err(e);
        }
        wait_exit(process, on_wait).map(|()| Termination::AlreadyExited)
    }

    /// Block until the process behind `process`, an [`open_process`] handle, has exited.
    ///
    /// `on_wait` runs once, just before blocking on a process that has not yet exited.
    pub(super) fn wait_exit(process: &OwnedHandle, on_wait: impl FnOnce()) -> io::Result<()> {
        // A zero-timeout poll reads the state; it is not a wait. Already signalled: nothing to wait for.
        // SAFETY: `process` is a live handle with `PROCESS_SYNCHRONIZE`.
        match unsafe { WaitForSingleObject(process.as_raw_handle(), 0) } {
            WAIT_OBJECT_0 => return Ok(()),
            WAIT_TIMEOUT => {}
            _ => return Err(io::Error::last_os_error()),
        }
        // Unbounded on purpose: the process's exit is the event, and there is no outcome to report
        // before it. A process wedged in a driver request is never signalled, so say why this blocks.
        on_wait();
        // SAFETY: as above.
        match unsafe { WaitForSingleObject(process.as_raw_handle(), INFINITE) } {
            WAIT_OBJECT_0 => Ok(()),
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
