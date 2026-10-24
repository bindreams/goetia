//! `kill` for a service, which SCM has no call for: there is `ControlService(STOP)` and nothing that
//! ends a service's process as a failure. The service's process is read from SCM and terminated
//! directly, and what makes that safe is the order: the process is **opened first**, SCM is asked
//! **again** whether it is still the service's, and only then is it terminated. A service restarted
//! between the first read and the open leaves a pid that now belongs to anyone; the second read
//! catches that, and the process is never terminated through a pid SCM no longer vouches for.
//!
//! A pure state machine over [`KillTarget`], unit-tested with a script in `kill_tests.rs`;
//! [`SystemKillTarget`] drives the real SCM and `cosca`'s identity-bound `Process::kill`.
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

/// The three things [`kill_service`] needs of the world, so its ordering is testable without one.
pub trait KillTarget {
    /// An opened process: the handle, or the identity, that terminating it goes through.
    type Process: Copy;

    /// The pid SCM reports for the service's process, or `None` when it reports none.
    fn service_pid(&mut self) -> io::Result<Option<u32>>;

    /// Open the process holding `pid`; `None` if there is none any more.
    fn open(&mut self, pid: u32) -> io::Result<Option<Self::Process>>;

    /// Terminate `process`.
    fn terminate(&mut self, process: Self::Process) -> io::Result<()>;
}

/// Terminate the service's process, or report that it has none.
///
/// Loops only when the service's state changed under it: the process was gone before it could be
/// opened, or SCM no longer vouches for the pid it was opened by. Each round re-reads SCM, which
/// settles on the new process or on none, so the loop follows the service rather than a count.
pub fn kill_service<T: KillTarget>(target: &mut T) -> io::Result<Killed> {
    loop {
        let Some(pid) = target.service_pid()? else {
            return Ok(Killed::NotRunning);
        };
        let Some(process) = target.open(pid)? else {
            continue;
        };
        if target.service_pid()? != Some(pid) {
            continue;
        }
        target.terminate(process)?;
        return Ok(Killed::Terminated);
    }
}

#[cfg(windows)]
pub use system::SystemKillTarget;

#[cfg(windows)]
mod system {
    use std::io;

    use cosca::Process;
    use cosca::identity::{ProcessId, Resolved};
    use windows_service::service::{Service, ServiceState};

    use super::KillTarget;

    /// The real SCM service, and `cosca` for the process.
    pub struct SystemKillTarget {
        pub service: Service,
    }

    impl KillTarget for SystemKillTarget {
        /// `(pid, start time)`, not a pid: terminating through it re-verifies the start time on the
        /// handle it opens, so a pid recycled by another process since the open is not terminated.
        type Process = ProcessId;

        fn service_pid(&mut self) -> io::Result<Option<u32>> {
            let status = self.service.query_status().map_err(|e| match e {
                windows_service::Error::Winapi(e) => e,
                other => io::Error::other(other),
            })?;
            // `Stopped` carries no pid; every other state, the pending ones included, has a process.
            Ok(match status.current_state {
                ServiceState::Stopped => None,
                _ => status.process_id,
            })
        }

        fn open(&mut self, pid: u32) -> io::Result<Option<ProcessId>> {
            match ProcessId::of(pid) {
                Resolved::Found(id) => Ok(Some(id)),
                Resolved::Gone => Ok(None),
                Resolved::Unknown => Err(io::Error::other(format!(
                    "the operating system refused to open process {pid}, the service's"
                ))),
            }
        }

        fn terminate(&mut self, process: ProcessId) -> io::Result<()> {
            Process::from_id(process).kill().map_err(io::Error::other)
        }
    }
}

#[cfg(test)]
#[path = "kill_tests.rs"]
mod kill_tests;
