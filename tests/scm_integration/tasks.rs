//! Tasks on the real Task Scheduler: the write side (install, change between a service and a task,
//! `enable`, `disable`, `uninstall`) and running one (`start`, `stop`, `kill`, `run`), through the
//! public manager. Every test registers and removes real tasks and takes the machine-wide install
//! lock, so it needs elevation like the rest of this binary (`scm_group`), and shares the labels the
//! lock's own tests poll by. A task's program is this binary, in one of the sentinel modes.

use std::path::{Path, PathBuf};

use goetia::backend::scm::manager::Scm;
use goetia::decide::Outcome;
use goetia::error::Error;
use goetia::manager::{Budget, Exit, ServiceManager as _, State, Status, TaskStatus, conformance};
use goetia::spec::{DaemonSpec, Id, Protocol, ServiceFields, ServiceSpec, User};
use goetia::testing::{remove_task, service_fields};

use crate::common::conformance_mk;
use crate::managed::UNIT_DIR_EXCLUSIVE;
use crate::support::sentinel::{PRINT_AND_EXIT, RUN_UNTIL_KILLED};
use crate::support::windows_tasks::{EndsTheRunOnPanic, ProcessHandle, task_spec};
use crate::support::{self, ConnectBack, Group, ServiceGuard, kill, scm_group};

pub(crate) const SCHEDULE: &str = "30 3 * * *";

/// A fresh test id, and what removes the task of that id, its run records and a service of it when the
/// guard goes out of scope (`ServiceGuard`). Declared before the thing it removes.
pub(crate) struct TaskGuard {
    pub id: Id,
    _guard: ServiceGuard,
}

impl TaskGuard {
    pub fn new() -> Self {
        let id = test_id();
        Self {
            _guard: ServiceGuard::new(id.as_str()),
            id,
        }
    }
}

/// A fresh test id, for a test that makes things its guard must outlive (and so makes the id before the
/// guard, with a [`ServiceGuard`] of it).
pub(crate) fn test_id() -> Id {
    Id::try_from(support::random_test_id().as_str()).expect("a test id is an id")
}

/// Where the shim of task `id` writes its run records.
pub(crate) fn records_of(id: &Id) -> PathBuf {
    support::task_records(id.as_str())
}

/// The task `id`, run by `user`, on a schedule: its program is never started here.
pub(crate) fn task_as(id: &Id, user: User, schedule: Option<&str>) -> DaemonSpec {
    task_spec(id, vec![support::current_exe_str()], schedule, user, None)
}

fn service_as(id: &Id, protocol: Protocol) -> DaemonSpec {
    let fields = ServiceFields {
        protocol,
        ..service_fields(conformance_mk(id.as_str()))
    };
    DaemonSpec::from(ServiceSpec::new(fields).expect("a valid service"))
}

pub(crate) fn task_status(mgr: &Scm, id: &Id) -> TaskStatus {
    match mgr.status(id).expect("status of a task") {
        Status::Task(status) => status,
        Status::Service(status) => panic!("`{id}` reads as a service: {status:?}"),
    }
}

/// The status of `id` once its run has ended and Task Scheduler has recorded how: idle, with a last
/// result. Read again with no cap, since the run's end is a state of the machine and not a time.
pub(crate) fn status_once_ended(mgr: &Scm, id: &Id) -> TaskStatus {
    let mut found = None;
    kill::until(|| {
        let status = task_status(mgr, id);
        let ended = status.common.state == State::Idle && status.last_result.is_some();
        found = Some(status);
        ended
    });
    found.expect("a status was read")
}

