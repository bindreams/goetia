//! Tasks on the real Task Scheduler: the write side (install, change between a service and a task,
//! `enable`, `disable`, `uninstall`) and running one (`start`, `stop`, `kill`, `run`), through the
//! public manager. Every test registers and removes real tasks and takes the machine-wide install
//! lock, so it needs elevation like the rest of this binary (`scm_group`), and shares the labels the
//! lock's own tests poll by. A task's program is this binary, in one of the sentinel modes.

use std::path::Path;

use goetia::backend::scm::manager::Scm;
use goetia::decide::Outcome;
use goetia::error::Error;
use goetia::manager::{Budget, Exit, ServiceManager as _, State, Status, TaskStatus};
use goetia::spec::{DaemonSpec, Id, Protocol, ServiceFields, ServiceSpec, TaskSpec, User};
use goetia::testing::{remove_task, service_fields, task};

use crate::common::conformance_mk;
use crate::managed::UNIT_DIR_EXCLUSIVE;
use crate::support::sentinel::{PRINT_AND_EXIT, RUN_UNTIL_KILLED};
use crate::support::{self, ConnectBack, Group, ServiceGuard, scm_group};

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

// Running =============================================================================================================

/// The task `id` whose program is `command` and whose output goes to `logs` where it has one. It has
/// no schedule: only a test starts it.
fn task_running(id: &Id, command: Vec<String>, logs: Option<&Path>) -> DaemonSpec {
    let DaemonSpec::Task(spec) = task(id.as_str(), command, None) else {
        unreachable!("`testing::task` builds a task")
    };
    let mut fields = spec.into_fields();
    fields.common.logs = logs.map(Path::to_path_buf);
    TaskSpec::new(fields).map(DaemonSpec::from).expect("a valid task")
}

/// This binary, told to print a line and exit with `code`.
fn prints_and_exits(code: i32, text: &str) -> Vec<String> {
    vec![
        support::current_exe_str(),
        PRINT_AND_EXIT.to_owned(),
        code.to_string(),
        text.to_owned(),
    ]
}

/// This binary, told to connect to `listener` and then run until it is stopped or killed.
fn runs_until_ended(listener: &ConnectBack) -> Vec<String> {
    vec![
        support::current_exe_str(),
        RUN_UNTIL_KILLED.to_owned(),
        listener.port().to_string(),
    ]
}

/// What ends a run for a test that fails while a `run` thread waits for it. A `thread::scope` joins its
/// threads when the body panics, and the `run` thread waits on a live shim, so without this a failing
/// assertion in the scope is a hang that leaves the task and its program running. Made right after the
/// thread is spawned; dropped, it removes the task, which stops its run and waits for the shim to exit,
/// so the thread returns. [`Self::defuse`] once the test has ended the run itself.
struct EndsTheRunOnPanic<'a>(Option<&'a Id>);

impl<'a> EndsTheRunOnPanic<'a> {
    fn new(id: &'a Id) -> Self {
        Self(Some(id))
    }

    fn defuse(mut self) {
        self.0 = None;
    }
}

impl Drop for EndsTheRunOnPanic<'_> {
    fn drop(&mut self) {
        if let Some(id) = self.0 {
            remove_task(id);
        }
    }
}

/// A `run` of `id` on its own thread, started before the program is released: its result once it ends.
fn run_of(mgr: &Scm, id: &Id) -> Result<Exit, Error> {
    mgr.run(id, &mut Vec::new())
}

#[skuld::test(labels = [UNIT_DIR_EXCLUSIVE], serial = UNIT_DIR_EXCLUSIVE)]
fn run_returns_the_programs_exit_and_its_lines(#[fixture(scm_group)] _group: &Group) {
    // Declared first, so it goes last: the task is removed before the directory its log is in.
    let dir = tempfile::tempdir().expect("tempdir");
    let guard = TaskGuard::new();
    let id = &guard.id;
    let mgr = support::stock_shim_manager();
    let spec = task_running(id, prints_and_exits(7, "hello"), Some(&dir.path().join("job.log")));
    mgr.install(&spec, false).expect("install");

    let mut out = Vec::new();
    let exit = mgr.run(id, &mut out).expect("run");

    assert_eq!(exit, Exit::Code(7), "the program's own exit");
    let out = String::from_utf8(out).expect("the program wrote text");
    assert!(
        out.lines().count() == 1 && out.starts_with("hello "),
        "the run's line, and no marker: {out:?}"
    );
    assert_eq!(task_status(&mgr, id).common.state, State::Idle);
    mgr.uninstall(id).expect("uninstall");
}

