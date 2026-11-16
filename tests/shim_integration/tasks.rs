//! The shim's task mode on the real Task Scheduler, through the public manager: the shim reads its own
//! task, runs the program and records how the run ended, and `run` reads that record back. A task's
//! program is this binary in a shim fixture mode (`fixture.rs`). The tasks of a service account run a
//! copy of the shim and of this binary that the account can execute (`WorldReadableExe`).

use goetia::manager::{Budget, Exit, ServiceManager as _, State};
use goetia::spec::{Id, User};

use crate::common::{UNIT_DIR_EXCLUSIVE, fixture_command, fixture_command_with_exe, manager};
use crate::supervisor_integration::pid_is_alive;
use crate::support::windows_tasks::{EndsTheRunOnPanic, ServiceAccountHost, task_spec};
use crate::support::{self, ConnectBack, Group, ServiceGuard, kill, scm_group};

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

/// A stop ends the run through Task Scheduler. Until the shim's own stop path waits for the program's exit
/// (goetia#263), the shim ends its tree at once and writes one record, `Stopped`.
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

        mgr.stop(&id, Budget::DEFAULT).expect("stop");
        ends_the_run.defuse();

        let exit = run.join().expect("the run thread").expect("run");
        assert_eq!(exit, STOPPED, "one record, `Stopped`");
        // Gone is a state the kernel reaches as the shim's job closes, so it is waited for.
        for (what, pid) in [("direct child", child), ("grandchild", grandchild)] {
            kill::until(|| !pid_is_alive(&pid));
            assert!(!pid_is_alive(&pid), "the {what} (pid {pid}) is gone");
        }
    });
    assert_eq!(mgr.status(&id).expect("status").common().state, State::Idle);
    mgr.uninstall(&id).expect("uninstall");
}

#[skuld::test(labels = [UNIT_DIR_EXCLUSIVE], serial = UNIT_DIR_EXCLUSIVE)]
fn the_programs_run_as_the_tasks_principal(#[fixture(scm_group)] _group: &Group) {
    let (_, sid) = run_as_local_service("user", &[]);

    assert_eq!(sid, "S-1-5-19", "LocalService's well-known SID");
}

/// The shim runs as an account that is no administrator, and reads its own task through Task Scheduler's
/// COM to find its instance: a read that is refused starts the program all the same and writes no
/// record, so `run` would report the run unrecorded.
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
    mgr.stop(&id, Budget::DEFAULT).expect("stop");
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
