//! What a test that registered a task removes when it ends. Exported as `testing::remove_task`.

use super::verbs::{Processes, System, gone, stop_runs};
use super::{Com, TaskApi};
use crate::backend::scm::instance::TaskPath;
use crate::backend::scm::shim::lock::InstallLock;
use crate::error::{Error, Result};
use crate::manager::Budget;
use crate::spec::Id;
use crate::testing::cleanup::{CLEANUP_BOUND, retrying, warn_left_behind};

/// Disables the task `\Goetia\<id>`, stops its runs and waits for the shim's process to exit, then
/// deletes it under the install lock, retrying a transient failure for [`CLEANUP_BOUND`], then warns and
/// lets go. A missing task is done. Never panics, so it is safe in a guard's `Drop` while a test unwinds.
///
/// The delete ends in the removal of the empty `\Goetia` folder, even for a task that was never there, and
/// an install makes the folder and registers its task under the install lock: without the lock, a guard
/// could remove the folder between the two and fail another test's install.
pub(crate) fn remove_task(id: &Id) {
    remove_task_with(&Com, id, &mut System, InstallLock::acquire);
}

/// [`remove_task`] over `tasks` and `processes`, and `lock`, which takes the install lock. A lock that
/// cannot be taken is warned of, and the task is deleted without it: a guard leaves nothing behind for want
/// of it. The stop is not made under the lock, since it waits for a shim and an install would wait for it.
fn remove_task_with<P: Processes, L>(
    tasks: &dyn TaskApi,
    id: &Id,
    processes: &mut P,
    lock: impl FnOnce() -> Result<L>,
) {
    let path = TaskPath::of(id);
    // So that a scheduled fire starts no run the delete would leave behind. Best effort, as an uninstall's
    // first step is not: a task that cannot be disabled (or is not there) is deleted all the same.
    warn_left_behind(disable_problem(&path, tasks.set_enabled(&path, false)));
    // A shim that outlives its task has nothing to stop it. A run that cannot be ended, or that does not
    // end in time, is warned of, and the task is deleted all the same.
    if let Err(e) = stop_runs(tasks, "cleanup", id, Budget::Bounded(CLEANUP_BOUND), processes) {
        warn_left_behind(Some(format!(
            "stop the runs of the task `{}`: {}",
            path.as_str(),
            e.report()
        )));
    }
    let _lock = lock()
        .inspect_err(|e| {
            warn_left_behind(Some(format!(
                "take the install lock to delete `{}`: {}",
                path.as_str(),
                e.report()
            )))
        })
        .ok();
    warn_left_behind(retrying(|| {
        tasks
            .delete(&path)
            .err()
            // The task is gone; a folder left empty is not what the guard removes.
            .filter(|e| !matches!(e, Error::TaskFolderKept { .. }))
            .map(|e| format!("delete the task `{}`: {}", path.as_str(), e.report()))
    }));
}

/// What a failed disable of the task at `path` says, or `None` for one that went through or found no
/// task.
fn disable_problem(path: &TaskPath, disabled: Result<()>) -> Option<String> {
    disabled.err().filter(|e| !gone(e)).map(|e| {
        format!(
            "disable the task `{}` (a scheduled fire can still start a run): {}",
            path.as_str(),
            e.report()
        )
    })
}

#[cfg(test)]
#[path = "cleanup_tests.rs"]
mod cleanup_tests;