#[skuld::test(labels = [UNIT_DIR_EXCLUSIVE], serial = UNIT_DIR_EXCLUSIVE)]
fn a_second_run_shows_none_of_the_first_runs_lines(#[fixture(scm_group)] _group: &Group) {
    let dir = tempfile::tempdir().expect("tempdir");
    let guard = TaskGuard::new();
    let id = &guard.id;
    let mgr = support::stock_shim_manager();
    let spec = task_running(id, prints_and_exits(0, "run"), Some(&dir.path().join("job.log")));
    mgr.install(&spec, false).expect("install");

    let (mut first, mut second) = (Vec::new(), Vec::new());
    mgr.run(id, &mut first).expect("the first run");
    mgr.run(id, &mut second).expect("the second run");

    let (first, second) = (
        String::from_utf8(first).expect("text"),
        String::from_utf8(second).expect("text"),
    );
    assert_eq!(first.lines().count(), 1, "{first:?}");
    assert_eq!(second.lines().count(), 1, "only the second run's own line: {second:?}");
    assert_ne!(first, second, "each run prints its own process's id");
    mgr.uninstall(id).expect("uninstall");
}

/// A stop ends the run through Task Scheduler, which gives the program its close event: the program ends
/// as a console process does, and the shim records it as a stop.
#[skuld::test(labels = [UNIT_DIR_EXCLUSIVE], serial = UNIT_DIR_EXCLUSIVE)]
fn stop_ends_a_run_and_run_reports_the_stopped_code(#[fixture(scm_group)] _group: &Group) {
    let guard = TaskGuard::new();
    let id = &guard.id;
    let mgr = support::stock_shim_manager();
    let listener = ConnectBack::listen();
    mgr.install(&task_running(id, runs_until_ended(&listener), None), false)
        .expect("install");

    std::thread::scope(|scope| {
        let run = scope.spawn(|| run_of(&mgr, id));
        let ends_the_run = EndsTheRunOnPanic::new(id);
        listener.accept("the task's program to start");

        mgr.stop(id, Budget::DEFAULT).expect("stop");
        ends_the_run.defuse();

        let exit = run.join().expect("the run thread").expect("run");
        assert_eq!(exit, Exit::Code(0xC000_013A_u32 as i32), "a stopped run's code");
    });
    assert_eq!(task_status(&mgr, id).common.state, State::Idle);
    mgr.uninstall(id).expect("uninstall");
}

/// A kill ends the shim's process, which writes no record: the run's exit is unknown.
#[skuld::test(labels = [UNIT_DIR_EXCLUSIVE], serial = UNIT_DIR_EXCLUSIVE)]
fn kill_ends_a_run_and_run_reports_unrecorded(#[fixture(scm_group)] _group: &Group) {
    let guard = TaskGuard::new();
    let id = &guard.id;
    let mgr = support::stock_shim_manager();
    let listener = ConnectBack::listen();
    mgr.install(&task_running(id, runs_until_ended(&listener), None), false)
        .expect("install");

    std::thread::scope(|scope| {
        let run = scope.spawn(|| run_of(&mgr, id));
        let ends_the_run = EndsTheRunOnPanic::new(id);
        listener.accept("the task's program to start");

        mgr.kill(id).expect("kill");
        ends_the_run.defuse();

        let failed = run
            .join()
            .expect("the run thread")
            .expect_err("a killed run has no exit");
        assert!(matches!(failed, Error::RunUnrecorded { .. }), "{failed:?}");
    });
    assert_eq!(task_status(&mgr, id).common.state, State::Idle);
    mgr.uninstall(id).expect("uninstall");
}

#[skuld::test(labels = [UNIT_DIR_EXCLUSIVE], serial = UNIT_DIR_EXCLUSIVE)]
fn run_refuses_a_run_in_flight(#[fixture(scm_group)] _group: &Group) {
    let guard = TaskGuard::new();
    let id = &guard.id;
    let mgr = support::stock_shim_manager();
    let listener = ConnectBack::listen();
    mgr.install(&task_running(id, runs_until_ended(&listener), None), false)
        .expect("install");
    mgr.start(id, Budget::DEFAULT).expect("start");
    listener.accept("the task's program to start");

    let refused = run_of(&mgr, id).expect_err("a run in flight is not run again");
    assert!(matches!(refused, Error::AlreadyRunning { .. }), "{refused:?}");
    assert_eq!(task_status(&mgr, id).common.state, State::Running);

    mgr.stop(id, Budget::DEFAULT).expect("stop");
    assert_eq!(task_status(&mgr, id).common.state, State::Idle);
    mgr.uninstall(id).expect("uninstall");
}
