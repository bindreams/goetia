//! A task's `start`, `stop` and `kill`, and the stop an `uninstall` makes.
//!
//! A run is a process, and goetia acts on it as `kill.rs` acts on a service's: the process is opened
//! by the pid the listing gives and held, the listing is read again, and only a pid it still names is
//! the run's, since Task Scheduler holds its own handle to the process for as long as it lists the pid,
//! so no stranger can have the number (L1, M5). [`RunTarget`] is that for one run, a
//! [`KillTarget`]: `kill` is [`kill_service`] over it, and `stop` holds through the same
//! [`hold_once`].
//!
//! Three things differ from a service.
//!
//! - A task can have several runs alive at once (L2: a password-logon user's start can be accepted
//!   just before the first process exits, and the user can start one by hand), so each verb acts on
//!   every run [`live_runs`] returns, never on one.
//! - A run is one process and never a later one, so one look is the whole hold: a process that is gone
//!   when it is opened is no process, and there is no read of the listing to wait for it to change.
//! - No verb waits for the listing to drop: starts are accepted while an ended run is still listed
//!   (L2), so [`KillTarget::await_recorded`] waits on nothing.

use std::io;

use super::api::{Instance, is_not_running};
use super::live::{Liveness, probe};
use super::read::live_runs;
use super::{TaskApi, TaskPath};
use crate::backend::kill::{
    Hold, HoldError, KillTarget, Waited, hold_once, kill_failed, kill_service, wait_saying, waiting_notice,
};
use crate::backend::scm::folder::program_data;
use crate::backend::scm::instance::InstanceId;
use crate::backend::scm::kill::system::Held;
use crate::backend::scm::manager::{Scm, open_refused};
use crate::backend::scm::runs::{RunOutcome, RunRecord, Runs};
use crate::error::{Error, Result, target_failed, within};
use crate::manager::Budget;
use crate::manager::budget::{self, Deadline};
use crate::spec::Id;

// The machine's processes ---------------------------------------------------------------------------------------------

/// The processes of the runs a task lists, as a verb asks about them: whether one is alive, and the held
/// process of [`KillTarget`] (open, terminate, wait for its exit). [`System`] is the machine's; a test
/// states them.
pub(crate) trait Processes {
    /// An open process, which keeps its pid from being given to another.
    type Held;

    /// Whether the process behind `pid` runs. A process that cannot be judged is the error, never alive.
    fn alive(&mut self, pid: u32) -> io::Result<Liveness>;

    /// The process behind `pid`, held, or `None` if there is no such process.
    fn open(&mut self, pid: u32) -> io::Result<Option<Self::Held>>;

    /// Ends `held`, through the hold and never by pid, and does not wait for it.
    fn terminate(&mut self, held: &Self::Held) -> io::Result<()>;

    /// Blocks until `held` has exited, or until `deadline`.
    fn await_exit(&mut self, held: &Self::Held, deadline: Deadline) -> io::Result<Waited>;
}

/// The machine's own processes.
pub(crate) struct System;

impl Processes for System {
    type Held = Held;

    fn alive(&mut self, pid: u32) -> io::Result<Liveness> {
        probe(pid)
    }

    fn open(&mut self, pid: u32) -> io::Result<Option<Held>> {
        Held::open(pid)
    }

    fn terminate(&mut self, held: &Held) -> io::Result<()> {
        held.terminate()
    }

    fn await_exit(&mut self, held: &Held, deadline: Deadline) -> io::Result<Waited> {
        held.wait_for_exit(deadline, || {})
    }
}

