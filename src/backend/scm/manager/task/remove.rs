//! Removing a task: disable it so that nothing starts another run, stop every run, delete the task and its
//! run records, and sweep the shim file nothing references any more. None of it needs the account's
//! password.
//!
//! A re-run finishes what a failure left. The steps run in an order whose every prefix is a state the
//! next `uninstall` reads and completes: a task that is registered and disabled, a task that is gone
//! with its records left (`uninstall` of an id that is absent removes them), or nothing.

use std::io;
use std::os::windows::io::OwnedHandle;
use std::path::PathBuf;

use windows_sys::Win32::System::Threading::PROCESS_SYNCHRONIZE;

use super::api::{Registered, is_not_running};
use super::live::{Liveness, probe};
use super::{TaskPath, TaskSnapshot};
use crate::backend::InstalledService;
use crate::backend::scm::folder::program_data;
use crate::backend::scm::instance::InstanceId;
use crate::backend::scm::kill::HoldOutcome;
use crate::backend::scm::kill::system::{open_process_with, wait_exit};
use crate::backend::scm::manager::{Scm, Uninstalled, open_refused, shim_exe};
use crate::backend::scm::runs::{RunOutcome, RunRecord, Runs};
use crate::backend::scm::shim::lock::InstallLock;
use crate::backend::scm::task_xml::TaskXml;
use crate::error::{Error, Result};
use crate::manager::Budget;
use crate::spec::Id;

/// The processes of the runs a task lists, as a removal asks about them: whether one is alive, a hold
/// on one, and the wait for it to exit. [`System`] is the machine's; a test states them.
pub(super) trait Processes {
    /// An open process, which keeps its pid from being given to another.
    type Held;

    /// Whether the process behind `pid` runs. A process that cannot be judged is the error, never alive.
    fn alive(&mut self, pid: u32) -> io::Result<Liveness>;

    /// The process behind `pid` held open, or `None` if there is no such process.
    fn hold(&mut self, pid: u32) -> io::Result<Option<Self::Held>>;

    /// Blocks until `held` has exited. The exit is the event: there is no bound.
    fn wait_for_exit(&mut self, held: Self::Held) -> io::Result<()>;
}

/// The machine's own processes.
pub(super) struct System;

impl Processes for System {
    type Held = OwnedHandle;

    fn alive(&mut self, pid: u32) -> io::Result<Liveness> {
        probe(pid)
    }

    fn hold(&mut self, pid: u32) -> io::Result<Option<OwnedHandle>> {
        open_process_with(pid, PROCESS_SYNCHRONIZE)
    }

    fn wait_for_exit(&mut self, held: OwnedHandle) -> io::Result<()> {
        wait_exit(&held, Budget::Unbounded.start(), || {}).map(|_| ())
    }
}

/// Where a failed removal left the task, which is what its message may promise.
enum Left {
    /// Disabling it failed: whether it is still enabled is not known.
    Unknown,
    /// The task is registered and disabled, so nothing starts it again.
    Disabled,
    /// The task is gone; its run records are not.
    Records,
}

/// An `uninstall` of `id` that failed with `cause`, led by what it left and the retry that finishes it.
fn uninstall_failed(id: &Id, left: Left, cause: Error) -> Error {
    let what = match left {
        Left::Unknown => format!(
            "`{id}` was NOT deleted, and whether it is still enabled is not known. Re-run `goetia daemon uninstall \
             {id}` to retry"
        ),
        Left::Disabled => format!(
            "`{id}` was NOT deleted. It is disabled, so nothing starts it. Re-run `goetia daemon uninstall {id}` to \
             retry"
        ),
        Left::Records => format!(
            "`{id}` is deleted, but its run records could not be removed. Re-run `goetia daemon uninstall {id}` to \
             remove them"
        ),
    };
    Error::Step {
        what,
        source: Box::new(cause),
    }
}

/// What a stop whose shim recorded that it could not end the program's process tree leaves: the program
/// may still run, and the task is kept, disabled, until a stop ends it.
fn incomplete_stop(id: &Id) -> Error {
    uninstall_failed(
        id,
        Left::Disabled,
        Error::os(
            format!("stop `{id}`"),
            io::Error::other(
                "run stopped: goetia-shim recorded that it could not end the program's process tree, so the program \
                 may still be running",
            ),
        ),
    )
}

/// The shim file the stored task runs, when its file name is that of one a provider placed (the one
/// file an uninstall may sweep). `None` for any other host.
fn task_shim_host(registered: &Registered) -> Result<Option<PathBuf>> {
    Ok(TaskXml::executables(&registered.xml)?
        .into_iter()
        .find_map(|exe| shim_exe(PathBuf::from(exe.trim().trim_matches('"')))))
}

impl Scm {
    /// `uninstall` of a task: under the install lock, the task and its records are removed
    /// ([`Self::task_remove`]), and the shim file nothing references is swept as a service's uninstall
    /// sweeps it. The runs have exited by then, so no process is left to wait for.
    pub(in crate::backend::scm) fn task_uninstall(&self, snapshot: TaskSnapshot) -> Result<()> {
        let _lock = InstallLock::acquire()?;
        let host = task_shim_host(snapshot.registered());
        self.task_remove(&snapshot)?;
        self.after_uninstall(
            snapshot.id(),
            Uninstalled::Deleted {
                process: HoldOutcome::NoProcess,
            },
            host,
        )
    }

