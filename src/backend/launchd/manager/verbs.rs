//! A task's `start` and `stop`. Its job is a trigger-less launchd job that a timer, or a `start`,
//! kicks; neither verb ever unloads it.
//!
//! `start` loads the job from wherever its plist is and kicks it: it requests a run and returns, and a
//! run in flight is left alone (measured: `kickstart` of a running job does nothing). `stop` ends the
//! run in flight, and any run that begins while it does, with SIGTERM by label, and returns when
//! launchd shows no process.

use super::watch::{Added, Watch};
use super::{Launchd, Plist, SignalOutcome, UNLOAD_POLL, Until, never_expires};
use crate::backend::bounded::{Reservation, Role};
use crate::backend::launchd::state::Evidence;
use crate::error::{Error, Result};
use crate::manager::budget::{self, Deadline};
use crate::manager::{Budget, State, Step, flow};
use crate::spec::Id;

/// What one pass of `stop` left to do.
#[derive(Debug, PartialEq, Eq)]
enum Pass {
    /// `print` showed no process and no start launchd has accepted.
    Done,
    /// Read again: a run ended, or may have begun.
    Again,
    /// Read again, paced: launchd still shows what has ended (a pid that exited, a process a signal
    /// found none for) or a job other than the one watched, and its view has to catch up. The pause is
    /// [`UNLOAD_POLL`], as for a wait on `print`: it spaces the reads and decides nothing.
    Behind,
}

impl Launchd {
    /// Loads the job from where its plist is, waiting out one that is being torn down, then a plain
    /// `launchctl kickstart system/<id>`: no `-p`, so the pid is not waited for, and no state read
    /// before it, so no budget is read. The CLI refuses `--timeout` per id for a task.
    pub(super) fn start_task(&self, snapshot: Plist, reservation: &mut Reservation) -> Result<()> {
        let path = snapshot.job()?.location.path.clone();
        let id = snapshot.id.as_str();
        self.launchctl.load_job(id, &path, reservation)?;
        never_expires(self.launchctl.kickstart(
            id,
            Budget::Immediate,
            Budget::Unbounded.start(),
            Evidence::Queued,
            reservation,
        )?);
        Ok(())
    }

    /// Ends the run in flight, and returns once launchd shows no process. Each pass reads `print`:
    ///
    /// - No process and an idle job (`not running`, `exited`, or a label launchd does not hold): done.
    ///   No process and any other state, or a `print` that failed or was not whole, is
    ///   [`Error::StopUnconfirmed`]: a stop that cannot tell that no process is left does not report that
    ///   it stopped, and a SIGTERM an earlier pass sent stands.
    /// - A start accepted and not yet forked (`spawn scheduled`, no pid): waits for the pid, which a
    ///   signal cannot reach before (`launchctl kill` of that state answers "No process to signal"
    ///   and does not cancel the start). That wait is on `print`, and under no budget, whatever it is:
    ///   it lasts as long as launchd's throttle.
    /// - A pid: watches it, confirms that `print` still shows it, signals SIGTERM by label, and waits
    ///   for the exit under `budget` (`Immediate` does not wait). A run that began meanwhile is a real
    ///   run and is ended too, so the loop ends only when no process is left.
    ///
    /// A pid that exited before it was watched, or is not the job's any more, is the next pass, paced
    /// by [`UNLOAD_POLL`] while launchd's view catches up. The first pass draws on `reservation`, and each
    /// later one on one of its own: an uncapped loop would draw what was never reserved. A pass that
    /// cannot reserve returns that error, the SIGTERM standing. The one [`Watch`] is made first, so that
    /// failing to make it sends nothing.
    pub(super) fn stop_task(&self, id: &Id, budget: Budget, reservation: &mut Reservation) -> Result<()> {
        let watch = Watch::new()?;
        // From the verb's entry. Unused under `Immediate`, which waits for no exit.
        let deadline = budget.start();
        let mut own: Option<Reservation> = None;
        loop {
            let pass = own.as_mut().unwrap_or(&mut *reservation);
            match self.stop_pass(id, budget, deadline, &watch, pass)? {
                Pass::Done => return Ok(()),
                Pass::Again => {}
                Pass::Behind => std::thread::sleep(UNLOAD_POLL),
            }
            own = Some(flow::reserved(self, id, &[Step::Stop], budget)?);
        }
    }

    fn stop_pass(
        &self,
        id: &Id,
        budget: Budget,
        deadline: Deadline,
        watch: &Watch,
        reservation: &mut Reservation,
    ) -> Result<Pass> {
        let name = id.as_str();
        // Unbounded: `print` answers at once, and its body is whole.
        let issuing = Budget::Unbounded.start();
        let print = self
            .launchctl
            .read_task_print(name, issuing, Role::Query, reservation)?;
        let Some(pid) = print.pid else {
            if print.queued {
                let kept = reservation
                    .keep(Role::COUNTED_FILES)
                    .map_err(|source| Error::Reservation { id: None, source })?;
                never_expires(
                    self.launchctl
                        .wait_print(name, issuing, &kept, Until::ProcessOrNotScheduled)?,
                );
                return Ok(Pass::Again);
            }
            // Idle: no process, and launchd holds no start. Anything else with no pid, or a read that
            // was not whole, does not establish that none is left, so the stop is not reported done.
            return match print.state {
                State::Idle => Ok(Pass::Done),
                _ => Err(Error::StopUnconfirmed {
                    id: id.to_string(),
                    shown: print.shown.describe(print.state),
                }),
            };
        };
        if watch.add(pid)? == Added::Gone {
            return Ok(Pass::Behind);
        }
        // The pid was read before it was watched, and a pid can be reused: only a job that `print`
        // still shows with this pid is the one the watch holds.
        let still = self
            .launchctl
            .read_task_print(name, issuing, Role::Query, reservation)?;
        if still.pid != Some(pid) {
            return Ok(Pass::Behind);
        }
        if self.launchctl.signal(name, "SIGTERM", reservation)? == SignalOutcome::NoProcess {
            return Ok(Pass::Behind);
        }
        if budget == Budget::Immediate {
            return Ok(Pass::Done);
        }
        match watch.wait(pid, Some(deadline))? {
            Some(_) => Ok(Pass::Again),
            None => Err(budget::timed_out(name, "stopped", budget)),
        }
    }
}

#[cfg(test)]
#[path = "verbs_tests.rs"]
mod verbs_tests;
