//! Removing a task: disable it so that nothing starts another run, close its run records to any new shim,
//! stop every run, delete the task and then its run records, and sweep the shim file nothing references
//! any more. None of it needs the account's password.
//!
//! A re-run finishes what a failure left. The steps run in an order whose every prefix is a state the
//! next `uninstall` reads and completes: a task that is registered and disabled, a task that is gone
//! with its records left (`uninstall` of an id that is absent removes them), or nothing.
//!
//! The task is disabled first, so a trigger that comes due after that does not fire; only a run launched
//! just before the disable can reach the marker and be refused by its shim. The marker stands from the
//! step that places it until the re-run (or an install run again with `--force`) finishes: a failure
//! before it leaves the task disabled with `enable` and `disable` working, and the verbs refuse after it
//! with the message of [`unfinished_change`](crate::backend::scm::runs::unfinished_change).
//!
//! [`RunInFlight::Leave`] is the same removal without the stop of the runs and without any wait ([`remove_records`]):
//! a run that is still going keeps its pin, so its records and the marker stay, which is the second state above,
//! until an `uninstall` or an `install` of the id after the run has ended.

use std::cell::Cell;
use std::path::{Path, PathBuf};

use super::api::Registered;
use super::verbs::{Processes, StopPin, System, gone, pin_records};
use super::{TaskPath, TaskSnapshot};
use crate::backend::InstalledService;
use crate::backend::scm::folder::program_data;
use crate::backend::scm::kill::HoldOutcome;
use crate::backend::scm::manager::{Scm, Uninstalled, shim_exe};
use crate::backend::scm::runs::{MarkerCheck, PauseKind, Removal, Runs};
use crate::backend::scm::shim::lock::InstallLock;
use crate::backend::scm::task_xml::TaskXml;
use crate::error::{Error, RECORD_HOLDERS, Result};
use crate::manager::{Budget, RunInFlight};
use crate::spec::Id;

/// Where a failed removal left the task, which is what its message may promise.
#[derive(Clone, Copy)]
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

