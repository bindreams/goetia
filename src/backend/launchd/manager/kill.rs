//! `kill` of a job's process: the target the state machine in `backend::kill` drives, for a service
//! and a task alike. The pid is read from `launchctl print`, the process is held and signalled through
//! an audit token (`backend::kill::process`; macOS 14.4 and later), and the wait for launchd's record
//! is the existing wait on `print` until the job no longer shows the killed process.
//!
//! The confirmation after the hold is `print` again, as `stop` has it, and then `launchctl procinfo` on
//! the held pid ([`Launchctl::names_launch`](super::Launchctl)). `print` can still show a pid after
//! launchd has reaped its process, and the number may by then be a stranger's, which is what was
//! opened: launchd's own lookup of the process held by its number, which names the job that owns it,
//! is what ties the process to the job. A process that exited meanwhile is not the job's and is not
//! signalled; a live one `print` shows that `procinfo` does not name is an error, never a kill that
//! signalled nothing and said it worked.

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
    /// The files of the one counted call, taken before anything is sent so that a
    /// [`Error::Reservation`] means nothing was: the confirmation's `procinfo` and the wait for the
    /// record, which follow each other, read through them.
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
        let mut files = self
            .kept
            .again()
            .map_err(|source| io::Error::other(Error::Reservation { id: None, source }))?;
        self.launchctl
            .names_launch(self.id, pid, &mut files)
            .map_err(io::Error::other)
    }

    /// Reads `print` until the job shows another pid, none, or is not held, or shows the killed pid's
    /// number on a process that is not the killed one ([`Until::Recorded`]): launchd learns of the exit
    /// after the process is a zombie, a throttled restart shows no pid, and an unthrottled one can be
    /// given the number.
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
