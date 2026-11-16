//! What a test that registered a task removes when it ends. Exported as `testing::remove_task`.

use super::{Com, TaskApi};
use crate::backend::scm::instance::TaskPath;
use crate::spec::Id;
use crate::testing::cleanup::{retrying, warn_left_behind};

/// Disables the task `\Goetia\<id>`, then deletes it, retrying a transient failure for
/// [`CLEANUP_BOUND`](crate::testing::cleanup::CLEANUP_BOUND), then warns and lets go. A missing task
/// is done. Never panics, so it is safe in a guard's `Drop` while a test unwinds.
pub(crate) fn remove_task(id: &Id) {
    remove_task_with(&Com, id);
}

fn remove_task_with(tasks: &dyn TaskApi, id: &Id) {
    let path = TaskPath::of(id);
    // So that a scheduled fire starts no run the delete would leave behind. Best effort, as an uninstall's
    // first step is not: a task that cannot be disabled (or is not there) is deleted all the same.
    let _ = tasks.set_enabled(&path, false);
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
