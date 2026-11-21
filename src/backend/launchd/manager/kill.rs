//! `kill` of a job's process: the target the state machine in `backend::kill` drives, for a service
//! and a task alike. The pid is read from `launchctl print`, the process is held and signalled through
//! an audit token (`backend::kill::process`), and the wait for launchd's record
//! is the existing wait on `print` until the job no longer shows the killed process.
//!
//! The confirmation after the hold is `print` again, then `launchctl procinfo` on the held pid
//! ([`Launchctl::names_launch`](super::Launchctl)): `print` can show a pid after launchd reaped its
//! process, so procinfo ties the process to the job. A process that exited meanwhile, or that procinfo
//! does not name, is not the job's and is not signalled; the kill waits for launchd's record.

use std::io;

use cosca::Process;

use super::{Launchctl, Until};
use crate::backend::bounded::{Kept, Reservation, Role};
use crate::backend::kill::Waited;
use crate::backend::kill::process::{Reports, Signalling};
use crate::error::{Error, Result};
use crate::manager::budget::Deadline;

/// One job's process, as launchd reports it.
pub(super) struct JobKill<'a> {
    launchctl: &'a Launchctl,
    id: &'a str,
    /// The files of the one counted call, taken up front so a [`Error::Reservation`] is raised before
    /// any read; `procinfo` and the wait for the record read through them.
    kept: Kept,
}

impl<'a> JobKill<'a> {
    pub(super) fn new(
        launchctl: &'a Launchctl,
        id: &'a str,
        reservation: &mut Reservation,
    ) -> Result<Signalling<Self>> {
        let kept = reservation
            .keep(Role::COUNTED_FILES)
            .map_err(|source| Error::Reservation { id: None, source })?;
        Ok(Signalling(Self { launchctl, id, kept }))
    }
}

impl Reports for JobKill<'_> {
    /// The pid `print` shows: `None` for a job launchd does not hold, or holds with no process.
    fn service_pid(&mut self) -> io::Result<Option<u32>> {
        self.launchctl.job_pid(self.id).map_err(io::Error::other)
    }

    /// Whether `launchctl procinfo` names `pid` as a launch of the job.
    fn names(&mut self, pid: u32) -> io::Result<bool> {
        self.launchctl
            .names_launch_again(self.id, pid, &self.kept)
            .map_err(io::Error::other)
    }

    /// Reads `print` until the job shows another pid, none, or is not held, or shows the killed pid's
    /// number on a new process of the job's ([`Until::Recorded`]): launchd learns of the exit after the
    /// process is a zombie, a throttled restart shows no pid, and an unthrottled one can be given the
    /// number.
    fn await_recorded(&mut self, pid: u32, held: Option<&Process>, deadline: Deadline) -> io::Result<Waited> {
        match self
            .launchctl
            .wait_print(self.id, deadline, &self.kept, Until::Recorded { pid, held })
            .map_err(io::Error::other)?
        {
            Some(()) => Ok(Waited::Confirmed),
            None => Ok(Waited::Expired),
        }
    }
}
