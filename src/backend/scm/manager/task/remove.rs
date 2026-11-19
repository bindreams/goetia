//! Removing a task: disable it so that nothing starts another run, close its run records to any new shim,
//! stop every run, delete the task and then its run records, and sweep the shim file nothing references
//! any more. None of it needs the account's password.
//!
//! A re-run finishes what a failure left. The steps run in an order whose every prefix is a state the
//! next `uninstall` reads and completes: a task that is registered and disabled, a task that is gone
//! with its records left (`uninstall` of an id that is absent removes them), or nothing.

use std::path::{Path, PathBuf};

use super::api::Registered;
use super::verbs::{Processes, System, gone, pin_records};
use super::{TaskPath, TaskSnapshot};
use crate::backend::InstalledService;
use crate::backend::scm::folder::program_data;
use crate::backend::scm::kill::HoldOutcome;
use crate::backend::scm::manager::{Scm, Uninstalled, shim_exe};
use crate::backend::scm::runs::{MarkerCheck, PauseKind, Runs};
use crate::backend::scm::shim::lock::InstallLock;
use crate::backend::scm::task_xml::TaskXml;
use crate::error::{Error, Result};
use crate::manager::Budget;
use crate::spec::Id;

/// Where a failed removal left the task, which is what its message may promise.
enum Left {
    /// Disabling it failed: whether it is still enabled is not known.
    Unknown,
    /// The task is registered and disabled, so nothing starts it again.
    Disabled,
    /// The task is gone; its run records are not.
    Records,
    /// There was no task; run records left for it (by a failed install, an uninstall, or a task deleted
    /// outside goetia) are not all removed: some may be gone.
    LeftoverRecords,
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
        Left::LeftoverRecords => format!(
            "`{id}` is not installed, and run records left for it could not be removed. Re-run `goetia daemon \
             uninstall {id}` to remove them"
        ),
    };
    Error::Step {
        what,
        source: Box::new(cause),
    }
}

/// What an uninstall says on stderr when its removal of the run records has to wait, each time the path it
/// waits for changes: the manager has no channel for a notice, so it is said as its warnings are. A
/// directory that is not empty with nothing known to hold what is in it is said as that, and claims no
/// holder.
fn waiting_for_records(waits_for: &Path, kind: PauseKind) {
    match kind {
        PauseKind::Held => eprintln!(
            "waiting for {} to be released by a `run` or another program that has it open",
            waits_for.display()
        ),
        PauseKind::Emptying => eprintln!("waiting for {} to empty", waits_for.display()),
    }
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
        self.uninstall_absent_with(id, &Runs::of(&program_data()?, id), InstallLock::acquire, || {
            self.service_uninstall_absent()
        })
    }

    /// [`Self::task_uninstall_absent`] over the record directory `runs` of `id`, and `lock`, which takes
    /// the install lock. The records are removed under it: an install that has just made them holds it,
    /// and would lose them. With no records there is nothing to guard, and nothing is taken. A removal
    /// that fails says that the id is not installed and that records left for it could not be removed;
    /// some may be gone by then.
    ///
    /// `sweep` runs whether or not the records were removed, after the lock is released. If both fail,
    /// both errors are kept, the records' first. A `lock` that fails ends the call: the sweep takes the
    /// same lock.
    pub(super) fn uninstall_absent_with<L>(
        &self,
        id: &Id,
        runs: &Runs,
        lock: impl FnOnce() -> Result<L>,
        sweep: impl FnOnce() -> Result<()>,
    ) -> Result<()> {
        let removed = if runs.is_present() {
            let _lock = lock()?;
            runs.remove_all(waiting_for_records)
                .map_err(|e| uninstall_failed(id, Left::LeftoverRecords, e))
        } else {
            Ok(())
        };
        let swept = sweep();
        Error::both_all(removed.err().into_iter().chain(swept.err())).map_or(Ok(()), Err)
    }

    /// The task and its records removed, with nothing swept: the caller holds the install lock. See
    /// [`Self::task_remove_with`].
    pub(in crate::backend::scm) fn task_remove(&self, snapshot: &TaskSnapshot) -> Result<()> {
        let runs = Runs::of(&program_data()?, snapshot.id());
        self.task_remove_with(snapshot, &runs, &mut System)
    }

    /// Removes `snapshot`'s task, in an order whose every prefix a re-run finishes:
    ///
    /// 1. The task is disabled (its own flag, `TaskApi::set_enabled`), whether or not it is armed, so
    ///    that nothing starts another run. That is no registration, so it needs no password and works
    ///    for an account whose password changed or that is gone: an uninstall never needs the password.
    /// 2. The removal's marker is placed ([`Runs::close`]): no shim of the task pins the records from
    ///    now on, and a shim that is listed has passed its own check of the marker before this, or is
    ///    stopped by the next step.
    /// 3. The uninstall's own pin is taken past that marker, which would refuse any other
    ///    ([`MarkerCheck::Ignore`]): it keeps the shims' prune from deleting a record before it is read.
    /// 4. Every run that is alive is stopped and its process waited for, with no bound: the exit is the
    ///    event. A run whose shim recorded an incomplete stop, or whose record cannot be read, keeps the
    ///    task, disabled, and the marker: the re-run takes its own pin past it and finishes. The pin is
    ///    let go once the records are read.
    /// 5. The task is deleted, then its run records ([`Runs::remove_all`]), which waits for any holder
    ///    (a shim that pinned before the marker leaves once it reads the task gone), with no bound, and
    ///    says on stderr what it waits for. The install lock is held throughout.
    ///
    /// A task that is gone has nothing to disable or stop; the rest finds nothing. A task with no
    /// directory of records has no marker to place and no pin to take.
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

        // The marker, then the own pin past it: a shim that checks the marker after this sees it, and one
        // that checked before is listed by the stop's first read, which comes after (design section 4).
        runs.close().map_err(|e| uninstall_failed(id, Left::Disabled, e))?;
        let pin = pin_records("uninstall", id, runs, MarkerCheck::Ignore)
            .map_err(|e| uninstall_failed(id, Left::Disabled, e))?;
        // The pin is released when the stop returns, so the delete is made with none of the uninstall's own.
        self.task_stop_with("uninstall", id, Budget::Unbounded, runs, pin, processes)
            .map_err(|e| uninstall_failed(id, Left::Disabled, e))?;

        match self.tasks.delete(&path) {
            Ok(_) => {}
            // The task is gone, and an empty folder is the scheduler's own: the uninstall is done. The
            // manager has no channel for a warning, so it is said on stderr as its other warnings are.
            Err(e @ Error::TaskFolderKept { .. }) => eprintln!("warning: {}", e.report()),
            Err(e) => return Err(uninstall_failed(id, Left::Disabled, e)),
        }
        runs.remove_all(waiting_for_records)
            .map_err(|e| uninstall_failed(id, Left::Records, e))
    }
}

#[cfg(test)]
#[path = "remove_tests.rs"]
mod remove_tests;