/// One run of a task, as the state machine in `backend::kill` sees a service: the process the listing
/// names for `instance`.
struct RunTarget<'a, P: Processes> {
    tasks: &'a dyn TaskApi,
    path: &'a TaskPath,
    instance: InstanceId,
    /// The pid the listing gave the run when the verb began, which only names the run in a failed read.
    pid: u32,
    /// Whether the listing has been read for this run: the first read alone is made before it names a
    /// process, so it alone says which run it was for.
    listed: bool,
    processes: &'a mut P,
    /// The run's process, when a caller opened it beforehand ([`hold_all`]): the machine takes it in
    /// place of an open, and confirms it as it confirms any.
    held: Option<P::Held>,
}

impl<'a, P: Processes> RunTarget<'a, P> {
    fn new(tasks: &'a dyn TaskApi, path: &'a TaskPath, instance: &Instance, processes: &'a mut P) -> Self {
        Self {
            tasks,
            path,
            instance: instance.id,
            pid: instance.pid,
            listed: false,
            processes,
            held: None,
        }
    }
}

impl<P: Processes> KillTarget for RunTarget<'_, P> {
    type Process = P::Held;

    /// The pid the listing gives the run, read afresh: `None` once the entry is gone, or the task is. The
    /// failure of the first read says which run it was for; later reads follow a pid the hold's own words
    /// name.
    fn service_pid(&mut self) -> io::Result<Option<u32>> {
        let first = !std::mem::replace(&mut self.listed, true);
        match self.tasks.instances(self.path) {
            Ok(listed) => Ok(listed
                .iter()
                .find(|listed| listed.id == self.instance)
                .map(|listed| listed.pid)),
            Err(e) if gone(&e) => Ok(None),
            Err(e) if first => Err(within(
                format!(
                    "failed to list the instances of `{}` for its run (pid {})",
                    self.path.name(),
                    self.pid
                ),
                io::Error::other(e),
            )),
            Err(e) => Err(io::Error::other(e)),
        }
    }

    fn open(&mut self, pid: u32) -> io::Result<Option<P::Held>> {
        match self.held.take() {
            Some(held) => Ok(Some(held)),
            None => self.processes.open(pid),
        }
    }

    fn terminate(&mut self, process: &P::Held) -> io::Result<()> {
        self.processes.terminate(process)
    }

    fn await_exit(&mut self, process: &P::Held, deadline: Deadline) -> io::Result<Waited> {
        self.processes.await_exit(process, deadline)
    }

    /// Nothing: an ended run stays listed for a few milliseconds and a start is accepted meanwhile (L2),
    /// so no verb waits for the listing to drop.
    fn await_recorded(&mut self, _pid: u32, _held: Option<&P::Held>, _deadline: Deadline) -> io::Result<Waited> {
        Ok(Waited::Confirmed)
    }
}

/// Whether `error` says the task is not there.
pub(super) fn gone(error: &Error) -> bool {
    matches!(error.cause(), Error::NotInstalled { .. })
}

/// The process of the run `instance`, held and confirmed by a second read of the listing, or `None`
/// when the run has no process: it is gone when opened, or the listing no longer names it. `operation`
/// is the verb the hold is for, which a denial names.
pub(super) fn hold_run<P: Processes>(
    tasks: &dyn TaskApi,
    operation: &str,
    id: &Id,
    path: &TaskPath,
    instance: &Instance,
    processes: &mut P,
) -> Result<Option<P::Held>> {
    let mut target = RunTarget::new(tasks, path, instance, processes);
    match hold_once(&mut target, Budget::Unbounded.start()) {
        Ok(Hold::Held { process, .. }) => Ok(Some(process)),
        Ok(Hold::NoProcess | Hold::Ended { .. }) => Ok(None),
        Err(HoldError::Refused(e)) => Err(open_refused(
            operation,
            &format!("the process of `{id}`'s run"),
            &format!("hold the process of `{id}`'s run to wait for its exit"),
            e,
        )),
        // The first read of the run says which run it was for, and the other errors of a hold name the pid.
        Err(e) => Err(target_failed(
            format!("hold the process of `{id}`'s run"),
            io::Error::from(e),
        )),
    }
}

// Stop ================================================================================================================

