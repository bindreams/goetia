//! The task half of the Windows backend: a `type: task` daemon is a Task Scheduler task. Everything
//! that touches Task Scheduler goes through [`TaskApi`], so the logic over it runs against a table of
//! stated answers on every host that can build it, and the real service is reached on the Windows CI
//! legs.

pub(crate) mod api;
#[cfg(any(test, feature = "testing"))]
pub(crate) mod cleanup;
mod clock;
mod com;
#[cfg(test)]
pub(crate) mod fake;

pub(crate) use api::TaskApi;
pub(crate) use com::Com;
