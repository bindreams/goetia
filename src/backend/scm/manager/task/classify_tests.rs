use std::cell::Cell;

use super::*;
use crate::backend::scm::manager::task::api::Registered;
use crate::backend::scm::manager::task::read::TaskSnapshot;
use crate::backend::scm::parameters::Parameters;
use crate::backend::scm::test_support::blob_of;
use crate::error::Error;
use crate::spec::{Id, Protocol};

fn id() -> Id {
    Id::try_from("frpc").unwrap()
}

fn service() -> ScmSnapshot {
    ScmSnapshot::new(id(), Ok(blob_of(Protocol::Plain)), (Parameters::default(), None))
}

fn task() -> TaskSnapshot {
    TaskSnapshot::new(
        id(),
        Ok(blob_of(Protocol::Plain)),
        Registered {
            xml: String::new(),
            data: String::new(),
        },
    )
}

/// What the task store answers.
#[derive(Debug, Clone, Copy)]
enum Store {
    Marked,
    Foreign,
    Absent,
    Unusable,
}

impl Store {
    fn read(self) -> Result<ReadOutcome<TaskSnapshot>> {
        match self {
            Store::Marked => Ok(ReadOutcome::Marked(task())),
            Store::Foreign => Ok(ReadOutcome::Foreign { recovery: None }),
            Store::Absent => Ok(ReadOutcome::Absent),
            Store::Unusable => Err(Error::os(
                "read the task",
                std::io::Error::other("the Schedule service is not running"),
            )),
        }
    }
}

/// Where a read ended up, which is all these rows say.
#[derive(Debug, PartialEq, Eq)]
enum Found {
    Service,
    Task,
    Foreign,
    Absent,
    Failed,
}

/// `which` over a task store that answers `store`: where the read ended up, and whether the store
/// was asked at all.
fn asked(service: ReadOutcome<ScmSnapshot>, store: Store, purpose: Purpose) -> (Found, bool) {
    let was_asked = Cell::new(false);
    let read = which(
        service,
        || {
            was_asked.set(true);
            store.read()
        },
        purpose,
    );
    let found = match read {
        Ok(ReadOutcome::Marked(WindowsSnapshot::Service(_))) => Found::Service,
        Ok(ReadOutcome::Marked(WindowsSnapshot::Task(_))) => Found::Task,
        Ok(ReadOutcome::Foreign { .. }) => Found::Foreign,
        Ok(ReadOutcome::Absent) => Found::Absent,
        Err(_) => Found::Failed,
    };
    (found, was_asked.get())
}

const EVERY_STORE: [Store; 4] = [Store::Marked, Store::Foreign, Store::Absent, Store::Unusable];
const EVERY_PURPOSE: [Purpose; 2] = [Purpose::Verb, Purpose::ServiceInstall];

// A service wins ======================================================================================================

/// The task store is not even asked: a failing Task Scheduler never slows or breaks a service's verbs.
#[skuld::test]
fn a_marked_service_is_the_snapshot_whatever_the_task_store_says() {
    for store in EVERY_STORE {
        for purpose in EVERY_PURPOSE {
            assert_eq!(
                asked(ReadOutcome::Marked(service()), store, purpose),
                (Found::Service, false),
                "{store:?}, {purpose:?}"
            );
        }
    }
}

#[skuld::test]
fn a_foreign_service_is_foreign_whatever_the_task_store_says() {
    for store in EVERY_STORE {
        for purpose in EVERY_PURPOSE {
            assert_eq!(
                asked(ReadOutcome::Foreign { recovery: None }, store, purpose),
                (Found::Foreign, false),
                "{store:?}, {purpose:?}"
            );
        }
    }
}

// No service: the task store decides ==================================================================================

#[skuld::test]
fn with_no_service_a_marked_task_is_the_snapshot() {
    assert_eq!(
        asked(ReadOutcome::Absent, Store::Marked, Purpose::Verb),
        (Found::Task, true)
    );
}

#[skuld::test]
fn with_no_service_a_foreign_task_is_foreign() {
    assert_eq!(
        asked(ReadOutcome::Absent, Store::Foreign, Purpose::Verb),
        (Found::Foreign, true)
    );
}

#[skuld::test]
fn with_no_service_and_no_task_the_id_is_absent() {
    assert_eq!(
        asked(ReadOutcome::Absent, Store::Absent, Purpose::Verb),
        (Found::Absent, true)
    );
}

// The task store is unusable ==========================================================================================

/// Not "absent": nothing says the id is free.
#[skuld::test]
fn status_of_an_absent_id_fails_when_the_task_store_is_unreadable() {
    assert_eq!(
        asked(ReadOutcome::Absent, Store::Unusable, Purpose::Verb),
        (Found::Failed, true)
    );
}

/// A machine whose Task Scheduler is unusable still installs a new service.
#[skuld::test]
fn a_service_installs_with_task_scheduler_unusable() {
    assert_eq!(
        asked(ReadOutcome::Absent, Store::Unusable, Purpose::ServiceInstall),
        (Found::Absent, true)
    );
}

/// The install row is the only one that is not the read's error: a task found at the id is still the
/// id's occupant, and so is a foreign one.
#[skuld::test]
fn a_service_install_still_sees_what_the_task_store_holds_at_its_id() {
    assert_eq!(
        asked(ReadOutcome::Absent, Store::Marked, Purpose::ServiceInstall),
        (Found::Task, true)
    );
    assert_eq!(
        asked(ReadOutcome::Absent, Store::Foreign, Purpose::ServiceInstall),
        (Found::Foreign, true)
    );
    assert_eq!(
        asked(ReadOutcome::Absent, Store::Absent, Purpose::ServiceInstall),
        (Found::Absent, true)
    );
}