/// What a stop whose shim recorded that it could not end the program's process tree is: the program may
/// still be running. `verb` is the verb that saw the record.
pub(super) fn incomplete_stop(id: &Id, verb: &str) -> Error {
    Error::os(
        format!("{verb} `{id}`"),
        io::Error::other(
            "run stopped: goetia-shim recorded that it could not end the program's process tree, so the program may \
             still be running",
        ),
    )
}

/// Sends the `Stop` for `instance`. A run that has ended is stopped (L3: `SCHED_E_TASK_NOT_RUNNING`), and
/// so is one whose task is gone.
fn send_stop(tasks: &dyn TaskApi, path: &TaskPath, instance: &Instance) -> Result<()> {
    match tasks.stop(path, &instance.id) {
        Ok(()) => Ok(()),
        Err(e) if is_not_running(&e) || gone(&e) => Ok(()),
        Err(e) => Err(e),
    }
}

/// Sends the `Stop` of every run in `instances`, whatever one of them came to: the failures are
/// reported together, after every run has been asked.
fn send_stops<'a>(
    tasks: &dyn TaskApi,
    path: &TaskPath,
    instances: impl IntoIterator<Item = &'a Instance>,
) -> Result<()> {
    let failures: Vec<Error> = instances
        .into_iter()
        .filter_map(|instance| send_stop(tasks, path, instance).err())
        .collect();
    Error::both_all(failures).map_or(Ok(()), Err)
}

/// The process of every run in `live`, held, each confirmed by a second read of the listing. A run
/// with no process any more (see [`hold_run`]) is left out. Held before anything is done to any of them,
/// so a run whose process cannot be held fails the call with nothing sent.
fn hold_all<P: Processes>(
    tasks: &dyn TaskApi,
    operation: &str,
    id: &Id,
    path: &TaskPath,
    live: Vec<Instance>,
    processes: &mut P,
) -> Result<Vec<(Instance, P::Held)>> {
    let mut held = Vec::new();
    for instance in live {
        if let Some(process) = hold_run(tasks, operation, id, path, &instance, processes)? {
            held.push((instance, process));
        }
    }
    Ok(held)
}

/// Waits for `process`, the held process of `instance`, to exit. Under no bound it says why it blocks
/// once it has lasted a second; under `budget`, an expiry is the call's [`Error::WaitTimeout`], with the
/// stop already sent.
fn await_run<P: Processes>(
    target: &mut RunTarget<'_, P>,
    id: &Id,
    pid: u32,
    process: &P::Held,
    budget: Budget,
    deadline: Deadline,
) -> Result<()> {
    let waited = if deadline.at().is_none() {
        wait_saying(target, &waiting_notice(id.as_str(), pid, None), |target, deadline| {
            target.await_exit(process, deadline)
        })
        .map(|()| Waited::Confirmed)
    } else {
        target.await_exit(process, deadline)
    }
    .map_err(|e| Error::os(format!("wait for `{id}`'s run (pid {pid}) to exit"), e))?;
    match waited {
        Waited::Confirmed => Ok(()),
        Waited::Expired => Err(budget::timed_out(id.as_str(), "stopped", budget)),
    }
}

