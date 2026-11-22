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

/// Windows' code for a console process ended by its close event, which is what a run that a stop ended reads as: its
/// record, `Exited` with it when the event ended the program, `Stopped` when the shim never saw the program exit.
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

/// A stop of a program that has no handler for the close event: the event ends it, newest process first, and the shim
/// hears it only after the program has ended (measured on Windows Server 2025, build 26100). The shim's waiter had
/// seen the exit, so the run's record is the program's own death by the event, `Exited(0xC000013A)`, not `Stopped`.
#[skuld::test(labels = [UNIT_DIR_EXCLUSIVE], serial = UNIT_DIR_EXCLUSIVE)]
fn a_stop_ends_a_program_with_no_handler_and_the_run_records_its_death_by_the_event(
    #[fixture(scm_group)] _group: &Group,
) {
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
        assert_eq!(
            exit, STOPPED,
            "one record, `Exited(0xC000013A)`: the program's own death by the event"
        );
        // The kernel reaches Gone as the shim's job closes: wait for it.
        for (what, pid) in [("direct child", child), ("grandchild", grandchild)] {
            wait::until(|| !pid_is_alive(&pid));
            assert!(!pid_is_alive(&pid), "the {what} (pid {pid}) is gone");
        }
    });
    assert_eq!(mgr.status(&id).expect("status").common().state, State::Idle);
    mgr.uninstall(&id).expect("uninstall");
}

/// What stopping one run of a task left, from [`stop_a_run`].
struct StoppedRun {
    /// What `run` answered.
    exit: Result<Exit, goetia::Error>,
    /// The line the program reported when it was in place: its pid.
    pid: String,
    /// What the program wrote of its own in its record file.
    said: String,
    /// The records directory, the shim's log and the program's record file as the run left them, for a message.
    left: String,
}

/// A task whose program is this binary in fixture `mode`, given the port it reports on and then the arguments `args`
/// names from the program's record file and the task's run records, `run` on a thread of its own, and `stop` once the
/// program has reported in. The task is removed, and what the test made is cleaned up, when this returns.
fn stop_a_run(mode: &str, args: impl FnOnce(&std::path::Path, &std::path::Path) -> Vec<String>) -> StoppedRun {
    let id = task_id();
    // Declared before the guard, so the directory goes after the task is removed.
    let logs = LogDir::new();
    let _guard = ServiceGuard::new(id.as_str());
    let mgr = manager();
    let reported = ConnectBack::listen();
    let record = logs.path().join("record.txt");
    let shim_log = logs.path().join("task.log");
    let records = support::task_records(id.as_str());
    let port = reported.port().to_string();
    let rest = args(&record, &records);
    let args: Vec<&str> = std::iter::once(port.as_str())
        .chain(rest.iter().map(String::as_str))
        .collect();
    mgr.install(
        &task_spec(&id, fixture_command(mode, &args), None, User::Root, Some(&shim_log)),
        false,
    )
    .expect("install");

    let (exit, pid) = std::thread::scope(|scope| {
        let run = scope.spawn(|| mgr.run(&id, &mut Vec::new()));
        let ends_the_run = EndsTheRunOnPanic::new(&id);
        let pid = reported.accept_line("the program to report in, its handler in place");

        mgr.stop(&id, Some(Budget::DEFAULT)).expect("stop");
        ends_the_run.defuse();
        (run.join().expect("the run thread"), pid)
    });
    let said = std::fs::read_to_string(&record).unwrap_or_default();
    let left = what_the_run_left(&records, &shim_log, &record);
    mgr.uninstall(&id).expect("uninstall");
    StoppedRun { exit, pid, said, left }
}

/// A program that handles the close event runs its own shutdown, and the record is its exit: it ended before the shim
/// heard the stop, so the shim had its exit status. `stop` succeeds, `run` answers the program's code, and the
/// program's shutdown line is in its record file.
#[skuld::test(labels = [UNIT_DIR_EXCLUSIVE], serial = UNIT_DIR_EXCLUSIVE)]
fn a_program_that_handles_the_close_event_and_exits_has_its_own_exit_recorded(#[fixture(scm_group)] _group: &Group) {
    let stopped = stop_a_run("catch-close", |record, _| {
        vec![
            record.to_str().expect("the record's path is UTF-8").to_owned(),
            "7".to_owned(),
        ]
    });

    let left = &stopped.left;
    let exit = stopped.exit.unwrap_or_else(|e| panic!("run: {}\n{left}", e.report()));
    assert_eq!(exit, Exit::Code(7), "the program's own code, not the stop's\n{left}");
    assert!(
        stopped.said.contains("daemon: event=CTRL_CLOSE"),
        "the program was sent the close event: {}\n{left}",
        stopped.said
    );
    assert!(
        stopped.said.contains("daemon: exiting by the event"),
        "the program's own shutdown ran: {}\n{left}",
        stopped.said
    );
}

