//! The one boundary to Task Scheduler. [`TaskApi`] is the whole of what goetia asks of it, in plain
//! strings and numbers: `Com` answers from the real service, and the test `Fake` from a table. An
//! error is `Error::os`-shaped with the HRESULT kept in its source, or `Error::NotInstalled` where
//! the query completed and found no such task.
//!
//! Every method takes its own COM apartment for its duration, so a verb on the caller's thread and the
//! thread `flow::run` waits on both work.

use std::time::SystemTime;

use windows::Win32::Foundation::SYSTEMTIME;
use windows::Win32::System::Variant::VariantTimeToSystemTime;

use super::clock::local_to_system;
use crate::backend::scm::instance::{InstanceId, TaskPath};
use crate::error::Result;

/// One run of a task that Task Scheduler lists. A listing is not proof the run is alive: the entry
/// outlives the process.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Instance {
    pub id: InstanceId,
    /// The process of the run, or `None` while the engine has not started it.
    pub pid: Option<u32>,
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
    pub pid: Option<u32>,
}

/// The account a task runs as: the XML's principal and the registration's logon type, kept in one
/// value so they cannot disagree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum TaskAccount {
    /// `SYSTEM`, `LocalService` or `NetworkService`: no password.
    Service(String),
    /// A user account, which logs on with its password.
    User(String),
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

    /// Deletes the task: whether it existed. `\Goetia` goes with its last task.
    fn delete(&self, path: &TaskPath) -> Result<bool>;

    /// The runs Task Scheduler lists for the task: not proof of life.
    fn instances(&self, path: &TaskPath) -> Result<Vec<Instance>>;

    fn info(&self, path: &TaskPath) -> Result<TaskInfo>;

    /// Starts the task now and returns the run it listed.
    fn run(&self, path: &TaskPath) -> Result<Started>;

    /// Asks Task Scheduler to end the run `instance`. A run it no longer lists is done.
    fn stop(&self, path: &TaskPath, instance: &InstanceId) -> Result<()>;
}

/// The instant of one of Task Scheduler's date properties, a COM `DATE` in local time.
///
/// M1: a task with no last run or no next run reads as OLE date zero (1899-12-30); that, and what is
/// not a date at all, is `None`.
pub(crate) fn ole_date(date: f64) -> Option<SystemTime> {
    if !date.is_finite() || date <= 0.0 {
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
