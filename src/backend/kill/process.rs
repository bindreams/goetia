//! A process held through `cosca::Process`, for the Unix targets of `kill` (systemd's and launchd's):
//! a pidfd on Linux and an audit token on macOS, so the signal reaches the process opened and never a
//! later holder of its pid. SCM holds its own handle (`scm::kill`): cosca does not enable
//! `SeDebugPrivilege`, which a `LocalService` service's process needs.

use std::io;

use cosca::Process;

/// Open the process holding `pid`; `None` if there is none.
pub(crate) fn open(pid: u32) -> io::Result<Option<Process>> {
    Process::open_pid(pid).map_err(io::Error::other)
}

/// `SIGKILL` through the hold. A process that has exited is success.
pub(crate) fn terminate(process: &Process) -> io::Result<()> {
    process.kill().map_err(io::Error::other)
}

/// Block until the process has exited. It is a zombie by then, not necessarily reaped or recorded.
pub(crate) fn await_exit(process: &Process) -> io::Result<()> {
    process.wait().map_err(io::Error::other)
}