    /// What `uninstall` of an id that is absent does for a task: the run records an `uninstall` that
    /// deleted the task could not remove, and the sweep a service's does.
    pub(in crate::backend::scm) fn task_uninstall_absent(&self, id: &Id) -> Result<()> {
        self.uninstall_absent_with(&Runs::of(&program_data()?, id), InstallLock::acquire)
    }

    /// [`Self::task_uninstall_absent`] over the record directory `runs`, and `lock`, which takes the
    /// install lock. The records are removed under it: an install that has just made them holds it, and
    /// would lose them. With no records there is nothing to guard, and nothing is taken.
    pub(super) fn uninstall_absent_with<L>(&self, runs: &Runs, lock: impl FnOnce() -> Result<L>) -> Result<()> {
        if runs.is_present() {
            let _lock = lock()?;
            runs.remove_all()
                .map_err(|e| Error::os("remove the run records of a task that is gone", e))?;
        }
        self.service_uninstall_absent()
    }

    /// The task and its records removed, with nothing swept: the caller holds the install lock. See
    /// [`Self::task_remove_with`].
    pub(in crate::backend::scm) fn task_remove(&self, snapshot: &TaskSnapshot) -> Result<()> {
        let runs = Runs::of(&program_data()?, snapshot.id());
        self.task_remove_with(snapshot, &runs, &mut System)
    }

    /// Removes `snapshot`'s task, in an order whose every prefix a re-run finishes:
    ///
    /// 1. The task is disabled (its own flag, [`TaskApi::set_enabled`]), whether or not it is armed, so
    ///    that nothing starts another run. That is no registration, so it needs no password and works
    ///    for an account whose password changed or that is gone: an uninstall never needs the password.
    /// 2. Every run that is alive is stopped and its process waited for, with no bound: the exit is the
    ///    event. A run whose shim recorded an incomplete stop, or whose record cannot be read, keeps the
    ///    task, disabled.
    /// 3. The task is deleted, then its run records.
    ///
    /// A task that is gone has nothing to disable or stop; the rest finds nothing.
    pub(super) fn task_remove_with<P: Processes>(
        &self,
        snapshot: &TaskSnapshot,
        runs: &Runs,
        processes: &mut P,
    ) -> Result<()> {
        let id = snapshot.id();
        let path = TaskPath::of(id);

        match self.tasks.set_enabled(&path, false) {
            Ok(()) => {}
            Err(e) if gone(&e) => {}
            Err(e) => return Err(uninstall_failed(id, Left::Unknown, e)),
        }

        let stopped = self
            .stop_runs(id, &path, processes)
            .map_err(|e| uninstall_failed(id, Left::Disabled, e))?;
        for instance in &stopped {
            match runs.read(instance) {
                Ok(Some(RunRecord {
                    outcome: RunOutcome::IncompleteStop,
                    ..
                })) => return Err(incomplete_stop(id)),
                Ok(_) => {}
                // A record that cannot be read does not say the stop was complete.
                Err(e) => {
                    return Err(uninstall_failed(
                        id,
                        Left::Disabled,
                        Error::os(format!("read the record of `{id}`'s stopped run"), e),
                    ));
                }
            }
        }

        self.tasks
            .delete(&path)
            .map_err(|e| uninstall_failed(id, Left::Disabled, e))?;
        runs.remove_all().map_err(|e| {
            uninstall_failed(
                id,
                Left::Records,
                Error::os(format!("remove the run records of `{id}`"), e),
            )
        })
    }

    /// Stops every run of the task at `path` that is alive and waits for its process to exit, and says
    /// which. A listing is not proof that a run is alive, so each entry's process is judged; Task
    /// Scheduler holds its own handle to a run's process while it lists the pid, so a process held
    /// and then found still listed is the run's, and one that is not listed any more is not touched,
    /// since its pid may be another process's. A `Stop` that finds the run ended is a stop. A task
    /// that is gone has no runs.
    fn stop_runs<P: Processes>(&self, id: &Id, path: &TaskPath, processes: &mut P) -> Result<Vec<InstanceId>> {
        let live = match self.live_runs("uninstall", id, path, |pid| processes.alive(pid)) {
            Ok(live) => live,
            Err(e) if gone(&e) => return Ok(Vec::new()),
            Err(e) => return Err(e),
        };
        let mut stopped = Vec::new();
        for instance in live {
            let pid = instance.pid;
            let held = processes.hold(pid).map_err(|e| {
                open_refused(
                    "uninstall",
                    &format!("the process of `{id}`'s run (pid {pid})"),
                    &format!("open the process of `{id}`'s run (pid {pid}) to wait for its exit"),
                    e,
                )
            })?;
            let Some(held) = held else { continue };
            match self.tasks.instances(path) {
                Ok(listed) if listed.contains(&instance) => {}
                Ok(_) => continue,
                Err(e) if gone(&e) => continue,
                Err(e) => return Err(e),
            }
            match self.tasks.stop(path, &instance.id) {
                Ok(()) => {}
                Err(e) if is_not_running(&e) || gone(&e) => {}
                Err(e) => return Err(e),
            }
            processes
                .wait_for_exit(held)
                .map_err(|e| Error::os(format!("wait for `{id}`'s run (pid {pid}) to exit"), e))?;
            stopped.push(instance.id);
        }
        Ok(stopped)
    }
}

/// Whether `error` says the task is not there.
fn gone(error: &Error) -> bool {
    matches!(error.cause(), Error::NotInstalled { .. })
}

#[cfg(test)]
#[path = "remove_tests.rs"]
mod remove_tests;
