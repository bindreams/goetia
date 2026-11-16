//! A process held through `cosca::Process`, for the Unix targets of `kill` (systemd's and launchd's):
//! a pidfd on Linux and an audit token on macOS, so the signal reaches the process opened and never a
//! later holder of its pid. SCM holds its own handle (`scm::kill`): cosca does not enable
//! `SeDebugPrivilege`, which a `LocalService` service's process needs.

use std::io;

use cosca::Process;
use cosca::identity::{Liveness, ProcessId, Resolved};

use super::{KillTarget, Waited};
use crate::backend::bounded::{EXIT_AFTER_REFUSED_KILL, settle_kill};
use crate::manager::budget::Deadline;

/// What a Unix manager (systemd, launchd) says of a service, for [`Signalling`] to hold, signal and
/// wait for the process by: the rest of a [`KillTarget`].
pub(crate) trait Reports {
    /// The pid the manager reports for the service's process, in whatever state, or `None` when it
    /// reports none.
    fn service_pid(&mut self) -> io::Result<Option<u32>>;

    /// Whether the manager's own lookup of the held process `pid`, by number, names it as the
    /// service's. For a manager that reports a pid it has not yet let go of after the process was
    /// reaped, so that an unrelated process that was given the number would pass the read of
    /// [`Self::service_pid`] alone. The default is a manager that does not.
    fn names(&mut self, _pid: u32) -> io::Result<bool> {
        Ok(true)
    }

    /// See [`KillTarget::await_recorded`].
    fn await_recorded(&mut self, pid: u32, held: Option<&Process>, deadline: Deadline) -> io::Result<Waited>;
}

/// The [`KillTarget`] of a service whose manager is `R`: the process is opened, signalled and waited for
/// through a `cosca::Process`, and `R` is the manager's side.
pub(crate) struct Signalling<R>(pub(crate) R);

impl<R: Reports> KillTarget for Signalling<R> {
    type Process = Process;

    fn service_pid(&mut self) -> io::Result<Option<u32>> {
        self.0.service_pid()
    }

    /// The manager shows `pid` and names the process held as the service's. A manager that shows `pid`
    /// without naming it has either reaped the process held, which is not the service's now (`false`:
    /// not signalled, and the manager's record is waited for), or the process held is alive and cannot
    /// be tied to the service: an error, since it is neither the service's nor an ended process.
    fn confirm(&mut self, pid: u32, process: &Process) -> io::Result<bool> {
        if self.0.service_pid()? != Some(pid) {
            return Ok(false);
        }
        if self.0.names(pid)? {
            return Ok(true);
        }
        match process.is_alive() {
            Liveness::Dead => Ok(false),
            Liveness::Alive | Liveness::Unknown => Err(io::Error::other(format!(
                "process {pid} could not be confirmed as the service's: the manager reports it, but its own lookup of the process does not name the service"
            ))),
        }
    }

    fn open(&mut self, pid: u32) -> io::Result<Option<Process>> {
        open(pid)
    }

    fn terminate(&mut self, process: &Process) -> io::Result<()> {
        terminate(process)
    }

    fn await_exit(&mut self, process: &Process, deadline: Deadline) -> io::Result<Waited> {
        await_exit(process, deadline)
    }

    fn await_recorded(&mut self, pid: u32, held: Option<&Process>, deadline: Deadline) -> io::Result<Waited> {
        self.0.await_recorded(pid, held, deadline)
    }
}

/// Open the process holding `pid`; `None` if there is none.
pub(crate) fn open(pid: u32) -> io::Result<Option<Process>> {
    Process::open_pid(pid).map_err(io::Error::other)
}

/// `SIGKILL` through the hold. A process that has exited is success, and so is one that is exiting:
/// on macOS a send by audit token refuses it with `EPERM`, which is success once the process is read
/// to have exited ([`settle_kill`]), and the refusal otherwise.
pub(crate) fn terminate(process: &Process) -> io::Result<()> {
    settle_kill(process.kill(), || process.is_alive(), EXIT_AFTER_REFUSED_KILL)
        .map(|_| ())
        .map_err(io::Error::other)
}

/// Block until the process has exited or `deadline` passes, [`Waited::Expired`] then. It is a zombie
/// by then, not necessarily reaped or recorded.
pub(crate) fn await_exit(process: &Process, deadline: Deadline) -> io::Result<Waited> {
    let exited = match deadline.remaining() {
        None => process.wait().map(|()| true),
        Some(left) => process.wait_timeout(left),
    }
    .map_err(io::Error::other)?;
    Ok(if exited { Waited::Confirmed } else { Waited::Expired })
}

/// Whether the manager has recorded the end of `pid`'s process, held through `held`: its report of the
/// service's pid is another pid or none, or is `pid` on a process other than the one held
/// ([`report_is_stale`]). The one rule of "recorded" for every manager that reports a pid.
pub(crate) fn recorded(reported: Option<u32>, held: Option<&Process>, pid: u32) -> io::Result<bool> {
    Ok(reported != Some(pid) || !report_is_stale(held, pid)?)
}

/// Whether the manager's report of `pid`, which it still shows after the process held through `held`
/// has exited, is still about that process: `true` while the manager has not yet recorded the exit,
/// `false` once `pid` names a process other than the one held. A manager that reaps an exit and
/// restarts the service can hand the new process the number the old one had, and a wait that only
/// compared numbers would never see the record.
///
/// An exited process that is not yet reaped still has its identity, so the report is the old one;
/// one that has been reaped is gone, and the report not yet updated. `held` is `None` when the
/// process was gone before it could be opened: any process that has `pid` now is another.
pub(crate) fn report_is_stale(held: Option<&Process>, pid: u32) -> io::Result<bool> {
    stale(held.map(Process::id), ProcessId::of(pid), pid)
}

/// [`report_is_stale`] over what was read.
fn stale(held: Option<ProcessId>, now: Resolved<ProcessId>, pid: u32) -> io::Result<bool> {
    match now {
        Resolved::Found(id) => Ok(Some(id) == held),
        Resolved::Gone => Ok(true),
        Resolved::Unknown => Err(io::Error::other(format!(
            "the manager still reports process {pid}, and the OS refused to say whether it is the one that was killed"
        ))),
    }
}

#[cfg(test)]
#[path = "process_tests.rs"]
pub(crate) mod process_tests;
