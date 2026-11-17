//! A task's `start` and `stop`. Its job is a trigger-less launchd job that a timer, or a `start`,
//! kicks; neither verb ever unloads it.
//!
//! `start` loads the job from wherever its plist is and kicks it: it requests a run and returns, and a
//! run in flight is left alone (measured: `kickstart` of a running job does nothing). `stop` ends the
//! run in flight, and any run that begins while it does, as the task's `stop-action` says (SIGTERM, then
//! SIGKILL once its `timeout` is over; SIGKILL at once for `kill`), each sent through a hold on the process
//! and never by the label, and returns when launchd shows no process.

use std::time::{Duration, Instant};

use cosca::Process;
use cosca::identity::ProcessId;

use super::kill::JobKill;
use super::task::TaskPrint;
use super::watch::{Added, Watch};
use super::{Launchd, Plist, UNLOAD_POLL, Until, never_expires};
use crate::backend::bounded::{Reservation, Role};
use crate::backend::kill::{Hold, KillOutcome, KillTarget, Signal, Waited, hold_pid, kill_failed, kill_service};
use crate::backend::launchd::state::Evidence;
use crate::error::{Error, Result};
use crate::manager::budget::{self, Deadline};
use crate::manager::{Budget, State, Step, flow};
use crate::spec::{Id, StopAction, StopSignal};

/// What one pass of `stop` left to do.
#[derive(Debug, PartialEq, Eq)]
enum Pass {
    /// `print` showed no process and no start launchd has accepted.
    Done,
    /// Read again: a run ended, or may have begun.
    Again,
    /// Read again: a start launchd had accepted has forked, after a wait of this long under no budget.
    /// That wait is not the caller's to spend, so the caller's budget is extended by it.
    Queued(Duration),
    /// Read again, paced: launchd still shows what has ended (a pid that exited) or a job other than the
    /// one watched, and its view has to catch up. The pause is [`UNLOAD_POLL`], as for a wait on `print`: it
    /// spaces the reads and decides nothing.
    Behind,
}

/// What a task's stop sends and how long a run has to answer it: its `stop-action`. A job whose
/// metadata does not decode has none, and is sent SIGTERM and waited for as long as the caller's budget
/// allows, as before `stop-action`.
#[derive(Debug, Clone, Copy)]
struct Ending {
    /// `send-signal: kill`: SIGKILL at once.
    kill: bool,
    /// How long a run has to answer SIGTERM before it is sent SIGKILL. `None` never escalates.
    grace: Option<Duration>,
}

impl Ending {
    fn of(action: Option<StopAction>) -> Ending {
        Ending {
            kill: action.is_some_and(|action| action.send_signal == StopSignal::Kill),
            grace: action.and_then(|action| action.timeout),
        }
    }
}

/// A run that was sent SIGTERM, and when its grace ends. The grace is the process's own, by identity: it
/// began when that process was signalled.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Termed {
    run: ProcessId,
    /// `None` for a run with no grace: it is never sent SIGKILL.
    grace_end: Option<Deadline>,
}

/// What a `stop` carries from one pass to the next.
struct Course {
    ending: Ending,
    watch: Watch,
    started: Instant,
    /// What the stop has waited for under the caller's budget. A wait for a queued start's fork is under
    /// no budget, so [`Self::extend`] moves the budget, and the deadline, which [`Self::deadline`] derives
    /// from it, with it. Unused under `Immediate`, which waits for no exit.
    counted: Budget,
    /// The run last sent SIGTERM. A later pass can find the same run again (launchd's view lags, and a
    /// confirmation can refuse): it is not sent a second one, and its grace is the one that began at the
    /// first.
    termed: Option<Termed>,
}

impl Course {
    fn new(action: Option<StopAction>, budget: Budget, watch: Watch) -> Course {
        Course {
            ending: Ending::of(action),
            watch,
            started: Instant::now(),
            counted: budget,
            termed: None,
        }
    }

    fn deadline(&self) -> Deadline {
        self.counted.start_at(self.started)
    }

    fn extend(&mut self, by: Duration) {
        self.counted = self.counted.extended(by);
    }

    /// `run`'s SIGTERM, if an earlier pass sent it. Another identity is another run, with a grace of its
    /// own, whatever pid it has.
    fn termed(&self, run: ProcessId) -> Option<Termed> {
        self.termed.filter(|termed| termed.run == run)
    }
}

