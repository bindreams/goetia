//! What a test that registered a task removes when it ends. Exported as `testing::remove_task`.

use super::write::{set_armed, stored_account};
use super::{Com, TaskApi};
use crate::backend::scm::instance::TaskPath;
use crate::backend::scm::task_xml::TaskXml;
use crate::spec::Id;
use crate::testing::cleanup::{retrying, warn_left_behind};

/// Disarms the task `\Goetia\<id>` if it is armed, then deletes it, retrying a transient failure for
/// [`CLEANUP_BOUND`](crate::testing::cleanup::CLEANUP_BOUND), then warns and lets go. A missing task
/// is done. Never panics, so it is safe in a guard's `Drop` while a test unwinds.
pub(crate) fn remove_task(id: &Id) {
    remove_task_with(&Com, id);
}

fn remove_task_with(tasks: &dyn TaskApi, id: &Id) {
    let path = TaskPath::of(id);
    disarm(tasks, &path);
    warn_left_behind(retrying(|| {
        tasks
            .delete(&path)
            .err()
            .map(|e| format!("delete the task `{}`: {}", path.as_str(), e.report()))
    }));
}

/// Registers the task at `path` again with its triggers disabled, so that a scheduled fire starts no
/// run the delete would leave behind. Best effort: what cannot be read or registered again (a real
/// user's task takes its password) is deleted armed.
fn disarm(tasks: &dyn TaskApi, path: &TaskPath) {
    let Ok(Some(registered)) = tasks.get(path) else {
        return;
    };
    if !TaskXml::armed(&registered.xml) {
        return;
    }
    if let Ok(account) = stored_account(&registered) {
        let _ = set_armed(tasks, path, &registered, false, &account, None);
    }
}

#[cfg(test)]
#[path = "cleanup_tests.rs"]
mod cleanup_tests;
