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
#[cfg(any(test, feature = "testing"))]
pub(crate) use write::{StoredAccount, registration_password, stored_account};
pub(in crate::backend::scm) use write::{refuse_held_records_at, task_beside};

use crate::backend::scm::instance::TaskPath;
