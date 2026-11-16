//! Tasks on the real Task Scheduler: the write side (install, change between a service and a task,
//! `enable`, `disable`, `uninstall`) through the public manager. Every test registers and removes
//! real tasks and takes the machine-wide install lock, so it needs elevation like the rest of this
//! binary (`scm_group`), and shares the labels the lock's own tests poll by.
//!
//! A task is only registered here, never started: running one is the shim's own suite.

use goetia::backend::scm::manager::Scm;
use goetia::decide::Outcome;
use goetia::error::Error;
use goetia::manager::{ServiceManager as _, State, Status, TaskStatus};
use goetia::spec::{DaemonSpec, Id, Protocol, ServiceFields, ServiceSpec, TaskSpec, User};
use goetia::testing::{remove_task, service_fields, task};

use crate::common::conformance_mk;
use crate::managed::UNIT_DIR_EXCLUSIVE;
use crate::support::{self, Group, ServiceGuard, scm_group};

const SCHEDULE: &str = "30 3 * * *";

/// Removes the task `id`, its run records and a service of the same id when it goes out of scope, and
/// before the test begins, in case an earlier run left them. Declared before the thing it removes.
struct TaskGuard {
    id: Id,
    _service: ServiceGuard,
}

impl TaskGuard {
    fn new() -> Self {
        let id = Id::try_from(support::random_test_id().as_str()).expect("a test id is an id");
        let guard = Self {
            _service: ServiceGuard::new(id.as_str()),
            id,
        };
        guard.remove();
        guard
    }

    fn remove(&self) {
        remove_task(&self.id);
        support::cleanup::remove_or_warn(&records_of(&self.id), true);
    }
}

impl Drop for TaskGuard {
    fn drop(&mut self) {
        self.remove();
    }
}

/// Where the shim of task `id` writes its run records.
fn records_of(id: &Id) -> std::path::PathBuf {
    support::program_data().join("Goetia").join("runs").join(id.as_str())
}

/// The task `id`, run by `user`, on a schedule: its program is never started here.
fn task_as(id: &Id, user: User, schedule: Option<&str>) -> DaemonSpec {
    let DaemonSpec::Task(spec) = task(id.as_str(), vec![support::current_exe_str()], schedule) else {
        unreachable!("`testing::task` builds a task")
    };
    let mut fields = spec.into_fields();
    fields.common.user = user;
    TaskSpec::new(fields).map(DaemonSpec::from).expect("a valid task")
}

fn service_as(id: &Id, protocol: Protocol) -> DaemonSpec {
    let fields = ServiceFields {
        protocol,
        ..service_fields(conformance_mk(id.as_str()))
    };
    DaemonSpec::from(ServiceSpec::new(fields).expect("a valid service"))
}

fn task_status(mgr: &Scm, id: &Id) -> TaskStatus {
    match mgr.status(id).expect("status of a task") {
        Status::Task(status) => status,
        Status::Service(status) => panic!("`{id}` reads as a service: {status:?}"),
    }
}

#[skuld::test(labels = [UNIT_DIR_EXCLUSIVE], serial = UNIT_DIR_EXCLUSIVE)]
fn a_task_installs_and_uninstalls(#[fixture(scm_group)] _group: &Group) {
    let guard = TaskGuard::new();
    let id = &guard.id;
    let mgr = support::stock_shim_manager();
    let spec = task_as(id, User::Root, Some(SCHEDULE));

    let outcome = mgr.install(&spec, false).expect("install");
    assert!(matches!(outcome, Outcome::Create), "{outcome:?}");

    let status = task_status(&mgr, id);
    assert_eq!(status.common.state, State::Idle);
    assert!(!status.common.enabled, "a task is installed disarmed");
    assert_eq!(status.next_run, None, "and a disarmed task has no next run");
    assert!(records_of(id).is_dir(), "the shim can write its records");
    let again = mgr.install(&spec, false).expect("install again");
    assert!(matches!(again, Outcome::UpToDate), "{again:?}");

    mgr.uninstall(id).expect("uninstall");

    let gone = mgr.status(id).expect_err("the task is gone");
    assert!(matches!(gone.cause(), Error::NotInstalled { .. }), "{gone:?}");
    assert!(!records_of(id).exists(), "its records go with it");
}

