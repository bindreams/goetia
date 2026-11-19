//! A task's `run`: asks Task Scheduler to start the task, finds out which run that was, and follows
//! it to its end.
//!
//! Which run it was is not always what `Run` returns: a task whose action is alive ignores the start
//! (`IgnoreNew`) and answers with a GUID that is never listed, and a scheduled run can begin in the
//! same moment. [`identify`] decides from what `Run` named, what is alive, and which records appeared
//! after the pin was taken.
//!
//! The run's own exit is its shim's record, written as the shim ends and read by the run's instance;
//! the process held is how `wait` knows the shim has ended, with no bound.

use std::io;
use std::path::PathBuf;
use std::sync::Arc;

use super::api::{Instance, Started};
use super::read::live_runs;
use super::status::CLOSED;
use super::verbs::{Processes, System, hold_run, incomplete_stop};
use super::{TaskPath, TaskSnapshot};
use crate::backend::kill::Waited;
use crate::backend::scm::folder::program_data;
use crate::backend::scm::instance::InstanceId;
use crate::backend::scm::manager::Scm;
use crate::backend::scm::runs::{Pin, RunOutcome, RunRecord, Runs};
use crate::backend::{InstalledService, Marker, OutputSource, RUN_MARKER, RunHandle};
use crate::error::{Error, Result};
use crate::manager::budget::Budget;
use crate::manager::{Exit, output};
use crate::spec::Id;

/// Why a run has no record of its end: either its program ran and the record could not be written, or its
/// shim started no program. Which it was is left to the logs: the task's last result is not read, since a
/// later run overwrites it. A failed logon starts nothing and raises nothing (M8), so the reason names it.
pub(super) const UNRECORDED: &str = "it left no record: either its program ran and its record could not be \
     written (it was stopped or killed, Windows shut down, or goetia-shim failed), or its shim started no \
     program (goetia-shim refused or failed to, or the task could not log on, which a missing logon right or a \
     changed password causes and raises nothing about); the task's log and the Event Log say which";

// Which run ===========================================================================================================

/// Which run a `run` follows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Identity {
    /// A run that is alive, with the pid it is listed with.
    Live { id: InstanceId, pid: u32 },
    /// A run that has ended and left its record.
    Done(RunRecord),
    /// No run that is known to be ours.
    Unrecorded,
}

/// The run a `run` follows, from what `started` (the `Run` call) named, the runs that are `live` and the
/// records that `appeared` after the pin was taken. A pure function of the three.
///
/// The instance `Run` returned decides first, by its GUID, whether or not `Run` returned a pid: listed
/// live it is the run followed; else a record of it is read. A pid says the instance is real, so with
/// neither it is unrecorded. `Run` named no instance of ours when it returned no pid and the GUID is
/// neither (the start was ignored under `IgnoreNew`, or refused): then one run alive is the one followed
/// (a scheduled run began after the command was asked), several alive are none known to be ours, and
/// with none alive the records decide: one is it, none is unrecorded, and several are the earliest by
/// `started` (the user's own concurrent start: a stated contract).
pub(super) fn identify(started: Option<&Started>, live: &[Instance], appeared: &[RunRecord]) -> Identity {
    if let Some(started) = started {
        // The instance `Run` named, by its GUID and in one place, whatever pid `Run` returned: a listed
        // instance is alive, so it is waited for even if its record has appeared (the shim writes it
        // before the process is gone). Not listed, a record of it is a run that began and ended before
        // it was asked (`EnginePID` reads `SCHED_E_TASK_NOT_RUNNING`), and that run is ours.
        if let Some(instance) = live.iter().find(|instance| instance.id == started.id) {
            return Identity::Live {
                id: started.id,
                pid: instance.pid,
            };
        }
        if let Some(record) = appeared.iter().find(|record| record.instance == started.id) {
            return Identity::Done(record.clone());
        }
        if started.pid.is_some() {
            return Identity::Unrecorded;
        }
    }
    match live {
        [only] => Identity::Live {
            id: only.id,
            pid: only.pid,
        },
        [] => appeared
            .iter()
            .min_by_key(|record| (record.started, record.instance))
            .map_or(Identity::Unrecorded, |record| Identity::Done(record.clone())),
        _ => Identity::Unrecorded,
    }
}

