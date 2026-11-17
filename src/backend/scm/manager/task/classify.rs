//! What is at an id when two stores can hold it: the SCM (a service) and Task Scheduler (a task). Pure.
//!
//! A service wins. An id goetia marked in both stores reads as the service, and a foreign service
//! is foreign whatever Task Scheduler holds, so each `uninstall` removes one store's artifact and
//! running it again finishes (a retype interrupted between its two halves). Task Scheduler is only
//! asked about an id the SCM does not hold, so an unusable one never reaches a service's verbs.

use super::read::TaskSnapshot;
use crate::backend::ReadOutcome;
use crate::backend::scm::manager::{ScmSnapshot, WindowsSnapshot};
use crate::error::Result;

/// What a read is for. It decides one row of [`which`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::backend::scm) enum Purpose {
    /// Every read but the one a service's install makes: a task store that cannot be read leaves an
    /// id neither absent nor ours.
    Verb,
    /// The read a service's install makes. A machine whose Task Scheduler is unusable still
    /// installs a new service, so a failed task read says nothing about whether the SCM's id is
    /// free.
    ServiceInstall,
}

/// What is at the id the SCM answered `service` for, asking the task store (`task`) only if the SCM
/// holds nothing there.
///
/// | SCM                | Task Scheduler       | result                                      |
/// |--------------------|----------------------|---------------------------------------------|
/// | marked, or foreign | not asked            | what the SCM says                           |
/// | absent             | marked or foreign    | what Task Scheduler says                    |
/// | absent             | absent               | absent                                      |
/// | absent             | read failed          | the failure, or absent for a service install |
pub(in crate::backend::scm) fn which(
    service: ReadOutcome<ScmSnapshot>,
    task: impl FnOnce() -> Result<ReadOutcome<TaskSnapshot>>,
    purpose: Purpose,
) -> Result<ReadOutcome<WindowsSnapshot>> {
    match service {
        ReadOutcome::Absent => match task() {
            Ok(read) => Ok(read.map(WindowsSnapshot::Task)),
            Err(_) if purpose == Purpose::ServiceInstall => Ok(ReadOutcome::Absent),
            Err(e) => Err(e),
        },
        read => Ok(read.map(WindowsSnapshot::Service)),
    }
}

#[cfg(test)]
#[path = "classify_tests.rs"]
mod classify_tests;
