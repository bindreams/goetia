//! A task's `start` and `stop`. Its job is a trigger-less launchd job that a timer, or a `start`,
//! kicks; neither verb ever unloads it.
//!
//! `start` loads the job from wherever its plist is and kicks it: it requests a run and returns, and a
//! run in flight is left alone (measured: `kickstart` of a running job does nothing). `stop` ends the
//! run in flight, and any run that begins while it does, as the task's `stop-action` says (SIGTERM, then
//! SIGKILL once its `timeout` is over; SIGKILL at once for `kill`), each sent through a hold on the process
//! and never by the label, and returns when launchd shows no process.

use std::io;
use std::time::{Duration, Instant};

use cosca::Process;
use cosca::identity::{ProcessId, Resolved};

use super::kill::JobKill;
use super::task::TaskPrint;
use super::watch::{Added, Watch};
use super::{Launchd, Plist, TASK_STOP_CALLS, TASK_STOP_REQUESTS, UNLOAD_POLL, Until, never_expires, own_reservation};
use crate::backend::bounded::{Reservation, Role};
use crate::backend::kill::process::{Signal, send_signal};
use crate::backend::kill::{Hold, KillOutcome, KillTarget as _, failed_as, hold_pid, kill_failed, kill_service};
use crate::backend::launchd::state::Evidence;
use crate::error::{Error, Result};
use crate::manager::budget::{self, Deadline};
use crate::manager::{Budget, State};
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

/// A run this stop has signalled, and when its grace ends. The grace is the process's own, by identity: it
/// began when that process was sent SIGTERM.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Termed {
    run: ProcessId,
    /// `None` for a run with no grace: it is never sent SIGKILL after a SIGTERM.
    grace_end: Option<Deadline>,
    /// SIGKILL has been sent to it, at once (`send-signal: kill`) or when its grace was over.
    killed: bool,
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
    /// The run last signalled. A later pass can find the same run again (launchd's view lags, and a
    /// confirmation can refuse): it is not sent a second SIGTERM, and its grace is the one that began at
    /// the first.
    termed: Option<Termed>,
    /// The first pass is over: a pass that was not the wait for a queued start has ended. That pass always
    /// runs, and the wait for a queued start and the signal to the run found after it are part of it.
    first_over: bool,
}

impl Course {
    fn new(action: Option<StopAction>, budget: Budget, watch: Watch) -> Course {
        Course {
            ending: Ending::of(action),
            watch,
            started: Instant::now(),
            counted: budget,
            termed: None,
            first_over: false,
        }
    }

    fn deadline(&self) -> Deadline {
        self.counted.start_at(self.started)
    }

    fn extend(&mut self, by: Duration) {
        self.counted = self.counted.extended(by);
    }

    /// Whether the caller's deadline is spent. The first pass always runs, whatever the budget, as every
    /// backend's stop is issued: until it is over, or something has been sent, the deadline is not read. After
    /// that every pass reads it first, whether anything was sent or not. `Immediate` waits for nothing, so
    /// it has no deadline to spend.
    fn spent(&self) -> bool {
        (self.first_over || self.termed.is_some())
            && matches!(self.counted, Budget::Bounded(_))
            && self.deadline().expired()
    }

    /// Records that the first pass is over.
    fn first_pass_over(&mut self) {
        self.first_over = true;
    }

    /// What this stop did to `run`, the run that is up when its deadline is spent: the signals it was sent,
    /// or that it was sent none. `None` for a run sent SIGTERM with no grace to follow it.
    fn escalation(&self, run: ProcessId) -> Option<Escalation> {
        match self.termed(run) {
            None => Some(Escalation::Unsent),
            Some(Termed { killed: true, .. }) => Some(Escalation::Sent),
            Some(Termed {
                grace_end: Some(end), ..
            }) => Some(if end.expired() {
                Escalation::Spent
            } else {
                Escalation::Pending
            }),
            Some(Termed { grace_end: None, .. }) => None,
        }
    }

    /// `run`'s signals, if an earlier pass sent any. Another identity is another run, with a grace of its
    /// own, whatever pid it has.
    fn termed(&self, run: ProcessId) -> Option<Termed> {
        self.termed.filter(|termed| termed.run == run)
    }

    /// The signal due `run`, which a pass has held: its first, or SIGKILL once its grace is over. None for
    /// a run that has been sent what it is due and whose grace is not over.
    fn due(&self, run: ProcessId) -> Option<Signal> {
        match self.termed(run) {
            None if self.ending.kill => Some(Signal::Kill),
            None => Some(Signal::Terminate),
            Some(termed) if !termed.killed && termed.grace_end.is_some_and(|end| end.expired()) => Some(Signal::Kill),
            Some(_) => None,
        }
    }