// The run =============================================================================================================

/// What a [`TaskRun`] has of the run it follows.
enum Following<H> {
    /// The run's process, held: its exit is what `wait` blocks on, and the record is read after it.
    Process { instance: InstanceId, process: H },
    /// The run's process was gone when it was opened: it has ended, and its shim wrote its record first.
    Ended(InstanceId),
    /// The run ended before it was followed, and left this record.
    Done(RunRecord),
    /// No run known to be ours.
    Unrecorded,
}

/// A run of a task that [`Scm::task_begin_run`] began: the process it is, held.
pub(crate) struct TaskRun<P: Processes = System> {
    id: Id,
    runs: Runs,
    processes: P,
    following: Following<P::Held>,
    /// Held until the run has been read: no write prunes the record it waits for, and an uninstall
    /// waits for it ([`Runs::remove_all`]) before it removes the records.
    pin: Pin,
    /// The task's `logs` file and its length before the run was requested, where it has one.
    logs: Option<(PathBuf, u64)>,
    /// The instance whose marker begins the run's own lines in the log: the run followed, or the one
    /// `Run` named when none is.
    marked: InstanceId,
}

/// The line the shim writes first for the run `instance`, by which its lines are told from those of the
/// runs that share the file. A run can have begun before the length was measured: a scheduled run that
/// `Run` joined.
fn marker_of(instance: &InstanceId) -> Marker {
    Marker {
        prefix: RUN_MARKER,
        line: instance.marker(),
        id_len: 32,
        earlier_start_possible: true,
    }
}

impl<P> RunHandle for TaskRun<P>
where
    P: Processes + Send + 'static,
    P::Held: Send + 'static,
{
    fn output(&self) -> OutputSource {
        match &self.logs {
            Some((path, offset)) => OutputSource::LogFile {
                path: path.clone(),
                offset: *offset,
                marker: Some(marker_of(&self.marked)),
            },
            None => OutputSource::None,
        }
    }

    /// Blocks on the held process, under no bound: the run lasts as long as it lasts. Then reads the
    /// record its shim wrote. A run with none is [`Error::RunUnrecorded`].
    fn wait(self) -> Result<Exit> {
        let Self {
            id,
            runs,
            mut processes,
            following,
            pin: _pin,
            ..
        } = self;
        let instance = match following {
            Following::Process { instance, process } => {
                match processes.await_exit(&process, Budget::Unbounded.start()) {
                    Ok(Waited::Confirmed) => {}
                    Ok(Waited::Expired) => unreachable!("an unbounded deadline never expires"),
                    Err(e) => return Err(lost(&id, e)),
                }
                instance
            }
            Following::Ended(instance) => instance,
            Following::Done(record) => return exit_of(&id, record.outcome),
            Following::Unrecorded => return Err(unrecorded(&id, UNRECORDED, None)),
        };
        match runs.read(&instance) {
            Ok(Some(record)) => exit_of(&id, record.outcome),
            Ok(None) => Err(unrecorded(&id, UNRECORDED, None)),
            Err(e) => Err(unrecorded(
                &id,
                "its record could not be read",
                Some(Error::os("read it", e)),
            )),
        }
    }
}

/// How a run that left `outcome` ended.
fn exit_of(id: &Id, outcome: RunOutcome) -> Result<Exit> {
    match outcome {
        RunOutcome::Exited(code) => Ok(Exit::Code(code as i32)),
        RunOutcome::Stopped => Ok(Exit::Code(CLOSED as i32)),
        RunOutcome::IncompleteStop => Err(incomplete_stop(id, "run")),
    }
}

/// A run whose exit was not recorded, for `reason`, which `source` (the failure that left it so) follows.
fn unrecorded(id: &Id, reason: &str, source: Option<Error>) -> Error {
    Error::RunUnrecorded {
        id: id.to_string(),
        reason: reason.to_owned(),
        source: source.map(Box::new),
    }
}

