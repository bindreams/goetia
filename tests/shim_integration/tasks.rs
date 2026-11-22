//! The shim's task mode on the real Task Scheduler, through the public manager: the shim reads its own
//! task, runs the program and records how the run ended, and `run` reads that record back. A task's
//! program is this binary in a shim fixture mode (`fixture.rs`). The tasks of a service account run a
//! copy of the shim and of this binary that the account can execute (`WorldReadableExe`).

use windows_sys::Win32::Foundation::ERROR_GEN_FAILURE;

use goetia::manager::{Budget, Exit, ServiceManager as _, State};
use goetia::spec::{Id, User};
use goetia::testing::RemovalMarker;

use crate::common::{LogDir, UNIT_DIR_EXCLUSIVE, fixture_command, fixture_command_with_exe, manager};
use crate::supervisor_integration::pid_is_alive;
use crate::support::windows_tasks::{EndsTheRunOnPanic, ServiceAccountHost, status_once_ended, task_spec};
use crate::support::{self, ConnectBack, Group, ServiceGuard, scm_group, wait};

/// What a run that `Stop` ended exits as: Windows' code for a console process ended by its close event.
const STOPPED: Exit = Exit::Code(0xC000_013A_u32 as i32);

fn task_id() -> Id {
    Id::try_from(support::random_test_id().as_str()).expect("a test id is an id")
}

/// A `run` of a task as `LocalService` whose program is this binary in fixture `mode`, given the port it
/// reports on and then `args`: how the run ended, and what the program reported.
fn run_as_local_service(mode: &str, args: &[&str]) -> (Exit, String) {
    let id = task_id();
    // Declared before the guard, so they go after the task is removed.
    let host = ServiceAccountHost::new(&id);
    let _guard = ServiceGuard::new(id.as_str());
    let mgr = host.manager();
    let reported = ConnectBack::listen();
    let port = reported.port().to_string();
    let args: Vec<&str> = std::iter::once(port.as_str()).chain(args.iter().copied()).collect();
    let command = fixture_command_with_exe(&host.exe.path_str(), mode, &args);
    mgr.install(
        &task_spec(&id, command, None, User::Name("LocalService".to_owned()), None),
        false,
    )
    .expect("install");

    let exit = mgr
        .run(&id, &mut Vec::new())
        .unwrap_or_else(|e| panic!("run: {}", e.report()));
    let said = reported.accept_line("the program to report");
    mgr.uninstall(&id).expect("uninstall");
    (exit, said)
}

#[skuld::test(labels = [UNIT_DIR_EXCLUSIVE], serial = UNIT_DIR_EXCLUSIVE)]
fn a_task_run_by_the_shim_records_its_exit(#[fixture(scm_group)] _group: &Group) {
    let id = task_id();
    let _guard = ServiceGuard::new(id.as_str());
    let mgr = manager();
    let reported = ConnectBack::listen();
    let command = fixture_command("exit", &[&reported.port().to_string(), "7"]);
    mgr.install(&task_spec(&id, command, None, User::Root, None), false)
        .expect("install");

    let exit = mgr
        .run(&id, &mut Vec::new())
        .unwrap_or_else(|e| panic!("run: {}", e.report()));

    assert_eq!(
        exit,
        Exit::Code(7),
        "the program's own exit, read from the shim's record"
    );
    reported.accept("the program to have run");
    mgr.uninstall(&id).expect("uninstall");
}

/// A stop ends the run through Task Scheduler: the close event reaches the program on the shim's console, which
/// ignores it, and Windows ends everything on that console. The shim has recorded `Stopped` by then.
#[skuld::test(labels = [UNIT_DIR_EXCLUSIVE], serial = UNIT_DIR_EXCLUSIVE)]
fn ending_the_instance_records_stopped_and_the_child_tree_is_gone(#[fixture(scm_group)] _group: &Group) {
    let id = task_id();
    let _guard = ServiceGuard::new(id.as_str());
    let mgr = manager();
    let (child_reported, grandchild_reported) = (ConnectBack::listen(), ConnectBack::listen());
    let command = fixture_command(
        "spawn-grandchild",
        &[
            &child_reported.port().to_string(),
            &grandchild_reported.port().to_string(),
        ],
    );
    mgr.install(&task_spec(&id, command, None, User::Root, None), false)
        .expect("install");

    std::thread::scope(|scope| {
        let run = scope.spawn(|| mgr.run(&id, &mut Vec::new()));
        let ends_the_run = EndsTheRunOnPanic::new(&id);
        let child = child_reported.accept_line("the direct child to start and report its pid");
        let grandchild = grandchild_reported.accept_line("the grandchild to start and report its pid");

        mgr.stop(&id, Some(Budget::DEFAULT)).expect("stop");
        ends_the_run.defuse();

        let exit = run.join().expect("the run thread").expect("run");
        assert_eq!(exit, STOPPED, "one record, `Stopped`");
        // The kernel reaches Gone as the shim's job closes: wait for it.
        for (what, pid) in [("direct child", child), ("grandchild", grandchild)] {
            wait::until(|| !pid_is_alive(&pid));
            assert!(!pid_is_alive(&pid), "the {what} (pid {pid}) is gone");
        }
    });
    assert_eq!(mgr.status(&id).expect("status").common().state, State::Idle);
    mgr.uninstall(&id).expect("uninstall");
}