/// An install that finds a task up to date still sees that its default log is there: see
/// `managed::an_up_to_date_install_makes_a_deleted_default_log_again`.
#[skuld::test(labels = [UNIT_DIR_EXCLUSIVE], serial = UNIT_DIR_EXCLUSIVE)]
fn an_up_to_date_install_of_a_task_makes_a_deleted_default_log_again(#[fixture(scm_group)] _group: &Group) {
    let guard = TaskGuard::new();
    let id = &guard.id;
    let mgr = support::stock_shim_manager();
    let spec = task_as(id, User::Root, Some(SCHEDULE));
    let log = support::shim_default_log(&support::program_data(), id.as_str());
    mgr.install(&spec, false).expect("install");
    assert!(log.is_file(), "the install made {}", log.display());
    std::fs::remove_file(&log).unwrap_or_else(|e| panic!("delete {}: {e}", log.display()));

    let again = mgr.install(&spec, false).expect("install again");

    assert!(matches!(again, Outcome::UpToDate), "{again:?}");
    assert!(log.is_file(), "the install made {} again", log.display());
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
pub(crate) fn task_running(id: &Id, command: Vec<String>, logs: Option<&Path>) -> DaemonSpec {
    task_spec(id, command, None, User::Root, logs)
}

/// This binary, told to print a line and exit with `code`.
pub(crate) fn prints_and_exits(code: i32, text: &str) -> Vec<String> {
    vec![
        support::current_exe_str(),
        PRINT_AND_EXIT.to_owned(),
        code.to_string(),
        text.to_owned(),
    ]
}

/// This binary, told to connect to `listener` and then run until it is stopped or killed.
pub(crate) fn runs_until_ended(listener: &ConnectBack) -> Vec<String> {
    vec![
        support::current_exe_str(),
        RUN_UNTIL_KILLED.to_owned(),
        listener.port().to_string(),
    ]
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

#[skuld::test(labels = [UNIT_DIR_EXCLUSIVE], serial = UNIT_DIR_EXCLUSIVE)]
fn start_returns_before_the_run_ends(#[fixture(scm_group)] _group: &Group) {
    let guard = TaskGuard::new();
    let id = &guard.id;
    let mgr = support::stock_shim_manager();
    let listener = ConnectBack::listen();
    mgr.install(&task_running(id, runs_until_ended(&listener), None), false)
        .expect("install");

    // The program runs until it is stopped, so a `start` that waited for the run to end would never
    // return: it returns once Task Scheduler has accepted the run.
    mgr.start(id, Budget::DEFAULT).expect("start");

    listener.accept("the task's program to start");
    let status = task_status(&mgr, id);
    assert_eq!(status.common.state, State::Running);
    assert_eq!(status.last_result, None, "no run has ended");
    mgr.stop(id, Budget::DEFAULT).expect("stop");
    mgr.uninstall(id).expect("uninstall");
}

/// `restart --timeout 0` sends the stop and asks for the start without waiting for the run to end. A
/// run still alive makes Task Scheduler accept the `Run` and begin nothing, so the start is refused
/// naming it. No stop is sent here, so the run is alive whatever the timing.
#[skuld::test(labels = [UNIT_DIR_EXCLUSIVE], serial = UNIT_DIR_EXCLUSIVE)]
fn a_start_after_an_unwaited_stop_is_refused_while_the_run_is_alive(#[fixture(scm_group)] _group: &Group) {
    let guard = TaskGuard::new();
    let id = &guard.id;
    let mgr = support::stock_shim_manager();
    let listener = ConnectBack::listen();
    mgr.install(&task_running(id, runs_until_ended(&listener), None), false)
        .expect("install");
    mgr.start(id, Budget::DEFAULT).expect("start");
    listener.accept("the task's program to start");
    let pid = task_status(&mgr, id).common.pid.expect("a running task has a pid");

    let refused = mgr
        .request_start_after_stop(id)
        .expect_err("a start over a run still alive is refused");

    assert!(matches!(refused, Error::StillRunning { .. }), "{refused:?}");
    let status = task_status(&mgr, id);
    assert_eq!(status.common.state, State::Running);
    assert_eq!(status.common.pid, Some(pid), "the run it refused to start over");
    mgr.stop(id, Budget::DEFAULT).expect("stop");
    mgr.uninstall(id).expect("uninstall");
}

/// `run` of a task with no `logs` shows why it shows nothing, and still returns the run's exit.
#[skuld::test(labels = [UNIT_DIR_EXCLUSIVE], serial = UNIT_DIR_EXCLUSIVE)]
fn run_of_a_task_without_logs_says_why_it_shows_no_output(#[fixture(scm_group)] _group: &Group) {
    let guard = TaskGuard::new();
    let id = &guard.id;
    let mgr = support::stock_shim_manager();
    mgr.install(&task_running(id, prints_and_exits(3, "hello"), None), false)
        .expect("install");

    let mut out = Vec::new();
    let exit = mgr.run(id, &mut out).expect("run");

    assert_eq!(exit, Exit::Code(3));
    let out = String::from_utf8(out).expect("text");
    assert!(
        out.contains("has no `logs` file, so its output is not shown"),
        "the note that says why: {out:?}"
    );
    assert!(
        !out.contains("hello"),
        "the program's line has nowhere to be read from: {out:?}"
    );
    mgr.uninstall(id).expect("uninstall");
}

/// An ended run stays in Task Scheduler's listing for a few milliseconds, and a `run` straight after it
/// is neither refused as a run in flight nor made to wait for the listing.
#[skuld::test(labels = [UNIT_DIR_EXCLUSIVE], serial = UNIT_DIR_EXCLUSIVE)]
fn run_right_after_a_finished_run_is_not_refused_while_the_listing_lingers(#[fixture(scm_group)] _group: &Group) {
    let guard = TaskGuard::new();
    let id = &guard.id;
    let mgr = support::stock_shim_manager();
    mgr.install(&task_running(id, prints_and_exits(0, "run"), None), false)
        .expect("install");

    for which in ["first", "second", "third"] {
        let exit = mgr
            .run(id, &mut Vec::new())
            .unwrap_or_else(|e| panic!("the {which} run, straight after the one before: {}", e.report()));
        assert_eq!(exit, Exit::Code(0), "{which}");
    }
    mgr.uninstall(id).expect("uninstall");
}

/// A task whose definition changes after it has run is rewritten, and its next run is the new one's.
#[skuld::test(labels = [UNIT_DIR_EXCLUSIVE], serial = UNIT_DIR_EXCLUSIVE)]
fn editing_a_task_after_it_ran_works(#[fixture(scm_group)] _group: &Group) {
    let guard = TaskGuard::new();
    let id = &guard.id;
    let mgr = support::stock_shim_manager();
    mgr.install(&task_running(id, prints_and_exits(1, "before"), None), false)
        .expect("install");
    assert_eq!(mgr.run(id, &mut Vec::new()).expect("the first run"), Exit::Code(1));

    let edited = mgr
        .install(&task_running(id, prints_and_exits(2, "after"), None), false)
        .expect("the edit");

    assert!(matches!(edited, Outcome::Update { .. }), "{edited:?}");
    assert_eq!(
        mgr.run(id, &mut Vec::new()).expect("a run of the edited task"),
        Exit::Code(2)
    );
    mgr.uninstall(id).expect("uninstall");
}

/// An uninstall of a task that is armed and running disarms it, ends the run and waits for the shim to
/// exit, then removes the task.
#[skuld::test(labels = [UNIT_DIR_EXCLUSIVE], serial = UNIT_DIR_EXCLUSIVE)]
fn uninstall_of_a_running_armed_task_ends_it(#[fixture(scm_group)] _group: &Group) {
    let guard = TaskGuard::new();
    let id = &guard.id;
    let mgr = support::stock_shim_manager();
    let listener = ConnectBack::listen();
    let spec = task_spec(id, runs_until_ended(&listener), Some(SCHEDULE), User::Root, None);
    mgr.install(&spec, false).expect("install");
    mgr.enable(id).expect("enable");
    mgr.start(id, Budget::DEFAULT).expect("start");
    listener.accept("the task's program to start");
    let pid = task_status(&mgr, id).common.pid.expect("a running task has a pid");
    let shim = ProcessHandle::open(pid);

    mgr.uninstall(id).expect("uninstall of a running, armed task");

    let gone = mgr.status(id).expect_err("the task is gone");
    assert!(matches!(gone.cause(), Error::NotInstalled { .. }), "{gone:?}");
    assert!(
        shim.has_exited(),
        "the uninstall returned while the shim (pid {pid}) was still running"
    );
    assert!(!records_of(id).exists(), "its records go with it");
}

/// The tasks Task Scheduler lists under `\Goetia`, by the names `schtasks` gives them.
fn goetia_folder_tasks() -> Vec<String> {
    let listing = support::cmd::run("schtasks.exe", &["/query", "/fo", "csv", "/nh"]).expect_ok();
    listing
        .stdout
        .lines()
        .filter_map(|line| line.split(',').next())
        .map(|name| name.trim_matches('"'))
        .filter(|name| name.to_ascii_lowercase().starts_with(r"\goetia\"))
        .map(str::to_owned)
        .collect()
}

/// Whether Task Scheduler has the folder `\Goetia`.
pub(crate) fn goetia_folder_exists() -> bool {
    support::cmd::run("schtasks.exe", &["/query", "/tn", r"\Goetia\"]).ok()
}

/// Whether Task Scheduler has the task `\Goetia\<id>`.
pub(crate) fn task_exists(id: &Id) -> bool {
    support::cmd::run("schtasks.exe", &["/query", "/tn", &format!(r"\Goetia\{id}")]).ok()
}

/// Removes every task `\Goetia` holds, which an abrupt stop of an earlier run can have left, so that a test
/// whose subject is the folder's emptiness starts from an empty one. A task goetia can name is removed as a
/// guard removes it (its runs ended first); any other is deleted outright.
fn clear_goetia_folder() {
    for name in goetia_folder_tasks() {
        let id = name.rsplit('\\').next().and_then(|stem| Id::try_from(stem).ok());
        match id {
            Some(id) => {
                remove_task(&id);
                support::cleanup::remove_or_warn(&records_of(&id), true);
            }
            None => {
                let deleted = support::cmd::run("schtasks.exe", &["/delete", "/tn", &name, "/f"]);
                if !deleted.ok() {
                    eprintln!("warning: left behind: {deleted}");
                }
            }
        }
    }
}

#[skuld::test(labels = [UNIT_DIR_EXCLUSIVE], serial = UNIT_DIR_EXCLUSIVE)]
fn uninstall_removes_the_task_its_records_and_the_empty_folder(#[fixture(scm_group)] _group: &Group) {
    clear_goetia_folder();
    let guard = TaskGuard::new();
    let id = &guard.id;
    let mgr = support::stock_shim_manager();
    mgr.install(&task_running(id, prints_and_exits(0, "run"), None), false)
        .expect("install");
    mgr.run(id, &mut Vec::new()).expect("a run, which leaves a record");
    assert_eq!(
        goetia_folder_tasks(),
        [format!(r"\Goetia\{id}")],
        "precondition: this is the only task in the folder (leftovers were removed at the start), so its \
         removal empties it"
    );
    assert!(records_of(id).is_dir(), "precondition: the run left its records");

    mgr.uninstall(id).expect("uninstall");

    assert!(!task_exists(id), "the task");
    assert!(!records_of(id).exists(), "its records");
    assert!(!goetia_folder_exists(), "the folder it was the last task of");
}

/// An id held by something goetia did not make is refused by an install of either kind, with or without
/// `--force`, and what holds it is left as it was. A foreign service and a foreign task each hold the id
/// against both a task and a service.
#[skuld::test(labels = [UNIT_DIR_EXCLUSIVE], serial = UNIT_DIR_EXCLUSIVE)]
fn an_id_held_by_a_foreign_service_or_task_is_refused_both_ways(#[fixture(scm_group)] _group: &Group) {
    let mgr = support::stock_shim_manager();
    let refused = |holder: &str, id: &Id| {
        let specs = [
            ("a task", task_as(id, User::Root, Some(SCHEDULE))),
            ("a service", service_as(id, Protocol::Native)),
        ];
        for (what, spec) in specs {
            for force in [false, true] {
                let outcome = mgr
                    .install(&spec, force)
                    .unwrap_or_else(|e| panic!("{holder}, {what}, force {force}: {}", e.report()));
                assert!(
                    matches!(outcome, Outcome::RefuseForeign { .. }),
                    "{holder} holds the id against {what} (force {force}): {outcome:?}"
                );
            }
        }
    };

    let service = TaskGuard::new();
    support::cmd::run(
        "sc.exe",
        &[
            "create",
            service.id.as_str(),
            "binPath=",
            &support::current_exe_str(),
            "start=",
            "demand",
        ],
    )
    .expect_ok();
    refused("a foreign service", &service.id);
    assert!(
        support::cmd::run("sc.exe", &["query", service.id.as_str()]).ok(),
        "the foreign service is still there"
    );
    assert!(!task_exists(&service.id), "and no task was made beside it");

    let task = TaskGuard::new();
    support::cmd::run(
        "schtasks.exe",
        &[
            "/create",
            "/tn",
            &format!(r"\Goetia\{}", task.id),
            "/tr",
            "cmd.exe /c exit 0",
            "/sc",
            "onstart",
            "/ru",
            "SYSTEM",
            "/f",
        ],
    )
    .expect_ok();
    refused("a foreign task", &task.id);
    assert!(task_exists(&task.id), "the foreign task is still there");
    assert!(
        !support::cmd::run("sc.exe", &["query", task.id.as_str()]).ok(),
        "and no service was made beside it"
    );
    assert!(!records_of(&task.id).exists(), "nor any records");
}

#[skuld::test(labels = [UNIT_DIR_EXCLUSIVE], serial = UNIT_DIR_EXCLUSIVE)]
fn windows_passes_task_conformance(#[fixture(scm_group)] _group: &Group) {
    let mgr = support::stock_shim_manager();
    // Every task `run_tasks` makes, with a schedule and without, runs until it is stopped.
    let listener = ConnectBack::listen();
    conformance::run_tasks(&mgr, &|id, schedule| {
        let id = Id::try_from(id).expect("a conformance id is an id");
        task_spec(&id, runs_until_ended(&listener), schedule, User::Root, None)
    });
}