/// A run that began, whose exit goetia cannot establish because `what` failed with `cause`.
fn unfollowed(id: &Id, what: &str, cause: Error) -> Error {
    unrecorded(id, &format!("goetia could not {what}"), Some(cause))
}

/// The wait for the run of `id`'s process failed: the run may still be going, so this is in doubt, as a
/// request goetia lost track of is.
fn lost(id: &Id, source: io::Error) -> Error {
    Error::RequestInDoubt {
        request: format!("start `{id}` in Task Scheduler"),
        reached: true,
        note: Some("its run could not be waited for".to_string()),
        source: Box::new(Error::os(
            format!("wait for the process of `{id}`'s run to exit"),
            source,
        )),
    }
}

impl Scm {
    /// [`Self::task_begin_run_with`] over the machine's processes and the task's records.
    pub(in crate::backend::scm) fn task_begin_run(&self, snapshot: TaskSnapshot) -> Result<TaskRun> {
        let runs = Runs::of(&program_data()?, snapshot.id());
        self.task_begin_run_with(snapshot, runs, System)
    }

    /// Starts the task, and holds the run it began. In this order:
    ///
    /// 1. The pin is taken, which lists the records there are, and the log's length is measured: what
    ///    the request needs, before it is sent. A failure of either is the call's error with nothing sent.
    /// 2. `Run` is asked for. A failure is the call's error.
    /// 3. The runs that are alive are read, and the records that appeared after the pin, and
    ///    [`identify`] says which run was ours. A failure from here is a run that may have begun:
    ///    [`Error::RunUnrecorded`].
    /// 4. A run that is alive has its process held, by its pid and confirmed by a second read of the
    ///    listing; one whose process is gone then has ended, and is read by its instance.
    ///
    /// There is no wait for a pid or for the listing to drop: every listed run has a pid (L1), and
    /// starts are accepted while an ended run is listed (L2).
    pub(super) fn task_begin_run_with<P: Processes>(
        &self,
        snapshot: TaskSnapshot,
        runs: Runs,
        mut processes: P,
    ) -> Result<TaskRun<P>> {
        let id = snapshot.id().clone();
        let path = TaskPath::of(&id);
        let logs = snapshot
            .blob()
            .map_err(|e| Error::Kept(Arc::clone(e)))?
            .spec
            .common()
            .logs
            .clone();

        let pin = runs
            .pin()
            .map_err(|e| Error::os(format!("pin the run records of `{id}`"), e))?;
        let logs = logs
            .map(|path| output::length_of(&path).map(|offset| (path, offset)))
            .transpose()?;

        let started = self.tasks.run(&path)?;

        let live = live_runs(&*self.tasks, "run", &id, &path, |pid| processes.alive(pid))
            .map_err(|e| unfollowed(&id, "tell which run it began", e))?;
        let appeared = pin.appeared().map_err(|e| {
            unfollowed(
                &id,
                "tell which run it began",
                Error::os(format!("read the run records of `{id}`"), e),
            )
        })?;
        let following = match identify(Some(&started), &live, &appeared) {
            Identity::Live { id: instance, pid } => {
                let listed = Instance { id: instance, pid };
                match hold_run(&*self.tasks, "run", &id, &path, &listed, &mut processes)
                    .map_err(|e| unfollowed(&id, "hold the process of the run it began", e))?
                {
                    Some(process) => Following::Process { instance, process },
                    None => Following::Ended(instance),
                }
            }
            Identity::Done(record) => Following::Done(record),
            Identity::Unrecorded => Following::Unrecorded,
        };
        let marked = match &following {
            Following::Process { instance, .. } | Following::Ended(instance) => *instance,
            Following::Done(record) => record.instance,
            Following::Unrecorded => started.id,
        };
        Ok(TaskRun {
            id,
            runs,
            processes,
            following,
            pin,
            logs,
            marked,
        })
    }
}

#[cfg(test)]
#[path = "run_tests.rs"]
mod run_tests;