/// Ends every run of the task `id` that is alive: its `Stop` is sent, and under a budget that waits its
/// process is waited for until `budget` runs out. Returns the runs whose process was waited for, which
/// is none under a budget that does not wait.
///
/// Every process is held before the first `Stop` is sent, so a run whose process cannot be held
/// fails the call with nothing sent. A run that ended on its own, or that no longer has a process, is
/// not touched: the listing is no proof of a run, and a pid it no longer names may be a stranger's. A
/// task that is gone has no runs.
///
/// Under a budget that waits, `before_stops` is called once, when there is a run to stop and before the
/// first `Stop` is sent: a failure is the call's, with nothing sent. It is for what must be in place
/// before a run ends.
pub(super) fn stop_runs<P: Processes>(
    tasks: &dyn TaskApi,
    operation: &'static str,
    id: &Id,
    budget: Budget,
    processes: &mut P,
    before_stops: impl FnOnce() -> Result<()>,
) -> Result<Vec<InstanceId>> {
    // First, so the wait is counted from the verb's entry.
    let deadline = budget.start();
    let path = TaskPath::of(id);
    let live = match live_runs(tasks, operation, id, &path, |pid| processes.alive(pid)) {
        Ok(live) => live,
        Err(e) if gone(&e) => return Ok(Vec::new()),
        Err(e) => return Err(e),
    };
    if !budget.waits() {
        send_stops(tasks, &path, &live)?;
        return Ok(Vec::new());
    }

    let held = hold_all(tasks, operation, id, &path, live, processes)?;
    if !held.is_empty() {
        before_stops()?;
    }
    send_stops(tasks, &path, held.iter().map(|(instance, _)| instance))?;
    for (instance, process) in &held {
        let mut target = RunTarget::new(tasks, &path, instance, &mut *processes);
        await_run(&mut target, id, instance.pid, process, budget, deadline)?;
    }
    Ok(held.into_iter().map(|(instance, _)| instance.id).collect())
}

/// Whether `error` from [`Runs::hold`] says the records' directory holds no records: it is not there, or it is
/// a link, which the task's account can make of it and which goetia reads nothing through.
fn no_records(error: &io::Error) -> bool {
    matches!(error.kind(), io::ErrorKind::NotFound | io::ErrorKind::NotADirectory)
}

/// The runs `stopped` ended, as their shims recorded them: one that could not end the program's process
/// tree is a failed stop, and a record that cannot be read does not say the stop was complete. A task
/// with no records, or whose directory is a link, has no stop to read ([`Runs::read`]).
fn check_stopped(id: &Id, runs: &Runs, stopped: &[InstanceId]) -> Result<()> {
    for instance in stopped {
        match runs.read(instance) {
            Ok(Some(RunRecord {
                outcome: RunOutcome::IncompleteStop,
                ..
            })) => return Err(incomplete_stop(id, "stop")),
            Ok(_) => {}
            Err(e) => return Err(Error::os(format!("read the record of `{id}`'s stopped run"), e)),
        }
    }
    Ok(())
}

impl Scm {
    /// `start` of a task: Task Scheduler is asked to run it, and the call returns once it has accepted.
    /// A run that is alive makes it start nothing (`IgnoreNew`), which is no failure. A restart's start
    /// is [`Self::task_request_start_after_stop`], which refuses over a run that is alive.
    pub(in crate::backend::scm) fn task_start(&self, id: &Id) -> Result<()> {
        self.tasks.run(&TaskPath::of(id)).map(|_| ())
    }

    /// The start of a restart: [`Self::task_request_start_after_stop_with`] over the machine's processes.
    pub(in crate::backend::scm) fn task_request_start_after_stop(&self, id: &Id) -> Result<()> {
        self.task_request_start_after_stop_with(id, &mut System)
    }

    /// The start a restart makes once its stop has been sent, which `restart --timeout 0` did not wait
    /// for. A run still alive makes Task Scheduler accept the `Run` and start nothing (`IgnoreNew`, M7),
    /// so it is refused with [`Error::StillRunning`] naming that run, as launchd's task path is, and
    /// nothing is sent. An ended run that is still listed is no run (L2): the start is made.
    pub(super) fn task_request_start_after_stop_with<P: Processes>(&self, id: &Id, processes: &mut P) -> Result<()> {
        let path = TaskPath::of(id);
        let live = match live_runs(&*self.tasks, "start", id, &path, |pid| processes.alive(pid)) {
            Ok(live) => live,
            // The task is gone: the start says so.
            Err(e) if gone(&e) => Vec::new(),
            Err(e) => return Err(e),
        };
        if let Some(run) = live.first() {
            return Err(Error::StillRunning {
                id: id.to_string(),
                pid: run.pid,
            });
        }
        self.task_start(id)
    }