#[skuld::test(labels = [UNIT_DIR_EXCLUSIVE], serial = UNIT_DIR_EXCLUSIVE)]
fn enable_arms_and_disable_disarms_on_the_real_scheduler(#[fixture(scm_group)] _group: &Group) {
    let guard = TaskGuard::new();
    let id = &guard.id;
    let mgr = support::stock_shim_manager();
    let spec = task_as(id, User::Root, Some(SCHEDULE));
    mgr.install(&spec, false).expect("install");

    mgr.enable(id).expect("enable");
    let armed = task_status(&mgr, id);
    assert!(armed.common.enabled);
    assert!(armed.next_run.is_some(), "an armed daily task has a next run");
    let again = mgr.install(&spec, false).expect("install of an armed task");
    assert!(
        matches!(again, Outcome::UpToDate),
        "being armed is not drift: {again:?}"
    );
    assert!(
        task_status(&mgr, id).common.enabled,
        "a reinstall keeps the armed state"
    );

    mgr.disable(id).expect("disable");
    let disarmed = task_status(&mgr, id);
    assert!(!disarmed.common.enabled);
    assert_eq!(disarmed.next_run, None);

    mgr.uninstall(id).expect("uninstall");
}

/// A task's principal is written as a SID, and registered as it: the three accounts that need no
/// password. A task that is armed is disarmed by the uninstall before it goes.
#[skuld::test(labels = [UNIT_DIR_EXCLUSIVE], serial = UNIT_DIR_EXCLUSIVE)]
fn each_well_known_account_installs_arms_and_uninstalls_a_task(#[fixture(scm_group)] _group: &Group) {
    let mgr = support::stock_shim_manager();
    for user in ["root", "LocalService", "NetworkService"] {
        let guard = TaskGuard::new();
        let id = &guard.id;
        let user = if user == "root" {
            User::Root
        } else {
            User::Name(user.to_owned())
        };
        let spec = task_as(id, user.clone(), Some(SCHEDULE));

        mgr.install(&spec, false)
            .unwrap_or_else(|e| panic!("install as {user:?}: {}", e.report()));
        mgr.enable(id)
            .unwrap_or_else(|e| panic!("enable as {user:?}: {}", e.report()));
        assert!(task_status(&mgr, id).common.enabled, "{user:?}");
        mgr.uninstall(id)
            .unwrap_or_else(|e| panic!("uninstall of an armed task as {user:?}: {}", e.report()));
        assert!(mgr.status(id).is_err(), "{user:?}");
    }
}

/// A daemon changed between a service and a task: the old one is removed and the new created, in
/// either direction, for a service with and without the shim host.
#[skuld::test(labels = [UNIT_DIR_EXCLUSIVE], serial = UNIT_DIR_EXCLUSIVE)]
fn a_service_changes_to_a_task_and_back(#[fixture(scm_group)] _group: &Group) {
    let mgr = support::stock_shim_manager();
    for protocol in [Protocol::Native, Protocol::Plain] {
        let guard = TaskGuard::new();
        let id = &guard.id;
        let service = service_as(id, protocol);
        let as_task = task_as(id, User::Root, Some(SCHEDULE));
        mgr.install(&service, false).expect("install the service");

        let outcome = mgr.install(&as_task, false).expect("change it to a task");
        assert!(matches!(outcome, Outcome::Update { .. }), "{protocol:?}: {outcome:?}");
        assert_eq!(task_status(&mgr, id).common.state, State::Idle, "{protocol:?}");

        let back = mgr.install(&service, false).expect("change it back to a service");
        assert!(matches!(back, Outcome::Update { .. }), "{protocol:?}: {back:?}");
        assert!(
            matches!(mgr.status(id).expect("status"), Status::Service(_)),
            "{protocol:?}"
        );
        assert!(
            !records_of(id).exists(),
            "the task's records went with it: {protocol:?}"
        );
        mgr.uninstall(id).expect("uninstall");
    }
}