/// A program that handles the close event and exits by itself is let run its own shutdown: goetia sends it
/// nothing and adds no wait of its own. The run's record says it was stopped, the program's own shutdown ran (it
/// left its line in the record file), and `stop` succeeds.
#[skuld::test(labels = [UNIT_DIR_EXCLUSIVE], serial = UNIT_DIR_EXCLUSIVE)]
fn a_program_that_handles_the_close_event_runs_its_own_shutdown(#[fixture(scm_group)] _group: &Group) {
    let id = task_id();
    // Declared before the guard, so the directory goes after the task is removed.
    let logs = LogDir::new();
    let _guard = ServiceGuard::new(id.as_str());
    let mgr = manager();
    let reported = ConnectBack::listen();
    let record = logs.path().join("record.txt");
    let command = fixture_command(
        "catch",
        &[
            &reported.port().to_string(),
            record.to_str().expect("the record's path is UTF-8"),
            "close",
        ],
    );
    mgr.install(&task_spec(&id, command, None, User::Root, None), false)
        .expect("install");

    std::thread::scope(|scope| {
        let run = scope.spawn(|| mgr.run(&id, &mut Vec::new()));
        let ends_the_run = EndsTheRunOnPanic::new(&id);
        reported.accept_line("the program to report in, its handler in place");

        mgr.stop(&id, Some(Budget::DEFAULT)).expect("stop");
        ends_the_run.defuse();

        let exit = run.join().expect("the run thread").expect("run");
        assert_eq!(exit, STOPPED, "the record says a stop was accepted");
    });
    let said = std::fs::read_to_string(&record).unwrap_or_else(|e| panic!("read {}: {e}", record.display()));
    assert!(
        said.contains("daemon: event=CTRL_CLOSE"),
        "the program was sent the close event: {said}"
    );
    assert!(
        said.contains("daemon: exiting by the event"),
        "the program's own shutdown ran: {said}"
    );
    assert_eq!(mgr.status(&id).expect("status").common().state, State::Idle);
    mgr.uninstall(&id).expect("uninstall");
}

#[skuld::test(labels = [UNIT_DIR_EXCLUSIVE], serial = UNIT_DIR_EXCLUSIVE)]
fn the_programs_run_as_the_tasks_principal(#[fixture(scm_group)] _group: &Group) {
    let (_, sid) = run_as_local_service("user", &[]);

    assert_eq!(sid, "S-1-5-19", "LocalService's well-known SID");
}

/// The shim runs as an account that is no administrator, and reads its own task through Task Scheduler's
/// COM to find its instance: a read that is refused starts no program, so `run` would report the run
/// unrecorded.
#[skuld::test(labels = [UNIT_DIR_EXCLUSIVE], serial = UNIT_DIR_EXCLUSIVE)]
fn a_task_reads_its_own_task_unelevated(#[fixture(scm_group)] _group: &Group) {
    let (exit, _) = run_as_local_service("exit", &["3"]);

    assert_eq!(exit, Exit::Code(3), "a recorded run, so the shim found its instance");
}

#[skuld::test(labels = [UNIT_DIR_EXCLUSIVE], serial = UNIT_DIR_EXCLUSIVE)]
fn a_spec_without_cwd_runs_in_system32(#[fixture(scm_group)] _group: &Group) {
    let id = task_id();
    let host = ServiceAccountHost::new(&id);
    let _guard = ServiceGuard::new(id.as_str());
    let mgr = host.manager();
    let reported = ConnectBack::listen();
    let command = fixture_command_with_exe(
        &host.exe.path_str(),
        "cwd-env",
        &[&reported.port().to_string(), "SystemRoot"],
    );
    mgr.install(
        &task_spec(&id, command, None, User::Name("LocalService".to_owned()), None),
        false,
    )
    .expect("install");
    mgr.start(&id, Budget::DEFAULT).expect("start");

    let said = reported.accept_line("the program to report its working directory");

    let cwd = said
        .strip_prefix("cwd=")
        .and_then(|rest| rest.split(';').next())
        .unwrap_or_else(|| panic!("the report is `{said}`"));
    let system32 = format!(r"{}\System32", std::env::var("SystemRoot").expect("SystemRoot"));
    assert!(
        cwd.eq_ignore_ascii_case(&system32),
        "a spec with no `cwd` runs where Task Scheduler starts the shim: {cwd} (want {system32})"
    );
    mgr.stop(&id, Some(Budget::DEFAULT)).expect("stop");
    mgr.uninstall(&id).expect("uninstall");
}

