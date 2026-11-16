//! What a test that registered a task removes when it ends. Exported as `testing::remove_task`.

use super::verbs::{Processes, System, stop_runs};
use super::{Com, TaskApi};
use crate::backend::scm::instance::TaskPath;
use crate::manager::Budget;
use crate::spec::Id;
use crate::testing::cleanup::{CLEANUP_BOUND, retrying, warn_left_behind};

/// Disables the task `\Goetia\<id>`, stops its runs and waits for the shim's process to exit, then
/// deletes it, retrying a transient failure for [`CLEANUP_BOUND`], then warns and lets go. A missing task
/// is done. Never panics, so it is safe in a guard's `Drop` while a test unwinds.
pub(crate) fn remove_task(id: &Id) {
    remove_task_with(&Com, id, &mut System);
}

fn remove_task_with<P: Processes>(tasks: &dyn TaskApi, id: &Id, processes: &mut P) {
    let path = TaskPath::of(id);
    // So that a scheduled fire starts no run the delete would leave behind. Best effort, as an uninstall's
    // first step is not: a task that cannot be disabled (or is not there) is deleted all the same.
    let _ = tasks.set_enabled(&path, false);
    // A shim that outlives its task has nothing to stop it. A run that cannot be ended, or that does not
    // end in time, is warned of, and the task is deleted all the same.
    if let Err(e) = stop_runs(tasks, "cleanup", id, Budget::Bounded(CLEANUP_BOUND), processes) {
        warn_left_behind(Some(format!(
            "stop the runs of the task `{}`: {}",
            path.as_str(),
            e.report()
        )));
    }
    warn_left_behind(retrying(|| {
        tasks
            .delete(&path)
            .err()
            .map(|e| format!("delete the task `{}`: {}", path.as_str(), e.report()))
    }));
}

#[cfg(test)]
#[path = "cleanup_tests.rs"]
mod cleanup_tests;
