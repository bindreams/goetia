//! A task's `stop`: the loop that ends the run in flight, and what it carries from one pass to the next.

use std::io;
use std::time::{Duration, Instant};

use cosca::Process;
use cosca::identity::ProcessId;

use super::kill::JobKill;
use super::verbs::{Quiet, quiet};
use super::watch::{Added, Registered, Watch};
use super::{Launchd, TASK_STOP_CALLS, TASK_STOP_REQUESTS, UNLOAD_POLL, own_reservation};
use crate::backend::bounded::{Reservation, Role};
use crate::backend::kill::process::{Signal, Signalling, send_signal};
use crate::backend::kill::{Hold, KillTarget as _, hold_pid};
use crate::error::{Error, Result, target_failed};
use crate::manager::Budget;
use crate::manager::budget::{self, Deadline};
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
    /// Launchd shows the pid `.0`, which is no run of the job's that the stop can reach: the run ended and
    /// launchd has not cleared it, or launchd has moved on to another run. Under a budget that waits: read
    /// again, paced by [`UNLOAD_POLL`], as for a wait on `print`: it spaces the reads and decides nothing, and
    /// never runs past the deadline. Under `Immediate`, which waits for nothing: read again at once, and the
    /// same pid on that reading ends the stop ([`unreached`]).
    Behind(u32),
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
    /// What a task's stop sends and how long a run has to answer it. A job whose metadata does not decode has
    /// none, and is sent SIGTERM and waited for as long as the caller's budget allows, as before
    /// `stop-action`.
    action: Option<StopAction>,
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
    /// Under `Immediate`: the pid the last reading showed that the stop could not reach. A reading that shows
    /// it again ends the stop, and is not attempted.
    unreached: Option<u32>,
}

impl Course {
    fn new(action: Option<StopAction>, budget: Budget, watch: Watch) -> Course {
        Course {
            action,
            watch,
            started: Instant::now(),
            counted: budget,
            termed: None,
            first_over: false,
            unreached: None,
        }
    }

    fn deadline(&self) -> Deadline {
        self.counted.start_at(self.started)
    }

    fn extend(&mut self, by: Duration) {
        self.counted = self.counted.extended(by);
    }

    /// Whether the caller's deadline is spent: the stop's one rule for it. The first pass always runs, whatever
    /// the budget, as every backend's stop is issued: until it is over, or something has been sent, the
    /// deadline is not read. After that every pass reads it first, whether anything was sent or not, and a spent
    /// `Bounded` one is the caller's timeout: nothing more is sent, and the pass makes one reading, which ends the
    /// stop ([`Launchd::done_or`]). So a stop that keeps finding a pid launchd has not cleared ends at its
    /// deadline. `Immediate` waits for no exit, so it has no deadline to spend.
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

    /// `send-signal: kill`: SIGKILL at once.
    fn sends_kill(&self) -> bool {
        self.action.is_some_and(|action| action.send_signal == StopSignal::Kill)
    }

    /// The signal due `run`, which a pass has held: its first, or SIGKILL once its grace is over. None for
    /// a run that has been sent what it is due and whose grace is not over.
    fn due(&self, run: ProcessId) -> Option<Signal> {
        match self.termed(run) {
            None if self.sends_kill() => Some(Signal::Kill),
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
                grace_end: self
                    .action
                    .and_then(|action| action.timeout)
                    .map(|grace| Budget::Bounded(grace).start()),
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
            }) if end.ends_before(deadline) => end,
            _ => deadline,
        }
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
    /// Nothing, because there was nothing to signal: launchd showed a pid that is no run of the job's the stop
    /// can reach (the run ended and was not cleared, or launchd had moved on to another run) when the wait ran out.
    Uncleared,
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
                Escalation::Uncleared => format!(
                    "{recovery} Launchd still showed a process ID for the job when the deadline passed, but that \
                     process had already ended or belonged to a run that replaced it; run `goetia daemon stop {id}` \
                     again."
                ),
            },
            id,
            awaited,
            waited,
        },
        other => other,
    }
}

