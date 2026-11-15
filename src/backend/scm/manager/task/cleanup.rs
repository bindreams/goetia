//! What a test that registered a task removes when it ends. Exported as `testing::remove_task`.

use super::{Com, TaskApi};
use crate::backend::scm::instance::TaskPath;
use crate::spec::Id;
use crate::testing::cleanup::{retrying, warn_left_behind};

/// Deletes the task `\Goetia\<id>` if it exists, retrying a transient failure for
/// [`CLEANUP_BOUND`](crate::testing::cleanup::CLEANUP_BOUND), then warns and lets go. A missing task
/// is done. Never panics, so it is safe in a guard's `Drop` while a test unwinds.
pub(crate) fn remove_task(id: &Id) {
    let path = TaskPath::of(id);
    warn_left_behind(retrying(|| {
        Com.delete(&path)
            .err()
            .map(|e| format!("delete the task `{}`: {}", path.as_str(), e.report()))
    }));
}