/// Whether `a` ends before `b`; no deadline never does.
fn ends_before(a: Deadline, b: Deadline) -> bool {
    match (a.at(), b.at()) {
        (Some(a), Some(b)) => a < b,
        (Some(_), None) => true,
        (None, _) => false,
    }
}

/// What a launchd task's `stop` had sent the run it was waiting for when its wait ran out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Escalation {
    /// SIGTERM, and the wait ended while the run still had the `stop-action` `timeout` to answer it.
    Pending,
    /// SIGTERM, and the run did not answer it within the `timeout`; the wait ended before SIGKILL was sent.
    Spent,
    /// SIGKILL, and the run is still there.
    Sent,
}

/// [`timed_out`] for a `stop` of a launchd task, which says what it had sent the run: goetia sends SIGKILL
/// after the SIGTERM's `timeout` only while it waits, so a wait that ended before it was sent says nothing
/// will, and what ends the run; one that ended after it was sent says that.
fn timed_out_stopping(id: &str, budget: Budget, sent: Escalation) -> Error {
    match budget::timed_out(id, "stopped", budget) {
        Error::WaitTimeout {
            id,
            awaited,
            waited,
            recovery,
        } => Error::WaitTimeout {
            recovery: match sent {
                Escalation::Pending => format!(
                    "{recovery} The run was sent SIGTERM and has the `stop-action` timeout to answer it; goetia \
                     sends SIGKILL after that only while it waits, so with this wait ended nothing will: \
                     `goetia daemon kill {id}` ends it."
                ),
                Escalation::Spent => format!(
                    "{recovery} The run was sent SIGTERM and did not answer it within the `stop-action` timeout; \
                     this wait ended before goetia sent SIGKILL, and nothing will: `goetia daemon kill {id}` ends it."
                ),
                Escalation::Sent => format!("{recovery} The run was sent SIGKILL and has not exited yet."),
            },
            id,
            awaited,
            waited,
        },
        other => other,
    }
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
    ///   it lasts as long as launchd's throttle, and `budget` is extended by exactly that wait: the
    ///   caller's bound is for the stop, not for launchd's throttle.
    /// - A pid: watches it, holds the process, confirms with launchd that it is the job's (as `kill`
    ///   does: [`hold_pid`]), signals it through the hold, and waits for the exit under `budget`
    ///   (`Immediate` does not wait). The signal is the `action`'s: SIGKILL for `send-signal: kill`, else
    ///   SIGTERM. Neither goes by the label, which names whatever runs under it when the signal lands, so
    ///   a process that is not the one held is never signalled, whatever pid launchd shows by then. A run
    ///   that ignores SIGTERM for the action's `timeout`, which starts at its SIGTERM (if the budget lasts
    ///   that long), is confirmed again and sent SIGKILL through the hold, unless the caller's deadline is
    ///   spent by then: that is the timeout, with no SIGKILL sent. It is waited for under `budget` too: the
    ///   `timeout` is the grace the user configured, as systemd's `TimeoutStopSec=` is. No `action`, or a
    ///   `timeout` of `null`, never escalates. The SIGTERM and its grace belong to the process's
    ///   identity, kept across passes: a pass that holds the same run again sends no second SIGTERM and
    ///   does not restart the grace. A run that began meanwhile is a real run and is ended too, held and
    ///   given a grace of its own, so the loop ends only when no process is left.
    ///
    /// A pid that exited before it was watched, or is not the job's any more, is the next pass, paced
    /// by [`UNLOAD_POLL`] while launchd's view catches up. The first pass draws on `reservation`, and each
    /// later one on one of its own: an uncapped loop would draw what was never reserved. A pass that
    /// cannot reserve returns that error, the SIGTERM standing. The one [`Watch`] is made first, so that
    /// failing to make it sends nothing.
    pub(super) fn stop_task(
        &self,
        id: &Id,
        action: Option<StopAction>,
        budget: Budget,
        reservation: &mut Reservation,
    ) -> Result<()> {
        let course = Course::new(action, budget, Watch::new()?);
        self.stop_course(id, course, reservation, |reservation| {
            JobKill::new(&self.launchctl, id.as_str(), reservation)
        })
    }

    /// [`Self::stop_task`] over `course`, with the target of each pass made by `target`: the one place
    /// that decides what signals are sent, over whatever sends them.
    fn stop_course<T: KillTarget<Process = Process>>(
        &self,
        id: &Id,
        mut course: Course,
        reservation: &mut Reservation,
        mut target: impl FnMut(&mut Reservation) -> Result<T>,
    ) -> Result<()> {
        let budget = course.counted;
        let mut own: Option<Reservation> = None;
        loop {
            let pass = own.as_mut().unwrap_or(&mut *reservation);
            match self.stop_pass(id, &mut course, pass, &mut target)? {
                Pass::Done => return Ok(()),
                Pass::Again => {}
                Pass::Queued(waited) => course.extend(waited),
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

    /// One pass of [`Self::stop_task`] over the run `print` shows. `course` is what the stop is held to and
    /// reports having waited, and what it has sent: the waits for queued starts are in its budget.
    fn stop_pass<T: KillTarget<Process = Process>>(
        &self,
        id: &Id,
        course: &mut Course,
        reservation: &mut Reservation,
        target: &mut impl FnMut(&mut Reservation) -> Result<T>,
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
                    let began = Instant::now();
                    self.await_queued_start(name, reservation)?;
                    Ok(Pass::Queued(began.elapsed()))
                }
                Quiet::Idle => Ok(Pass::Done),
            };
        };
        // Dropped on any way out of this pass, which deletes the registration and drains the queue: a
        // pid that is reused later cannot be handed this process's exit.
        let Added::Watching(registered) = course.watch.add(pid)? else {
            return Ok(Pass::Behind);
        };
        // The process is held, and only then does launchd confirm that it is the job's: a pid that was
        // read and then reused is a process launchd does not name, and is left alone. Launchd's record of
        // a run that has gone is not waited for here: the next pass reads it, paced.
        let mut target = target(reservation)?;
        let held = match hold_pid(&mut target, pid) {
            Ok(Hold::Held { process, .. }) => process,
            Ok(Hold::Ended { .. }) => return Ok(Pass::Behind),
            Err(error) => return Err(kill_failed(name, error.into())),
        };
        let run = held.id();
        let kills = course.ending.kill;
        let grace_end = match course.termed(run) {
            Some(termed) => termed.grace_end,
            None if kills => {
                target.signal(&held, Signal::Kill).map_err(|e| kill_failed(name, e))?;
                None
            }
            None => {
                target.signal(&held, Signal::Term).map_err(|e| kill_failed(name, e))?;
                // A run sent SIGTERM has its `timeout` to answer, if the caller waits that long.
                let grace_end = course.ending.grace.map(|grace| Budget::Bounded(grace).start());
                course.termed = Some(Termed { run, grace_end });
                grace_end
            }
        };
        if course.counted == Budget::Immediate {
            return Ok(Pass::Done);
        }
        let (budget, deadline) = (course.counted, course.deadline());
        let (until, escalating) = match grace_end {
            Some(end) if ends_before(end, deadline) => (end, true),
            _ => (deadline, false),
        };
        if registered.wait(Some(until))?.is_some() {
            return Ok(Pass::Again);
        }
        if !escalating {
            return Err(match (kills, grace_end) {
                (true, _) => timed_out_stopping(name, budget, Escalation::Sent),
                (false, Some(_)) => timed_out_stopping(name, budget, Escalation::Pending),
                (false, None) => budget::timed_out(name, "stopped", budget),
            });
        }
        // The grace is over and the run is still there: launchd is asked again whether the process held is
        // the job's, which it is not if the run has gone and the job has another, and then it is killed
        // through the hold, never by its pid or by the label.
        if !target.confirm(pid, &held).map_err(|e| kill_failed(name, e))? {
            return Ok(Pass::Behind);
        }
        // A deadline that is spent is the caller's timeout, and nothing is sent after it.
        if deadline.expired() {
            return Err(timed_out_stopping(name, budget, Escalation::Spent));
        }
        target.signal(&held, Signal::Kill).map_err(|e| kill_failed(name, e))?;
        // The wait for the exit is on the hold too: the watch is spent by the wait that ran out.
        match target.await_exit(&held, deadline).map_err(|e| kill_failed(name, e))? {
            Waited::Confirmed => Ok(Pass::Again),
            Waited::Expired => Err(timed_out_stopping(name, budget, Escalation::Sent)),
        }
    }
}

#[cfg(test)]
#[path = "verbs_tests.rs"]
mod verbs_tests;
