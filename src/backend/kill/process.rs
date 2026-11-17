//! A process held through `cosca::Process`, for the Unix targets of `kill` (systemd's and launchd's):
//! a pidfd on Linux and an audit token on macOS, so the signal reaches the process opened and never a
//! later holder of its pid. SCM holds its own handle (`scm::kill`): cosca does not enable
//! `SeDebugPrivilege`, which a `LocalService` service's process needs.

use std::io;

use cosca::Process;
use cosca::identity::{ProcessId, Resolved};

use super::{KillTarget, Signal, Waited};
use crate::backend::bounded::{EXIT_AFTER_REFUSED_KILL, settle_kill};
use crate::manager::budget::Deadline;

/// What a Unix manager (systemd, launchd) reports of a service; [`Signalling`] adds the process half of
/// a [`KillTarget`].
pub(crate) trait Reports {
    /// The pid the manager reports for the service's process, in whatever state, or `None` when it
    /// reports none.
    fn service_pid(&mut self) -> io::Result<Option<u32>>;

    /// Whether the manager's own lookup of `pid` names the process now holding it as the service's; for
    /// a manager that keeps reporting a pid after it reaped the process. Default: `true`.
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

    /// The manager shows `pid` and its own lookup names the process held as the service's. One it shows
    /// without naming is a stranger that was given the number after the manager reaped the service's
    /// process, whether or not it is alive: not signalled, and the manager's record is waited for.
    fn confirm(&mut self, pid: u32, _process: &Process) -> io::Result<bool> {
        Ok(self.0.service_pid()? == Some(pid) && self.0.names(pid)?)
    }

    fn open(&mut self, pid: u32) -> io::Result<Option<Process>> {
        open(pid)
    }

    fn signal(&mut self, process: &Process, signal: Signal) -> io::Result<()> {
        signal_through(process, signal)
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

/// `signal` through the hold: only to the process opened, never to a later holder of its pid, and not
/// to its children. On macOS a send by audit token refuses an exiting process with `EPERM`, which is
/// success once the process is read to have exited ([`settle_kill`]), and the refusal otherwise.
pub(crate) fn signal_through(process: &Process, signal: Signal) -> io::Result<()> {
    let sent = match signal {
        Signal::Term => process.terminate(),
        Signal::Kill => process.kill(),
    };
    settle_kill(sent, || process.is_alive(), EXIT_AFTER_REFUSED_KILL)
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
/// service's pid is another pid or none, or is `pid` on a process other than the one held that the
/// manager's own lookup `names` as the service's ([`report_is_stale`]). The one rule of "recorded" for
/// every manager that reports a pid.
pub(crate) fn recorded(
    reported: Option<u32>,
    held: Option<&Process>,
    pid: u32,
    names: impl FnOnce() -> io::Result<bool>,
) -> io::Result<bool> {
    Ok(reported != Some(pid) || !report_is_stale(held, pid, names)?)
}

/// Whether the manager's report of `pid`, which it still shows after the process held through `held`
/// has exited, is still about that process: `true` while the manager has not yet recorded the exit.
///
/// An exited process that is not yet reaped still has its identity, so the report is the old one; one
/// that has been reaped is gone, and the report not yet updated. Another process at `pid` is the
/// service's new process when `names` says the manager's own lookup names it, as for systemd, whose
/// `MainPID` is cleared at the reap and which has no lookup to make. Otherwise it is a stranger that
/// was given the number while a manager whose report lags (launchd) still shows it, and the report is
/// stale. `held` is `None` when the process was gone before it could be opened.
pub(crate) fn report_is_stale(
    held: Option<&Process>,
    pid: u32,
    names: impl FnOnce() -> io::Result<bool>,
) -> io::Result<bool> {
    stale(held.map(Process::id), ProcessId::of(pid), pid, names)
}

fn stale(
    held: Option<ProcessId>,
    now: Resolved<ProcessId>,
    pid: u32,
    names: impl FnOnce() -> io::Result<bool>,
) -> io::Result<bool> {
    match now {
        Resolved::Found(id) if Some(id) == held => Ok(true),
        Resolved::Found(_) => Ok(!names()?),
        Resolved::Gone => Ok(true),
        Resolved::Unknown => Err(io::Error::other(format!(
            "the manager still reports process {pid}, and the OS refused to say whether it is the one that was killed"
        ))),
    }
}

#[cfg(test)]
#[path = "process_tests.rs"]
pub(crate) mod process_tests;
