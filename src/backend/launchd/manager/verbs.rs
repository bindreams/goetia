//! A task's `start` and `stop`. Its job is a trigger-less launchd job that a timer, or a `start`,
//! kicks; neither verb ever unloads it.
//!
//! `start` loads the job from wherever its plist is and kicks it: it requests a run and returns, and a
//! run in flight is left alone (measured: `kickstart` of a running job does nothing). `stop` ends the
//! run in flight, and any run that begins while it does, as the task's `stop-action` says (SIGTERM by
//! label, then SIGKILL once its `timeout` is over; SIGKILL at once for `kill`), and returns when launchd
//! shows no process.

use std::time::Duration;

use super::kill::JobKill;
use super::task::TaskPrint;
use super::watch::{Added, Watch};
use super::{Launchd, Plist, SignalOutcome, UNLOAD_POLL, Until, never_expires};
use crate::backend::bounded::{Reservation, Role};
use crate::backend::kill::{KillOutcome, kill_failed, kill_service, process};
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
    /// Read again: a start launchd had accepted has forked, after a wait under no budget. That wait is
    /// not the caller's to spend, so the caller's budget starts over.
    Queued,
    /// Read again, paced: launchd still shows what has ended (a pid that exited, a process a signal
    /// found none for) or a job other than the one watched, and its view has to catch up. The pause is
    /// [`UNLOAD_POLL`], as for a wait on `print`: it spaces the reads and decides nothing.
    Behind,
    /// Read again: the run with this pid ignored SIGTERM for its `timeout`, so the next pass that finds
    /// it sends SIGKILL.
    Escalate(u32),
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
    /// The run whose grace was last started, and when it ends: from the run's first SIGTERM, which a
    /// later pass that finds the same run does not restart.
    termed: Option<(u32, Deadline)>,
    /// The run whose grace ran out: SIGKILL is for it, and a run that began since is a new one.
    escalated: Option<u32>,
}

impl Ending {
    fn of(action: Option<StopAction>) -> Ending {
        Ending {
            kill: action.is_some_and(|action| action.send_signal == StopSignal::Kill),
            grace: action.and_then(|action| action.timeout),
            termed: None,
            escalated: None,
        }
    }

    /// When the grace of the run `pid` ends: `grace` from its first SIGTERM, which is now if none has
    /// been sent to it.
    fn grace_end(&mut self, pid: u32, grace: Duration) -> Deadline {
        match self.termed {
            Some((termed, end)) if termed == pid => end,
            _ => {
                let end = Budget::Bounded(grace).start();
                self.termed = Some((pid, end));
                end
            }
        }
    }

    /// Whether the run `pid` is sent SIGKILL, rather than SIGTERM.
    fn kills(&self, pid: u32) -> bool {
        self.kill || self.escalated == Some(pid)
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
    ///   it lasts as long as launchd's throttle, and `budget` starts over once it is done.
    /// - A pid: watches it, confirms that `print` still shows it, signals it by label, and waits for the
    ///   exit under `budget` (`Immediate` does not wait). The signal is the `action`'s: SIGKILL for
    ///   `send-signal: kill`, else SIGTERM. A run that ignores SIGTERM for the action's `timeout`, which
    ///   starts at its SIGTERM (if the budget lasts that long), is sent SIGKILL by the next pass, which
    ///   finds it by its pid and kills the process it opens by that pid, not whatever the label names by
    ///   then. It is waited for under `budget` too: the `timeout` is the grace the user configured, as systemd's
    ///   `TimeoutStopSec=` is. No `action`, or a `timeout` of `null`, never escalates. A run that began
    ///   meanwhile is a real run and is ended too, with a grace of its own, so the loop ends only when no
    ///   process is left.
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
        let watch = Watch::new()?;
        let mut ending = Ending::of(action);
        // From the verb's entry, and again after a wait for a queued start's fork, which is under no
        // budget. Unused under `Immediate`, which waits for no exit.
        let mut deadline = budget.start();
        let mut own: Option<Reservation> = None;
        loop {
            let pass = own.as_mut().unwrap_or(&mut *reservation);
            match self.stop_pass(id, &mut ending, budget, deadline, &watch, pass)? {
                Pass::Done => return Ok(()),
                Pass::Again => {}
                Pass::Queued => deadline = budget.start(),
                Pass::Escalate(pid) => ending.escalated = Some(pid),
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
        ending: &mut Ending,
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
                    Ok(Pass::Queued)
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
        let kills = ending.kills(pid);
        if ending.escalated == Some(pid) {
            // The run whose grace ran out, by its identity: a run that began since the pid was read is
            // not owed this SIGKILL, and a label would name it.
            match process::open(pid).map_err(|e| kill_failed(name, e))? {
                Some(held) => process::terminate(&held).map_err(|e| kill_failed(name, e))?,
                None => return Ok(Pass::Behind),
            }
        } else {
            let signal = if kills { "SIGKILL" } else { "SIGTERM" };
            if self.launchctl.signal(name, signal, reservation)? == SignalOutcome::NoProcess {
                return Ok(Pass::Behind);
            }
        }
        if budget == Budget::Immediate {
            return Ok(Pass::Done);
        }
        // A run sent SIGTERM has its `timeout` to answer, if the caller waits that long; SIGKILL has no
        // answer to wait for, and is waited for as long as the caller's budget allows.
        let grace = ending
            .grace
            .filter(|_| !kills)
            .map(|grace| ending.grace_end(pid, grace));
        let (until, escalating) = match grace {
            Some(grace) if ends_before(grace, deadline) => (grace, true),
            _ => (deadline, false),
        };
        match registered.wait(Some(until))? {
            Some(_) => Ok(Pass::Again),
            None if escalating => Ok(Pass::Escalate(pid)),
            None if grace.is_some() => Err(budget::timed_out_inside_grace(name, budget)),
            None => Err(budget::timed_out(name, "stopped", budget)),
        }
    }
}

#[cfg(test)]
#[path = "verbs_tests.rs"]
mod verbs_tests;
