//! A task's `start` and `stop`. Its job is a trigger-less launchd job that a timer, or a `start`,
//! kicks; neither verb ever unloads it.
//!
//! `start` loads the job from wherever its plist is and kicks it: it requests a run and returns, and a
//! run in flight is left alone (measured: `kickstart` of a running job does nothing). `stop` ends the
//! run in flight, and any run that begins while it does, with SIGTERM by label, and returns when
//! launchd shows no process.

use super::kill::JobKill;
use super::task::TaskPrint;
use super::watch::{Added, Watch};
use super::{Launchd, Plist, SignalOutcome, UNLOAD_POLL, Until, never_expires};
use crate::backend::bounded::{Reservation, Role};
use crate::backend::kill::{KillOutcome, kill_failed, kill_service};
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

/// What a `print` that shows no pid says of the job.
pub(super) enum Quiet {
    /// A start launchd has accepted and not yet forked: a run is coming.
    Queued,
    /// No process, and launchd holds no start (`not running`, `exited`, or a label it does not hold).
    Idle,
}

/// `print`, which shows no pid, read as a verb that needs to know no process is left reads it. Anything
/// else with no pid, or a read that was not whole, does not establish that none is left:
/// [`Error::StopUnconfirmed`], which `stop` and a start sent right after an unwaited stop share.
pub(super) fn quiet(id: &Id, print: &TaskPrint) -> Result<Quiet> {
    if print.queued {
        return Ok(Quiet::Queued);
    }
    match print.state {
        State::Idle => Ok(Quiet::Idle),
        _ => Err(Error::StopUnconfirmed {
            id: id.to_string(),
            shown: print.shown.describe(print.state),
        }),
    }
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

    /// SIGKILL to the run in flight, and the wait for launchd to stop showing it (see [`kill_service`]).
    /// A start launchd has accepted and not yet forked (`spawn scheduled`, no pid) is a run, as `status`
    /// and `stop` have it, but a signal reaches nothing before the fork, and the run it begins would not
    /// be killed. So a kill that finds no process reads `print` once more. A start that is queued is
    /// waited for until it forks, under no budget as `stop`'s wait is, and the kill is made again; a
    /// pid shown is a fork that came after the first look, and is killed. An idle `print` is
    /// [`Error::NotRunning`]; a read that failed or was not whole, or a no-pid state that is neither
    /// idle nor queued, is [`Error::StopUnconfirmed`] (see [`quiet`]). Draws the kill's wait, a read and
    /// a wait for the fork at most.
    pub(super) fn kill_task(&self, id: &Id, reservation: &mut Reservation) -> Result<()> {
        let name = id.as_str();
        let mut target = JobKill::new(&self.launchctl, name, reservation)?;
        if kill_service(&mut target, name).map_err(|e| kill_failed(name, e))? == KillOutcome::Ended {
            return Ok(());
        }
        let print = self
            .launchctl
            .read_task_print(name, Budget::Unbounded.start(), Role::Query, reservation)?;
        if print.pid.is_none() {
            match quiet(id, &print)? {
                Quiet::Idle => return Err(Error::NotRunning { id: id.to_string() }),
                Quiet::Queued => self.await_queued_start(name, reservation)?,
            }
        }
        match kill_service(&mut target, name).map_err(|e| kill_failed(name, e))? {
            KillOutcome::Ended => Ok(()),
            KillOutcome::NotRunning => Err(Error::NotRunning { id: id.to_string() }),
        }
    }

    /// Waits for the start launchd has accepted for `name` to fork, or to turn out not to happen (see
    /// [`Until::ProcessOrNotScheduled`]), under no budget. One call, drawing a call's files.
    fn await_queued_start(&self, name: &str, reservation: &mut Reservation) -> Result<()> {
        let kept = reservation
            .keep(Role::COUNTED_FILES)
            .map_err(|source| Error::Reservation { id: None, source })?;
        never_expires(self.launchctl.wait_print(
            name,
            Budget::Unbounded.start(),
            &kept,
            Until::ProcessOrNotScheduled,
        )?);
        Ok(())
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
            return match quiet(id, &print)? {
                Quiet::Queued => {
                    self.await_queued_start(name, reservation)?;
                    Ok(Pass::Again)
                }
                Quiet::Idle => Ok(Pass::Done),
            };
        };
        // Dropped on any way out of this pass, which deletes the registration and drains the queue: a
        // pid that is reused later cannot be handed this process's exit.
        let Added::Watching(registered) = watch.add(pid)? else {
            return Ok(Pass::Behind);
        };
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
        match registered.wait(Some(deadline))? {
            Some(_) => Ok(Pass::Again),
            None => Err(budget::timed_out(name, "stopped", budget)),
        }
    }
}

#[cfg(test)]
#[path = "verbs_tests.rs"]
mod verbs_tests;