/// What an `Immediate` stop ends with when launchd showed the same pid `pid` on two readings in a row and the
/// stop could not reach it: it sent nothing, and cannot tell that no run is left. Indeterminate, like any
/// [`Error::StopUnconfirmed`].
fn unreached(id: &Id, pid: u32) -> Error {
    Error::StopUnconfirmed {
        id: id.to_string(),
        shown: format!(
            "launchd showed process {pid} for the job on two readings in a row, and this stop could not reach it \
             (it has ended, or it is not the job's), and nothing was sent; run `goetia daemon stop {id}` again"
        ),
    }
}

/// What `print` and the process it shows say is up for a stop: [`Launchd::read_up`].
enum Up<'a> {
    /// No process, and launchd holds no start: [`Quiet::Idle`].
    Idle,
    /// A start launchd has accepted and not yet forked: [`Quiet::Queued`].
    Queued,
    /// Launchd shows the pid `.0`, and it is no run of the job's that the stop can reach: the process is gone,
    /// or is held and launchd does not name it (the run ended and is not cleared, or launchd has moved on to
    /// another run, which this reading did not hold), or it is the pid the stop's last reading could not
    /// reach, which is not attempted again. Nothing is signalled.
    Behind(u32),
    /// A process held and confirmed by launchd as the job's, with its exit registered.
    Run {
        pid: u32,
        registered: Registered,
        target: Signalling<JobKill<'a>>,
        held: Process,
    },
}

