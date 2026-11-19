//! The task half of the Windows backend: a `type: task` daemon is a Task Scheduler task. Everything
//! that touches Task Scheduler goes through [`TaskApi`], so the logic over it runs against a table of
//! stated answers on every host that can build it, and the real service is reached on the Windows CI
//! legs.

pub(crate) mod api;
mod classify;
#[cfg(any(test, feature = "testing"))]
pub(crate) mod cleanup;
pub(crate) mod clock;
mod com;
#[cfg(test)]
pub(crate) mod fake;
mod live;
mod read;
mod remove;
mod run;
mod status;
#[cfg(test)]
mod support;
mod verbs;
mod write;

pub(crate) use api::TaskApi;
pub(in crate::backend::scm) use classify::{Purpose, which};
pub(crate) use com::Com;
pub(crate) use read::TaskSnapshot;
pub(in crate::backend::scm) use read::combined;
pub(crate) use run::TaskRun;
pub(in crate::backend::scm) use write::task_beside;
#[cfg(any(test, feature = "testing"))]
pub(crate) use write::{registration_password, stored_account};

use crate::backend::scm::instance::TaskPath;

/// Starts the task `id` through Task Scheduler's own `Run`, as no verb of goetia does (a verb pins the run
/// records first), and returns its `LastTaskResult` once a run has ended: for a test of the shim host
/// as Task Scheduler starts it. The wait re-reads the result, which is the state of a run that has not ended
/// (never run, or running) until one has; the pace only keeps the loop from spinning.
#[cfg(any(test, feature = "testing"))]
pub(crate) fn run_until_it_ends(id: &crate::spec::Id) -> crate::error::Result<i32> {
    const PACE: std::time::Duration = std::time::Duration::from_millis(100);

    let path = TaskPath::of(id);
    Com.run(&path)?;
    loop {
        let result = Com.info(&path)?.last_result;
        if status::exit_of(result).is_some() {
            return Ok(result);
        }
        std::thread::sleep(PACE);
    }
}