    /// Records that `run` was sent `signal`. A run with a SIGTERM has its `timeout` to answer it, if the
    /// caller waits that long.
    fn sent(&mut self, run: ProcessId, signal: Signal) {
        self.termed = Some(match (signal, self.termed(run)) {
            (Signal::Terminate, _) => Termed {
                run,
                grace_end: self.ending.grace.map(|grace| Budget::Bounded(grace).start()),
                killed: false,
            },
            (Signal::Kill, Some(termed)) => Termed { killed: true, ..termed },
            (Signal::Kill, None) => Termed {
                run,
                grace_end: None,
                killed: true,
            },
        });
    }

    /// When a pass stops waiting for `run`: at the end of its grace, if that comes before the deadline and
    /// SIGKILL is still to be sent, else at the deadline.
    fn wait_until(&self, run: ProcessId) -> Deadline {
        let deadline = self.deadline();
        match self.termed(run) {
            Some(Termed {
                grace_end: Some(end),
                killed: false,
                ..
            }) if ends_before(end, deadline) => end,
            _ => deadline,
        }
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
    /// Nothing: the run that is up was not signalled before the deadline.
    Unsent,
    /// SIGTERM, and the wait ended while the run still had the `stop-action` `timeout` to answer it.
    Pending,
    /// SIGTERM, and the run did not answer it within the `timeout`; the wait ended before SIGKILL was sent.
    Spent,
    /// SIGKILL, and the run is still there.
    Sent,
}

/// [`budget::timed_out`] for a `stop` of a launchd task, which says what it had sent the run: goetia sends SIGKILL
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
                Escalation::Unsent => format!(
                    "{recovery} The run was not signalled before the deadline; nothing will signal it: \
                     `goetia daemon kill {id}` ends it."
                ),
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

    /// Ends the run in flight, and returns once launchd shows no process. The stop is always issued, as every
    /// backend's is, whatever the budget: the first pass always runs, and the wait for a queued start and the
    /// signal to the run found after it are part of it. From the second pass on every pass starts with the
    /// deadline, whether anything was sent or not: a `Bounded` budget that is spent is the caller's timeout,
    /// and nothing more is sent ([`Course::spent`]); the one read the pass makes then is to see whether the
    /// job is done ([`Self::done_or`]). So a stop that keeps finding a pid launchd has not cleared ends at its
    /// deadline. Otherwise it reads `print`:
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
    ///   does: [`hold_pid`]), sends it the signal it is due through the hold, and waits for the exit up to
    ///   the deadline (`Immediate` does not wait). A run is due SIGKILL for `send-signal: kill`, else
    ///   SIGTERM. When the action's `timeout`, which starts at its SIGTERM, ends inside the wait, the pass
    ///   confirms the process again and sends it SIGKILL, unless the deadline is spent by then. Neither
    ///   goes by the label, which names whatever runs under it when the signal lands, so a process that is
    ///   not the one held is never signalled, whatever pid launchd shows by then. No `action`, or a
    ///   `timeout` of `null`, never escalates. The signals sent and the grace belong to the process's
    ///   identity, kept across passes: a pass that holds the same run again sends no second SIGTERM and
    ///   does not restart the grace. A run that began meanwhile is a real run and is ended too, held and
    ///   given a grace of its own, so the loop ends only when no process is left.
    ///
    /// The deadline is decided before a send, not atomically with it: one that passes between the check and
    /// the send does not stop that send. A pid that exited before it was watched, or is not the job's any
    /// more, is the next pass, paced by [`UNLOAD_POLL`] while launchd's view catches up. The first pass draws
    /// on `reservation`, and each later one on one of its own: an uncapped loop would draw what was never
    /// reserved. A pass that cannot reserve returns that error, the SIGTERM standing. The one [`Watch`] is
    /// made first, so that failing to make it sends nothing.
    pub(super) fn stop_task(
        &self,
        id: &Id,
        action: Option<StopAction>,
        budget: Budget,
        reservation: &mut Reservation,
    ) -> Result<()> {
        let mut course = Course::new(action, budget, Watch::new()?);
        let mut own: Option<Reservation> = None;
        loop {
            let pass = own.as_mut().unwrap_or(&mut *reservation);
            if course.spent() {
                return self.done_or(id, &course, pass);
            }
            match self.stop_pass(id, &mut course, pass)? {
                Pass::Done => return Ok(()),
                Pass::Again => course.first_pass_over(),
                Pass::Queued(waited) => course.extend(waited),
                Pass::Behind => {
                    course.first_pass_over();
                    std::thread::sleep(UNLOAD_POLL);
                }
            }
            own = Some(own_reservation(id, TASK_STOP_REQUESTS, TASK_STOP_CALLS)?);
        }
    }