/// A program that left the console is sent nothing, so the shim hears the stop with the program alive: it records
/// `Stopped` and waits for the program's own exit. What the shim guarantees, and the program's own line proves, is that
/// the record on disk while the program was alive said `stopped`. How the run ends then is Windows' to decide, about a
/// second after the stop: the program exits by itself in time and `run` answers its code (the record is its exit), or
/// Windows ends the shim first and `run` answers `0xC000013A` (the record stays `stopped`). Either is accepted, and
/// which it was is printed. The order of the two records is pinned by the shim's own unit tests
/// (`task_tests::a_program_that_exits_after_the_stop_was_recorded_has_its_exit_recorded_with_the_pin_held_meanwhile`).
#[skuld::test(labels = [UNIT_DIR_EXCLUSIVE], serial = UNIT_DIR_EXCLUSIVE)]
fn a_program_with_no_console_has_stopped_on_disk_while_alive_and_ends_as_its_exit_or_as_stopped(
    #[fixture(scm_group)] _group: &Group,
) {
    let stopped = stop_a_run("detach", |record, records| {
        vec![
            record.to_str().expect("the record's path is UTF-8").to_owned(),
            records.to_str().expect("the records' path is UTF-8").to_owned(),
            "9".to_owned(),
        ]
    });

    let left = &stopped.left;
    let exit = stopped.exit.unwrap_or_else(|e| panic!("run: {}
{left}", e.report()));
    assert!(
        stopped.said.contains("daemon: record=") && stopped.said.contains("stopped"),
        "the record the program saw, as the stop was accepted, was `stopped`: {}
{left}",
        stopped.said
    );
    assert!(
        exit == Exit::Code(9) || exit == STOPPED,
        "the program's own code or the stop's, not {exit:?}
{left}"
    );
    println!(
        "measured: {}",
        if exit == STOPPED {
            "Windows ended the shim first: the record stayed `stopped`"
        } else {
            "the program exited in time: the record is its own exit"
        }
    );
}

/// The same program that never exits: Windows ends the shim about a second after the stop and the job closes over the
/// program, so the record is the `Stopped` the shim wrote first, `run` answers the stop's code, and nothing of the
/// tree is left.
#[skuld::test(labels = [UNIT_DIR_EXCLUSIVE], serial = UNIT_DIR_EXCLUSIVE)]
fn a_program_with_no_console_that_never_exits_is_recorded_stopped_and_ended_with_the_shim(
    #[fixture(scm_group)] _group: &Group,
) {
    let stopped = stop_a_run("detach", |record, records| {
        vec![
            record.to_str().expect("the record's path is UTF-8").to_owned(),
            records.to_str().expect("the records' path is UTF-8").to_owned(),
            "never".to_owned(),
        ]
    });

    let left = &stopped.left;
    let exit = stopped.exit.unwrap_or_else(|e| panic!("run: {}\n{left}", e.report()));
    assert_eq!(exit, STOPPED, "the record stays `stopped`\n{left}");
    assert!(
        stopped.said.contains("stopped"),
        "the record the program saw was `stopped`: {}\n{left}",
        stopped.said
    );
    // The kernel reaches Gone as the shim's job closes: wait for it.
    wait::until(|| !pid_is_alive(&stopped.pid));
    assert!(!pid_is_alive(&stopped.pid), "the program (pid {}) is gone", stopped.pid);
}

/// A console program that never leaves its handler for the close event: the shim's handler is never entered, Windows
/// ends the shim about a second after the stop and the job closes over the program, and nothing is recorded. `stop`
/// succeeds, `run` says the run left no record and why, and nothing of the tree is left.
#[skuld::test(labels = [UNIT_DIR_EXCLUSIVE], serial = UNIT_DIR_EXCLUSIVE)]
fn a_console_program_that_never_leaves_its_handler_leaves_no_record_and_is_ended_with_the_shim(
    #[fixture(scm_group)] _group: &Group,
) {
    let stopped = stop_a_run("catch-hang", |record, _| {
        vec![record.to_str().expect("the record's path is UTF-8").to_owned()]
    });

    let left = &stopped.left;
    let failed = stopped
        .exit
        .expect_err(&format!("the run was recorded, but nothing could record it\n{left}"));
    assert!(
        matches!(failed.cause(), goetia::Error::RunUnrecorded { .. }),
        "{}\n{left}",
        failed.report()
    );
    assert!(
        failed
            .report()
            .to_string()
            .contains("its program was still running about a second after a stop"),
        "{}\n{left}",
        failed.report()
    );
    assert!(
        stopped.said.contains("daemon: event=CTRL_CLOSE"),
        "the program was sent the close event: {}\n{left}",
        stopped.said
    );
    // The kernel reaches Gone as the shim's job closes: wait for it.
    wait::until(|| !pid_is_alive(&stopped.pid));
    assert!(!pid_is_alive(&stopped.pid), "the program (pid {}) is gone", stopped.pid);
}

/// What a run left that says why it ended as it did: the records directory (each record's text), the shim's
/// own log (the task's `logs`: the program's output and the shim's failures) and the program's record file.
fn what_the_run_left(
    records: &std::path::Path,
    shim_log: &std::path::Path,
    program_record: &std::path::Path,
) -> String {
    let mut said = format!("the records in {}:", records.display());
    match std::fs::read_dir(records) {
        Ok(entries) => {
            for entry in entries.flatten() {
                let text = std::fs::read_to_string(entry.path()).unwrap_or_default();
                said.push_str(&format!("\n  {}: {text}", entry.file_name().to_string_lossy()));
            }
        }
        Err(e) => said.push_str(&format!(" cannot be listed: {e}")),
    }
    for (what, path) in [("the shim's log", shim_log), ("the program's record", program_record)] {
        said.push_str(&format!(
            "\n{what} {}:\n{}",
            path.display(),
            std::fs::read_to_string(path).unwrap_or_else(|e| format!("<{e}>"))
        ));
    }
    said
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
