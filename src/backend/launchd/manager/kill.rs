//! `kill` of a job's process: the target the state machine in `backend::kill` drives, for a service
//! and a task alike. The pid is read from `launchctl print`, the process is held and signalled through
//! an audit token (`backend::kill::process`; macOS 14.4 and later), and the wait for launchd's record
//! is the existing wait on `print` until the job no longer shows the killed pid.
//!
//! The confirmation after the hold is `print` again, as `stop` has it, and not `procinfo`
//! ([`Launchctl::names_launch`](super::Launchctl)): the question the machine asks is whether the
//! manager still reports the pid as the job's, and `print` is how it is asked everywhere else.

use std::io;

use cosca::Process;

use super::{Launchctl, Until};
use crate::backend::bounded::{Kept, Reservation, Role};
use crate::backend::kill::{KillTarget, process};
use crate::backend::scm::wait::Waited;
use crate::error::{Error, Result};
use crate::manager::budget::Deadline;

/// One job's process, as launchd reports it.
pub(super) struct JobKill<'a> {
    launchctl: &'a Launchctl,
    id: &'a str,
    /// The files of the one counted call, the wait for the record, taken before anything is sent so
    /// that a [`Error::Reservation`] means nothing was.
    kept: Kept,
}

impl<'a> JobKill<'a> {
    pub(super) fn new(launchctl: &'a Launchctl, id: &'a str, reservation: &mut Reservation) -> Result<Self> {
        let kept = reservation
            .keep(Role::COUNTED_FILES)
            .map_err(|source| Error::Reservation { id: None, source })?;
        Ok(Self { launchctl, id, kept })
    }
}

impl KillTarget for JobKill<'_> {
    type Process = Process;

    /// The pid `print` shows: `None` for a job launchd does not hold, or holds with no process.
    fn service_pid(&mut self) -> io::Result<Option<u32>> {
        self.launchctl.job_pid(self.id).map_err(io::Error::other)
    }

    fn open(&mut self, pid: u32) -> io::Result<Option<Process>> {
        process::open(pid)
    }

    fn terminate(&mut self, held: &Process) -> io::Result<()> {
        process::terminate(held)
    }

    fn await_exit(&mut self, held: &Process) -> io::Result<()> {
        process::await_exit(held)
    }

    /// Reads `print` until the job shows another pid, none, or is not held ([`Until::PidNot`]): launchd
    /// learns of the exit after the process is a zombie, and a throttled restart shows no pid.
    fn await_recorded(&mut self, pid: u32, deadline: Deadline) -> io::Result<Waited> {
        match self
            .launchctl
            .wait_print(self.id, deadline, &self.kept, Until::PidNot(pid))
            .map_err(io::Error::other)?
        {
            Some(()) => Ok(Waited::Confirmed),
            None => Ok(Waited::Expired),
        }
    }
}