/// An `uninstall` of `id` that failed with `cause`, led by what it left and the retry that finishes it. The retry
/// is made as the failed call was: a `Leave` that is retried without the flag would stop the run it was asked to
/// keep, or wait on the caller's own pin.
fn uninstall_failed(id: &Id, left: Left, in_flight: RunInFlight, cause: Error) -> Error {
    let command = match in_flight {
        RunInFlight::Stop => format!("goetia daemon uninstall {id}"),
        RunInFlight::Leave => format!("goetia daemon uninstall --no-stop {id}"),
    };
    let what = match left {
        Left::Unknown => {
            format!("`{id}` was NOT deleted, and whether it is still enabled is not known. Re-run `{command}` to retry")
        }
        Left::Disabled => {
            format!("`{id}` was NOT deleted. It is disabled, so nothing starts it. Re-run `{command}` to retry")
        }
        Left::Records => {
            format!("`{id}` is deleted, but its run records could not be removed. Re-run `{command}` to remove them")
        }
        Left::LeftoverRecords => format!(
            "`{id}` is not installed, and run records left for it could not be removed. Re-run `{command}` to remove \
             them"
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
        PauseKind::Held => eprintln!("waiting for {} to be released by {RECORD_HOLDERS}", waits_for.display()),
        PauseKind::Emptying => eprintln!("waiting for {} to empty", waits_for.display()),
    }
}

/// What a `--no-stop` uninstall says on stderr when it leaves run records, in the style of
/// [`waiting_for_records`]: what holds them and what removes them.
fn left_records(id: &Id, held: &Path) {
    eprintln!(
        "the run records of `{id}` were left: {} is held by {RECORD_HOLDERS}; `goetia daemon uninstall {id}` removes          them once it is released",
        held.display()
    );
}

/// The run records of a task that is removed, or of an id that is not installed. Under [`RunInFlight::Stop`] they
/// are removed, waiting with no bound for whatever holds them ([`Runs::remove_all`]). Under
/// [`RunInFlight::Leave`] nothing is waited for: one taking of `lock` places the marker and makes one pass
/// ([`Runs::remove_unheld`]), and what holds the records is left with the marker, its path told to `left`.
///
/// `lock` is what the removal takes for each step and lets go of before it waits: the install lock for an id that
/// is not installed, whose wait can last as long as a left run, and nothing for a caller that holds the install
/// lock for its whole call (and so cannot have the marker it placed cleared). Every locked pass of a wait first
/// checks that the marker stands: an install that took the lock meanwhile is never undone.
fn remove_records<G>(
    runs: &Runs,
    in_flight: RunInFlight,
    mut lock: impl FnMut() -> Result<G>,
    left: impl FnOnce(&Path),
) -> Result<()> {
    match in_flight {
        RunInFlight::Stop => runs.remove_all(waiting_for_records, lock),
        RunInFlight::Leave => {
            if let Removal::Left(held) = lock().and_then(|_held| runs.remove_unheld())? {
                left(&held);
            }
            Ok(())
        }
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
    /// sweeps it.
    pub(in crate::backend::scm) fn task_uninstall(&self, snapshot: TaskSnapshot, in_flight: RunInFlight) -> Result<()> {
        let _lock = InstallLock::acquire()?;
        let host = task_shim_host(snapshot.registered());
        self.task_remove(&snapshot, in_flight)?;
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
    pub(in crate::backend::scm) fn task_uninstall_absent(&self, id: &Id, in_flight: RunInFlight) -> Result<()> {
        self.uninstall_absent_with(
            id,
            &Runs::of(&program_data()?, id),
            in_flight,
            InstallLock::acquire,
            || self.service_uninstall_absent(),
        )
    }

    /// [`Self::task_uninstall_absent`] over the record directory `runs` of `id`, and `lock`, which takes the
    /// install lock for each step of the removal and is not held while it waits for a pin ([`remove_records`]),
    /// so that no install or uninstall waits for a run that lives on. A removal that fails says that the id is not
    /// installed and that records left for it could not be removed; some may be gone by then. A `lock` that
    /// cannot be taken is the call's own error, and the sweep, which takes the same lock, is not tried.
    ///
    /// `sweep` runs whether or not the records were removed, after the lock is released. If both fail, both
    /// errors are kept, the records' first. It is no stop and runs under either choice.
    pub(super) fn uninstall_absent_with<L>(
        &self,
        id: &Id,
        runs: &Runs,
        in_flight: RunInFlight,
        mut lock: impl FnMut() -> Result<L>,
        sweep: impl FnOnce() -> Result<()>,
    ) -> Result<()> {
        // A lock that cannot be taken is the call's own error, once: the sweep takes the same lock.
        let lock_failed = Cell::new(false);
        let removed = if runs.is_present() {
            let lock = || lock().inspect_err(|_| lock_failed.set(true));
            remove_records(runs, in_flight, lock, |held| left_records(id, held)).map_err(|e| {
                if lock_failed.get() {
                    e
                } else {
                    uninstall_failed(id, Left::LeftoverRecords, in_flight, e)
                }
            })
        } else {
            Ok(())
        };
        if lock_failed.get() {
            return removed;
        }
        let swept = sweep();
        Error::both_all(removed.err().into_iter().chain(swept.err())).map_or(Ok(()), Err)
    }

    /// The task and its records removed, with nothing swept: the caller holds the install lock. See
    /// [`Self::task_remove_with`].
    pub(in crate::backend::scm) fn task_remove(&self, snapshot: &TaskSnapshot, in_flight: RunInFlight) -> Result<()> {
        let runs = Runs::of(&program_data()?, snapshot.id());
        self.task_remove_with(snapshot, &runs, in_flight, &mut System)
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
    ///    (a shim that pinned before the marker leaves once it reads the task gone).
    ///
    /// Under [`RunInFlight::Leave`] steps 3 and 4 are skipped, and step 5 is one pass that leaves what holds the
    /// records ([`remove_records`]).
    ///
    /// A task that is gone has nothing to disable or stop; a failure after that says it is not installed
    /// and records are what is left. A task with no directory of records has no pin to take; its marker
    /// is placed all the same, unless the `runs` folder is gone too
    /// ([`Closed::NoRecords`](crate::backend::scm::runs::Closed::NoRecords)).
    pub(super) fn task_remove_with<P: Processes>(
        &self,
        snapshot: &TaskSnapshot,
        runs: &Runs,
        in_flight: RunInFlight,
        processes: &mut P,
    ) -> Result<()> {
        let id = snapshot.id();
        let path = TaskPath::of(id);

        // What a failure from here on leaves of the task, as the disable found it.
        let left = match self.tasks.set_enabled(&path, false) {
            Ok(()) => Left::Disabled,
            Err(e) if gone(&e) => Left::LeftoverRecords,
            Err(e) => return Err(uninstall_failed(id, Left::Unknown, in_flight, e)),
        };

        runs.close().map_err(|e| uninstall_failed(id, left, in_flight, e))?;
        if in_flight == RunInFlight::Stop {
            let pin = pin_records("uninstall", id, runs, MarkerCheck::Ignore)
                .map_err(|e| uninstall_failed(id, left, in_flight, e))?;
            self.task_stop_with(
                "uninstall",
                id,
                Budget::Unbounded,
                runs,
                StopPin::Caller(pin),
                processes,
            )
            .map_err(|e| uninstall_failed(id, left, in_flight, e))?;
        }

        match self.tasks.delete(&path) {
            Ok(_) => {}
            // The task is gone, and an empty folder is the scheduler's own: the uninstall is done. The
            // manager has no channel for a warning, so it is said on stderr as its other warnings are.
            Err(e @ Error::TaskFolderKept { .. }) => eprintln!("warning: {}", e.report()),
            Err(e) => return Err(uninstall_failed(id, left, in_flight, e)),
        }
        remove_records(runs, in_flight, || Ok(()), |held| left_records(id, held))
            .map_err(|e| uninstall_failed(id, Left::Records, in_flight, e))
    }
}

#[cfg(test)]
#[path = "remove_tests.rs"]
mod remove_tests;