    /// `stop` of a task: [`Self::task_stop_with`] over the machine's processes and the task's records.
    pub(in crate::backend::scm) fn task_stop(&self, id: &Id, budget: Budget) -> Result<()> {
        let runs = Runs::of(&program_data()?, id);
        self.task_stop_with("stop", id, budget, &runs, &mut System)
    }

    /// Every run of the task ended ([`stop_runs`]), and then read in its record: a stop whose shim could
    /// not end the program's tree is a failed stop. `operation` is the verb, which a denial names.
    ///
    /// When there is a run to stop and the budget waits, a [`Pin`](crate::backend::scm::runs::Pin) is
    /// held from before the first `Stop` is sent until every record is read: the shims write as their
    /// runs end, and without it the write of a later run prunes the record of an earlier one before it
    /// is read. A task with no run to stop has no records to keep, so it takes none (a caller who is not
    /// an Administrator cannot); one whose record directory is gone has none either (`NotFound`), nor has
    /// one where the directory is a link ([`no_records`]), and the stop goes on. Any
    /// other refusal is an error, worded as any verb's refusal is (a denial for access denied): that
    /// includes the pin refused because the records are being removed, which is not "no records".
    pub(super) fn task_stop_with<P: Processes>(
        &self,
        operation: &'static str,
        id: &Id,
        budget: Budget,
        runs: &Runs,
        processes: &mut P,
    ) -> Result<()> {
        let mut pin = None;
        let stopped = stop_runs(&*self.tasks, operation, id, budget, processes, || {
            match runs.hold() {
                Ok(held) => pin = Some(held),
                Err(e) if no_records(&e) => {}
                Err(e) => {
                    return Err(open_refused(
                        operation,
                        &format!("the run records of `{id}`"),
                        &format!("pin the run records of `{id}`"),
                        e,
                    ));
                }
            }
            Ok(())
        })?;
        let checked = check_stopped(id, runs, &stopped);
        drop(pin);
        checked
    }

    /// `kill` of a task: [`Self::task_kill_with`] over the machine's processes.
    pub(in crate::backend::scm) fn task_kill(&self, id: &Id) -> Result<()> {
        self.task_kill_with(id, &mut System)
    }

    /// Ends every run of the task that is alive through its held process ([`kill_service`] over each).
    /// Every process is held before the first is terminated, so a run whose process cannot be held
    /// fails the call with nothing killed; once they are, a run that cannot be ended is an error beside
    /// the ones that were. None alive is [`Error::NotRunning`]; a run with no process any more has
    /// ended.
    pub(super) fn task_kill_with<P: Processes>(&self, id: &Id, processes: &mut P) -> Result<()> {
        let path = TaskPath::of(id);
        let live = match live_runs(&*self.tasks, "kill", id, &path, |pid| processes.alive(pid)) {
            Ok(live) => live,
            Err(e) if gone(&e) => Vec::new(),
            Err(e) => return Err(e),
        };
        if live.is_empty() {
            return Err(Error::NotRunning { id: id.to_string() });
        }
        let held = hold_all(&*self.tasks, "kill", id, &path, live, processes)?;
        let mut failures = Vec::new();
        for (instance, process) in held {
            let mut target = RunTarget::new(&*self.tasks, &path, &instance, &mut *processes);
            target.held = Some(process);
            // `NotRunning` is a run the listing dropped since: it is over.
            if let Err(e) = kill_service(&mut target, id.as_str()).map_err(|e| kill_failed(id.as_str(), e)) {
                failures.push(e);
            }
        }
        Error::both_all(failures).map_or(Ok(()), Err)
    }
}

#[cfg(test)]
#[path = "verbs_tests.rs"]
mod verbs_tests;