impl Launchd {
    /// Ends the run in flight, and returns once launchd shows no process. The stop is always issued, as every
    /// backend's is, whatever the budget: the first pass always runs, and the wait for a queued start and the
    /// signal to the run found after it are part of it. A spent deadline ends it ([`Course::spent`]).
    /// `Immediate` waits for no exit, and not for launchd's view either: it signals the run it finds and returns
    /// `Ok`. A pid launchd shows that it cannot reach is followed at once by another reading, with no pause. The
    /// same pid again is [`Error::StopUnconfirmed`] ([`unreached`]): launchd consistently gave the pid the stop
    /// could not signal, and that second reading only asks launchd. Another pid is attempted like the first,
    /// and no process on an idle job is `Ok`. There is no cap: the loop ends on a repeated pid, an idle job, or
    /// a run it signals. Otherwise the pass reads `print`:
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
    /// more, is the next pass, paced by [`UNLOAD_POLL`] while launchd's view catches up, or by what is left of
    /// the deadline when that is less (`Immediate` does not pause: the reading is its only wait). The first pass
    /// draws on `reservation`, and each later one on one of its own: an uncapped loop would draw what was never
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
            let reading = self.stop_pass(id, &mut course, pass)?;
            // Only a reading right after the one that failed can repeat its pid.
            let failed = std::mem::take(&mut course.unreached);
            match reading {
                Pass::Done => return Ok(()),
                Pass::Again => course.first_pass_over(),
                Pass::Queued(waited) => course.extend(waited),
                Pass::Behind(pid) if course.counted == Budget::Immediate => {
                    if failed == Some(pid) {
                        return Err(unreached(id, pid));
                    }
                    // Nothing is waited for, so there is no pause: the next reading is the wait.
                    course.unreached = Some(pid);
                }
                Pass::Behind(_) => {
                    course.first_pass_over();
                    // The pause never crosses a bounded deadline.
                    let pause = match (course.counted, course.deadline().remaining()) {
                        (Budget::Bounded(_), Some(left)) => left.min(UNLOAD_POLL),
                        _ => UNLOAD_POLL,
                    };
                    std::thread::sleep(pause);
                }
            }
            own = Some(own_reservation(id, TASK_STOP_REQUESTS, TASK_STOP_CALLS)?);
        }
    }

    /// What a stop whose deadline is spent ends with, after one reading ([`Self::read_up`]) on `reservation`
    /// and no loop: the overrun is that one reading, and nothing is sent. A job with no process and idle is
    /// `Ok`: a run that ended inside the budget is not a timeout because the pass that noticed came after the
    /// deadline. A run held and confirmed as the job's, and a start launchd has accepted, are the timeout,
    /// worded by what the stop did to the run that is up ([`Course::escalation`]). So is launchd showing a pid
    /// that is no run of the job's the stop can reach (gone, or one it does not name: the run ended and is not
    /// cleared, or launchd has moved on to another run), with its own wording: not `Ok`, since a `start` made
    /// now starts nothing while launchd shows a pid. Any other reading is the
    /// contract's error: no process and a state that is not idle is [`Error::StopUnconfirmed`] ([`quiet`]),
    /// and a read that failed returns its error.
    fn done_or(&self, id: &Id, course: &Course, reservation: &mut Reservation) -> Result<()> {
        let said = match self.read_up(id, &course.watch, None, reservation)? {
            Up::Idle => return Ok(()),
            Up::Queued => Some(Escalation::Unsent),
            Up::Behind(_) => Some(Escalation::Uncleared),
            Up::Run { held, .. } => course.escalation(held.id()),
        };
        Err(match said {
            Some(sent) => timed_out_stopping(id.as_str(), course.counted, sent),
            None => budget::timed_out(id.as_str(), "stopped", course.counted),
        })
    }

    /// What is up for the job, read the one way for the pass that acts on it ([`Self::stop_pass`]) and the one
    /// that only reports it ([`Self::done_or`]): `print`; for a pid, its exit registered with `watch`, the
    /// process held, and launchd asked whether it is the job's ([`hold_pid`], as `kill` does). The
    /// registration is dropped on any way out of the caller, which deletes it and drains the queue: a pid that
    /// is reused later cannot be handed this process's exit. Launchd's record of a run that has gone is not
    /// waited for here: [`Up::Behind`], and the caller reads again (paced, or at once under `Immediate`), or ends.
    /// A pid that is `unreached`, the one a reading just failed to reach, is not attempted again: it is
    /// [`Up::Behind`] on `print` alone.
    fn read_up<'a>(
        &'a self,
        id: &'a Id,
        watch: &Watch,
        unreached: Option<u32>,
        reservation: &mut Reservation,
    ) -> Result<Up<'a>> {
        let name = id.as_str();
        // Unbounded: `print` answers at once, and its body is whole.
        let issuing = Budget::Unbounded.start();
        let print = self
            .launchctl
            .read_task_print(name, issuing, Role::Query, reservation)?;
        let Some(pid) = print.pid else {
            return Ok(match quiet(id, &print)? {
                Quiet::Queued => Up::Queued,
                Quiet::Idle => Up::Idle,
            });
        };
        if unreached == Some(pid) {
            return Ok(Up::Behind(pid));
        }
        let Added::Watching(registered) = watch.add(pid)? else {
            return Ok(Up::Behind(pid));
        };
        // The process is held, and only then does launchd confirm that it is the job's: a pid that was
        // read and then reused is a process launchd does not name, and is left alone.
        let mut target = JobKill::new(&self.launchctl, name, reservation)?;
        let held = match hold_pid(&mut target, pid) {
            Ok(Hold::Held { process, .. }) => process,
            Ok(Hold::Gone { .. } | Hold::Disowned { .. }) => return Ok(Up::Behind(pid)),
            Err(error) => return Err(stop_failed(name, "holding the process", error.into())),
        };
        Ok(Up::Run {
            pid,
            registered,
            target,
            held,
        })
    }

    /// One pass of [`Self::stop_task`] over what [`Self::read_up`] finds. `course` is what the stop is held to
    /// and reports having waited, and what it has sent: the waits for queued starts are in its budget.
    fn stop_pass(&self, id: &Id, course: &mut Course, reservation: &mut Reservation) -> Result<Pass> {
        let name = id.as_str();
        let (pid, registered, mut target, held) =
            match self.read_up(id, &course.watch, course.unreached, reservation)? {
                Up::Idle => return Ok(Pass::Done),
                Up::Queued => {
                    let began = Instant::now();
                    self.await_queued_start(name, reservation)?;
                    return Ok(Pass::Queued(began.elapsed()));
                }
                Up::Behind(pid) => return Ok(Pass::Behind(pid)),
                Up::Run {
                    pid,
                    registered,
                    target,
                    held,
                } => (pid, registered, target, held),
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
            return Ok(Pass::Behind(pid));
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
    target_failed(format!("stop `{id}`: {doing}"), e)
}

#[cfg(test)]
#[path = "stop_tests.rs"]
mod stop_tests;
