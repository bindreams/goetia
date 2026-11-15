//! A process held through `cosca::Process`, for the Unix targets of `kill` (systemd's and launchd's):
//! a pidfd on Linux and an audit token on macOS, so the signal reaches the process opened and never a
//! later holder of its pid. SCM holds its own handle (`scm::kill`): cosca does not enable
//! `SeDebugPrivilege`, which a `LocalService` service's process needs.

use std::io;

use cosca::Process;
use cosca::identity::{Liveness, ProcessId, Resolved};

/// Open the process holding `pid`; `None` if there is none.
pub(crate) fn open(pid: u32) -> io::Result<Option<Process>> {
    Process::open_pid(pid).map_err(io::Error::other)
}

/// `SIGKILL` through the hold. A process that has exited is success.
pub(crate) fn terminate(process: &Process) -> io::Result<()> {
    process.kill().map_err(io::Error::other)
}

/// Whether the process has exited (is a zombie or gone). A look that cannot tell reads as "not yet":
/// the wait that follows it is the proof either way.
pub(crate) fn has_exited(process: &Process) -> io::Result<bool> {
    Ok(process.is_alive() == Liveness::Dead)
}

/// Block until the process has exited. It is a zombie by then, not necessarily reaped or recorded.
pub(crate) fn await_exit(process: &Process) -> io::Result<()> {
    process.wait().map_err(io::Error::other)
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