/// The shim's child inherits the priority class Task Scheduler gave the shim: `Priority` 5 is the normal
/// class a service runs at (M11), where Task Scheduler's own default would be below normal.
#[skuld::test(labels = [UNIT_DIR_EXCLUSIVE], serial = UNIT_DIR_EXCLUSIVE)]
fn a_task_runs_at_normal_priority(#[fixture(scm_group)] _group: &Group) {
    let id = task_id();
    let _guard = ServiceGuard::new(id.as_str());
    let mgr = manager();
    let reported = ConnectBack::listen();
    let command = fixture_command("priority", &[&reported.port().to_string()]);
    mgr.install(&task_spec(&id, command, None, User::Root, None), false)
        .expect("install");

    let exit = mgr
        .run(&id, &mut Vec::new())
        .unwrap_or_else(|e| panic!("run: {}", e.report()));

    assert_eq!(exit, Exit::Code(0));
    assert_eq!(
        reported.accept_line("the program to report its priority class"),
        "0x20",
        "NORMAL_PRIORITY_CLASS"
    );
    mgr.uninstall(&id).expect("uninstall");
}

/// An uninstall of a task whose run is in flight stops the run before it deletes the task.
#[skuld::test(labels = [UNIT_DIR_EXCLUSIVE], serial = UNIT_DIR_EXCLUSIVE)]
fn uninstall_with_a_run_in_flight_leaves_no_program(#[fixture(scm_group)] _group: &Group) {
    let id = task_id();
    let _guard = ServiceGuard::new(id.as_str());
    let mgr = manager();
    let (child_reported, grandchild_reported) = (ConnectBack::listen(), ConnectBack::listen());
    let command = fixture_command(
        "spawn-grandchild",
        &[
            &child_reported.port().to_string(),
            &grandchild_reported.port().to_string(),
        ],
    );
    mgr.install(&task_spec(&id, command, None, User::Root, None), false)
        .expect("install");
    mgr.start(&id, Budget::DEFAULT).expect("start");
    let ends_the_run = EndsTheRunOnPanic::new(&id);
    let child = child_reported.accept_line("the direct child to start and report its pid");
    let grandchild = grandchild_reported.accept_line("the grandchild to start and report its pid");

    mgr.uninstall(&id).expect("uninstall");
    ends_the_run.defuse();

    // The kernel reaches Gone as the shim's job closes: wait for it.
    for (what, pid) in [("direct child", child), ("grandchild", grandchild)] {
        wait::until(|| !pid_is_alive(&pid));
        assert!(!pid_is_alive(&pid), "the {what} (pid {pid}) is gone");
    }
    let absent = mgr.status(&id).expect_err("the task was deleted");
    assert!(
        matches!(absent.cause(), goetia::Error::NotInstalled { .. }),
        "{absent:?}"
    );
    let records = support::task_records(id.as_str());
    assert!(!records.exists(), "the run records {} are gone", records.display());
    let marker = records.with_file_name(format!("{id}~removing"));
    assert!(!marker.exists(), "and so is the marker {}", marker.display());
}

/// The shim obeys the removal's marker: its pin is refused, no program starts, and the task's last result
/// is the shim failure. Started through Task Scheduler's `Run`, since a goetia verb is refused by the marker
/// first.
#[skuld::test(labels = [UNIT_DIR_EXCLUSIVE], serial = UNIT_DIR_EXCLUSIVE)]
fn the_real_shim_obeys_the_marker(#[fixture(scm_group)] _group: &Group) {
    let id = task_id();
    let _guard = ServiceGuard::new(id.as_str());
    let mgr = manager();
    let reported = ConnectBack::listen();
    let command = fixture_command("exit", &[&reported.port().to_string(), "7"]);
    mgr.install(&task_spec(&id, command, None, User::Root, None), false)
        .expect("install");
    // The install makes the default log, empty: the refused shim appends nothing to it.
    let default_log = support::program_data()
        .join("Goetia")
        .join("logs")
        .join(format!("{id}.log"));
    assert_eq!(
        std::fs::metadata(&default_log).expect("the install made the log").len(),
        0
    );
    let marker = RemovalMarker::place(&id).unwrap_or_else(|e| panic!("place the marker: {}", e.report()));

    goetia::testing::run_task(&id).unwrap_or_else(|e| panic!("run: {}", e.report()));

    let status = status_once_ended(&mgr, &id);
    assert!(status.last_run.is_some(), "the task has not run");
    assert_eq!(
        status.last_result,
        Some(Exit::Code(ERROR_GEN_FAILURE as i32)),
        "the shim's failure, not the program's 7"
    );
    assert_eq!(
        std::fs::metadata(&default_log).expect("the log is still there").len(),
        0,
        "the refused shim wrote to the default log"
    );
    assert!(!reported.connected_yet(), "the program ran under a marker");
    drop(marker);
    mgr.uninstall(&id).expect("uninstall");
}
