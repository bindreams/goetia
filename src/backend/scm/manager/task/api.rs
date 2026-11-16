//! The one boundary to Task Scheduler. [`TaskApi`] is the whole of what goetia asks of it, in plain
//! strings and numbers: `Com` answers from the real service, and the test `Fake` from a table. An
//! error is `Error::os`-shaped with the HRESULT kept in its source, or `Error::NotInstalled` where
//! the query completed and found no such task.
//!
//! Every method takes its own COM apartment for its duration, so a verb on the caller's thread and the
//! thread `flow::run` waits on both work.

use std::time::SystemTime;

use windows::Win32::Foundation::{SCHED_E_TASK_DISABLED, SCHED_E_TASK_NOT_RUNNING, SYSTEMTIME};
use windows::Win32::System::Variant::VariantTimeToSystemTime;
use windows::core::HRESULT;

use super::clock::local_to_system;
use crate::backend::scm::instance::{InstanceId, TaskAccount, TaskPath};
use crate::error::{Error, Result};

/// One run of a task that Task Scheduler lists. A listing is not proof the run is alive: the entry
/// outlives the process.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Instance {
    pub id: InstanceId,
    /// The process of the run. A listed instance always has one: an entry that no longer offers its
    /// pid has ended and is not listed, and a pid of 0 is a failed read.
    pub pid: u32,
}

/// What a registered task says of itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Registered {
    /// The task's XML as Task Scheduler stores it.
    pub xml: String,
    /// The task's `Data` element, byte for byte.
    pub data: String,
}

/// Task Scheduler's own record of a task's runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TaskInfo {
    /// `None` if the task never ran.
    pub last_run: Option<SystemTime>,
    /// The raw `LastTaskResult`: an exit code, or a `SCHED_S_*` status.
    pub last_result: i32,
    /// `None` if no trigger will fire.
    pub next_run: Option<SystemTime>,
}

/// The run `Run` started.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Started {
    pub id: InstanceId,
    /// `None` for a `Run` that started nothing: an action that cannot start still returns a GUID.
    pub pid: Option<u32>,
}

/// How a [`TaskApi::register`] went.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Registration {
    Created,
    /// The task existed when it was to be created, or was gone when it was to be updated.
    Raced,
}

pub(crate) trait TaskApi: Send + Sync {
    /// The task's XML and data, or `None` if there is no such task.
    fn get(&self, path: &TaskPath) -> Result<Option<Registered>>;

    /// The names of the tasks in `\Goetia`; a folder that does not exist has none.
    fn names(&self) -> Result<Vec<String>>;

    /// Registers the task `xml` describes as `who`. `update` replaces an existing task, and a create
    /// leaves one alone ([`Registration::Raced`]). `password` is for a [`TaskAccount::User`] only;
    /// `None` keeps the credentials a task being updated has stored. A create makes `\Goetia` first.
    fn register(
        &self,
        path: &TaskPath,
        xml: &str,
        who: &TaskAccount,
        password: Option<&str>,
        update: bool,
    ) -> Result<Registration>;

    /// Sets the task's own `Enabled` flag (`IRegisteredTask.Enabled`, `Settings/Enabled` in its XML), which
    /// a registration is not: it needs no password, so it works for a password-logon task whose password
    /// changed and whose account is gone. A task that is not enabled is started by nothing, a `Run`
    /// included; the triggers' own flags, and a run that is alive, are as they were. A task that is not
    /// there is [`Error::NotInstalled`].
    fn set_enabled(&self, path: &TaskPath, enabled: bool) -> Result<()>;

    /// Deletes the task: whether it existed. Task Scheduler keeps the folder when its last task goes,
    /// so the call then asks to remove `\Goetia`, which Task Scheduler refuses while any task is in
    /// it: after the last task, and after a delete that found none, no folder is left empty. A folder
    /// that cannot be removed after a task was deleted is [`Error::TaskFolderKept`]: the task is gone.
    fn delete(&self, path: &TaskPath) -> Result<bool>;

    /// The runs Task Scheduler lists for the task: not proof of life.
    fn instances(&self, path: &TaskPath) -> Result<Vec<Instance>>;

    fn info(&self, path: &TaskPath) -> Result<TaskInfo>;

    /// Starts the task now (`Run`) and returns the run it names. Never `RunEx` with
    /// `TASK_RUN_IGNORE_CONSTRAINTS`: it bypasses `IgnoreNew`, so it would start a second instance
    /// beside a run that is alive.
    fn run(&self, path: &TaskPath) -> Result<Started>;

    /// Asks Task Scheduler to end the run `instance`. A run that has ended is not listed, so it is
    /// found nothing to stop and the call is `Ok`. A run that ends between the listing and the `Stop`
    /// makes Task Scheduler fail the `Stop` with `SCHED_E_TASK_NOT_RUNNING`: that is returned as the
    /// call's own `Error::Os`, with the HRESULT in its source, never as `NotInstalled`, and the
    /// caller maps it (to `Ok`: the run is stopped).
    fn stop(&self, path: &TaskPath, instance: &InstanceId) -> Result<()>;
}

/// Whether `error`, from [`TaskApi::stop`], says the run had ended: the one failure of a `Stop` that a
/// caller takes as a stop. Task Scheduler's own code is in the error's source chain.
pub(crate) fn is_not_running(error: &Error) -> bool {
    com_code(error) == Some(SCHED_E_TASK_NOT_RUNNING)
}

/// The code of the Task Scheduler (COM) failure in `error`'s source chain, if there is one.
pub(crate) fn com_code(error: &Error) -> Option<HRESULT> {
    let mut next: Option<&(dyn std::error::Error + 'static)> = Some(error);
    while let Some(error) = next {
        if let Some(com) = error.downcast_ref::<windows::core::Error>() {
            return Some(com.code());
        }
        next = error.source();
    }
    None
}

/// `error`, from the start of the task `id`, worded for a task whose own flag is false (`Run` refuses
/// one with `SCHED_E_TASK_DISABLED`): goetia leaves a task so only when an `uninstall` of it did not
/// finish, and running that again finishes it. Any other failure is as it was.
pub(crate) fn disabled_run(error: Error, id: &str) -> Error {
    if com_code(&error) != Some(SCHED_E_TASK_DISABLED) {
        return error;
    }
    Error::Step {
        what: format!(
            "`{id}` is disabled: an `uninstall` of it did not finish. Run `goetia daemon uninstall {id}` again to \
             remove it"
        ),
        source: Box::new(error),
    }
}

/// 1999-11-30 00:00 as an OLE date.
const NO_DATE: f64 = 36494.0;

/// The instant of one of Task Scheduler's date properties, a COM `DATE` in local time.
///
/// M1: a task that never ran reads as 1999-11-30 00:00, which Task Scheduler's engine uses for "no
/// date" (CI's first read of `LastRunTime`); OLE date zero (1899-12-30) is the same absence in
/// the other places COM uses one. Both, and what is not a date at all, are `None`.
pub(crate) fn ole_date(date: f64) -> Option<SystemTime> {
    if !date.is_finite() || date <= 0.0 || date == NO_DATE {
        return None;
    }
    let mut local = SYSTEMTIME::default();
    // SAFETY: `local` is a live SYSTEMTIME the call fills; a date it cannot convert returns zero.
    if unsafe { VariantTimeToSystemTime(date, &mut local) } == 0 {
        return None;
    }
    local_to_system(&local)
}

#[cfg(test)]
#[path = "api_tests.rs"]
mod api_tests;
