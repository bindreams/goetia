//! `kill` of a unit's main process: the target the state machine in `backend::kill` drives. The pid is
//! read with `systemctl show`, the process is held and signalled through a pidfd (`backend::kill::process`),
//! and the wait for the manager's record re-reads `MainPID` until it is not the killed pid.
//!
//! A task's job is its service unit, so a task's kill is a service's.

use std::io;
use std::time::Duration;

use cosca::Process;

use super::systemctl::Systemctl;
use crate::backend::kill::process::{Reports, Signalling};
use crate::backend::kill::{Waited, process};
use crate::manager::budget::Deadline;

/// How long [`UnitKill::await_recorded`] rests between two reads of `MainPID`. Pacing, never proof of
/// state.
const RECORD_RECHECK: Duration = Duration::from_millis(100);

/// One unit's main process, as the manager reports it.
pub(super) struct UnitKill<'a> {
    systemctl: &'a Systemctl,
    unit: String,
}

impl<'a> UnitKill<'a> {
    pub(super) fn new(systemctl: &'a Systemctl, unit: String) -> Signalling<Self> {
        Signalling(Self { systemctl, unit })
    }
}

impl Reports for UnitKill<'_> {
    /// `MainPID`: `None` when the unit has no main process. A unit systemd has not loaded is [`crate::error::Error::UnitNotLoaded`], never no process.
    fn service_pid(&mut self) -> io::Result<Option<u32>> {
        self.systemctl.main_pid_of(&self.unit).map_err(io::Error::other)
    }

    /// The process is a zombie once it has exited, which is not systemd's record of it: `MainPID` is
    /// cleared when systemd reaps it. Every read re-checks that. A `MainPID` that is the killed pid's number
    /// on another process is the record too: systemd reaped it and started the unit again, and the new
    /// process was given the number.
    fn await_recorded(&mut self, pid: u32, held: Option<&Process>, deadline: Deadline) -> io::Result<Waited> {
        loop {
            if process::recorded(self.service_pid()?, held, pid, || self.names(pid))? {
                return Ok(Waited::Confirmed);
            }
            if deadline.expired() {
                return Ok(Waited::Expired);
            }
            let pause = deadline
                .remaining()
                .map_or(RECORD_RECHECK, |left| left.min(RECORD_RECHECK));
            std::thread::sleep(pause);
        }
    }
}

#[cfg(test)]
#[path = "kill_tests.rs"]
mod kill_tests;