    /// What a stop whose deadline is spent ends with, after one read of `print` on `reservation` and no loop:
    /// the overrun is that one read, and nothing is sent. A job with no process and idle is `Ok`, and so is one
    /// whose pid the OS reports gone: a run that ended inside the budget is not a timeout because the pass that
    /// noticed came after the deadline. A process, or a start launchd has accepted, is the timeout, worded by
    /// what the stop did to the run that is up ([`Course::escalation`]). Any other read is the contract's error: no process and a state that is
    /// not idle is [`Error::StopUnconfirmed`] ([`quiet`]), and a read that failed returns its error.
    fn done_or(&self, id: &Id, course: &Course, reservation: &mut Reservation) -> Result<()> {
        let issuing = Budget::Unbounded.start();
        let print = self
            .launchctl
            .read_task_print(id.as_str(), issuing, Role::Query, reservation)?;
        let said = match print.pid {
            Some(pid) => match ProcessId::of(pid) {
                Resolved::Found(run) => course.escalation(run),
                // Launchd shows a pid it has not yet cleared: the run it was has ended.
                Resolved::Gone => return Ok(()),
                Resolved::Unknown => None,
            },
            None => match quiet(id, &print)? {
                Quiet::Idle => return Ok(()),
                Quiet::Queued => Some(Escalation::Unsent),
            },
        };
        Err(match said {
            Some(sent) => timed_out_stopping(id.as_str(), course.counted, sent),
            None => budget::timed_out(id.as_str(), "stopped", course.counted),
        })
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
    fn stop_pass(&self, id: &Id, course: &mut Course, reservation: &mut Reservation) -> Result<Pass> {
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
        let mut target = JobKill::new(&self.launchctl, name, reservation)?;
        let held = match hold_pid(&mut target, pid) {
            Ok(Hold::Held { process, .. }) => process,
            Ok(Hold::Ended { .. }) => return Ok(Pass::Behind),
            Err(error) => return Err(stop_failed(name, "holding the process", error.into())),
        };
        let run = held.id();
        if let Some(signal) = course.due(run)
            && !send(&held, course, name, signal)?
        {
            return Ok(Pass::Again);
        }
        if course.counted == Budget::Immediate {
            return Ok(Pass::Done);
        }
        if registered.wait(Some(course.wait_until(run)))?.is_some() {
            return Ok(Pass::Again);
        }
        // The wait ran out. At the deadline, the next pass decides. At the end of the grace, the run is still
        // there: launchd is asked again whether the process held is the job's, which it is not if the run has
        // gone and the job has another, and then it is killed through the hold, never by its pid or by the
        // label.
        if course.due(run) != Some(Signal::Kill) {
            return Ok(Pass::Again);
        }
        if !target
            .confirm(pid, &held)
            .map_err(|e| stop_failed(name, "confirming the process", e))?
        {
            return Ok(Pass::Behind);
        }
        if send(&held, course, name, Signal::Kill)? {
            // The wait for the exit is on the hold too: the watch is spent by the wait that ran out. Whether it
            // confirmed the exit or expired at the deadline, the next pass decides.
            let _waited = target
                .await_exit(&held, course.deadline())
                .map_err(|e| stop_failed(name, "waiting for the process to exit", e))?;
        }
        Ok(Pass::Again)
    }
}

/// Sends `held` `which` through the hold, and records it. The deadline is decided right before the send,
/// after the first: a spent one sends nothing, and the answer is `false`.
fn send(held: &Process, course: &mut Course, id: &str, which: Signal) -> Result<bool> {
    if course.spent() {
        return Ok(false);
    }
    send_signal(held, which).map_err(|e| stop_failed(id, &format!("sending {}", which.name()), e))?;
    course.sent(held.id(), which);
    Ok(true)
}

/// `e`, a failure of `doing` during a stop of `id`: the stop's own, in its words.
fn stop_failed(id: &str, doing: &str, e: io::Error) -> Error {
    failed_as(format!("stop `{id}`: {doing}"), e)
}

#[cfg(test)]
#[path = "verbs_tests.rs"]
mod verbs_tests;
