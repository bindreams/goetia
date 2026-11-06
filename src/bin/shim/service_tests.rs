//! What `launch` and `run_daemon` do, which is the whole of this module that can be tested without
//! a real SCM, over the `Fake`'s stated replies. `launch` makes the thread that will wait on the
//! daemon before the daemon, so the OS refusing that thread is a start failure with nothing
//! launched rather than an orphaned daemon nothing waits on. `run_daemon` is one pass: it reports
//! running after the spawn, never after a failed launch, and turns the daemon's end into what SCM
//! is told.

use std::path::Path;

use cosca::Containment;
use goetia::backend::scm::exit_status::{
    ERROR_CAN_NOT_COMPLETE, ERROR_GEN_FAILURE, ERROR_SERVICE_SPECIFIC_ERROR, Reported,
};
use goetia::spec::{Kind, Restart};
use skuld::temp_dir;
use windows_sys::Win32::Foundation::{
    ERROR_ACCESS_DENIED, ERROR_CALL_NOT_IMPLEMENTED, ERROR_FILE_NOT_FOUND, ERROR_PATH_NOT_FOUND, NO_ERROR,
};
use windows_sys::Win32::System::Services::{
    SERVICE_CONTROL_INTERROGATE, SERVICE_CONTROL_PAUSE, SERVICE_CONTROL_STOP, SERVICE_STOP_PENDING, SERVICE_STOPPED,
};

use super::*;
use crate::os::fake::{FakeGuard, FakeProcess, KillReply, NO_THREAD, Op, WAIT_FAILED, WaitReply, WaiterEnd};
use crate::test_support::{
    assert_failures, assert_one_failure, bus, daemon_spec, fallback_refused_failure, tree_refused_failure,
};

const ID: &str = "goetia-shim-launch-test";

/// [`super::run_daemon`] for the tests that do not look at the `STOP_PENDING` report.
fn run_daemon(spec: &DaemonSpec, stop_bus: &Arc<StopBus>, on_running: impl FnOnce()) -> Reported {
    super::run_daemon(spec, stop_bus, ID, on_running, || {})
}

/// A `type: simple` daemon. Nothing runs it: the `Fake` states what happens to it.
///
/// `logs` is in `dir`, the test's temporary directory, so the test can look at the file:
/// `build_command` opens the daemon's log for real. Unset, it would be in the `FakeGuard`'s
/// temporary directory, never `%ProgramData%`. Failure reporting goes to the `Fake`'s channels,
/// which never touch a file.
fn spec(dir: &Path) -> DaemonSpec {
    let command = [
        "powershell",
        "-NoProfile",
        "-NonInteractive",
        "-Command",
        "Start-Sleep -Seconds 300",
    ];
    DaemonSpec {
        name: "goetia-shim launch test".to_string(),
        restart: Restart::Always,
        logs: Some(dir.join("daemon.log")),
        kind: Kind::Simple,
        ..daemon_spec::spec(ID, command.map(str::to_string).to_vec())
    }
}

/// What a spawn of a bare binary name not found on `PATH` fails with: `Io(NotFound)` and no OS
/// error code (`os_tests` pins it against `Real`, with the absolute-path case, which carries
/// `ERROR_FILE_NOT_FOUND` from `CreateProcessW`).
fn not_found() -> cosca::error::Error {
    cosca::error::Error::Io(std::io::Error::from(std::io::ErrorKind::NotFound))
}

/// The line logged when no thread can be made to wait on the daemon.
fn no_thread_failure() -> String {
    format!(
        r#"make the thread that waits on ["powershell", "-NoProfile", "-NonInteractive", "-Command", "Start-Sleep -Seconds 300"]: {NO_THREAD}; it was not spawned, so nothing of this daemon is running"#
    )
}

/// The line logged when the spawn is refused with [`not_found`].
fn spawn_failure() -> String {
    format!(
        r#"spawn ["powershell", "-NoProfile", "-NonInteractive", "-Command", "Start-Sleep -Seconds 300"]: {}"#,
        not_found()
    )
}

/// The line logged when the daemon is not job-contained, whatever the kill then does.
const NOT_JOB_CONTAINED: &str = r#"the daemon ["powershell", "-NoProfile", "-NonInteractive", "-Command", "Start-Sleep -Seconds 300"] was contained by process-tree walk, not a job object: `type: simple` needs a job object to terminate its whole tree. The daemon ran from its launch, and no kill of it shows that its descendants are gone, so it is reported as possibly still running"#;

/// The line logged when the daemon's tree cannot be terminated after the daemon ended.
const EXITED_TREE_UNKILLED: &str = "kill_tree after the daemon ended: the kill was refused (fake); its descendants may still be running — SCM is told the service stopped with Win32 exit code 0, so that it does not start a second copy beside it, and with service-specific exit code 1003 as the record of this failure";

#[skuld::test]
fn no_daemon_is_spawned_when_no_thread_can_be_made_to_wait_on_it(#[fixture(temp_dir)] dir: &Path) {
    let fake = FakeGuard::new();
    fake.will_refuse_thread();

    // Reported, not panicked: a refused thread here surfaces as an `io::Result`, not a
    // `Scope::spawn` panic that would take the whole service process down with the daemon already
    // running.
    let launched = launch(&spec(dir), &bus(&fake), ID);

    assert_eq!(
        launched.as_ref().err(),
        Some(&Reported::shim_failure(None)),
        "launch reported a daemon that nothing could wait on"
    );
    assert_one_failure(&fake.take_failures(), ID, &no_thread_failure());
    assert_eq!(
        fake.calls(Op::Spawn),
        0,
        "the daemon was spawned before the thread that would have waited on it was made — \
         a failure there leaves it running with nothing waiting on it"
    );
    // A witness from the filesystem beside the `Fake`'s count: `build_command` opens this file with
    // `create(true)` before the spawn is reached, so its existence shows `launch` got past the
    // waiter. The control test below asserts it does exist when the spawn is reached, so this is
    // not vacuous.
    assert!(
        !dir.join("daemon.log").exists(),
        "the daemon's log file was created, so launch reached build_command — it was preparing to \
         spawn before the thread that would have waited on the daemon was made"
    );
}

#[skuld::test]
fn a_daemon_is_spawned_once_the_thread_that_waits_on_it_exists(#[fixture(temp_dir)] dir: &Path) {
    // The control for the assertions above: `launch` does reach the spawn, and both things they
    // assert against — the count and the daemon's log file — do move when the thread it needs
    // first can be made.
    let fake = FakeGuard::new();
    let daemon = FakeProcess::new();
    fake.will_make_thread();
    fake.will_spawn(daemon.clone());

    let launched = launch(&spec(dir), &bus(&fake), ID);

    assert!(launched.is_ok(), "launch failed with every thread available");
    assert_eq!(
        fake.take_failures(),
        Vec::<String>::new(),
        "a launch that succeeded reported a failure"
    );
    assert_eq!(
        fake.calls(Op::Spawn),
        1,
        "launch returned a daemon without spawning one"
    );
    assert!(
        dir.join("daemon.log").exists(),
        "no daemon log file even though the spawn was reached — the assertion above that there is \
         none when it is not reached would then hold for the wrong reason"
    );
    assert!(!daemon.dropped(), "the daemon was dropped while launch still owns it");

    // The waiter was never handed this child (no `wait_for_child_or_stop` here), so dropping it
    // drops the last `Arc`, and with it the daemon.
    drop(launched);
    // The un-handed waiter ends on drop; consume its end.
    assert_eq!(fake.waiter_end(0), WaiterEnd::Returned);
    assert!(daemon.dropped(), "the daemon outlived the last reference to it");
}

#[skuld::test]
fn a_daemon_that_cannot_be_spawned_is_logged_and_nothing_is_left_running(#[fixture(temp_dir)] dir: &Path) {
    let fake = FakeGuard::new();
    fake.will_make_thread();
    fake.will_refuse_spawn(not_found());

    let launched = launch(&spec(dir), &bus(&fake), ID);

    assert_eq!(
        launched.as_ref().err(),
        Some(&Reported::shim_failure(Some(ERROR_FILE_NOT_FOUND as i32))),
        "launch did not report the missing binary to SCM as ERROR_FILE_NOT_FOUND"
    );
    assert_one_failure(&fake.take_failures(), ID, &spawn_failure());
    // The un-handed waiter ends on drop; consume its end.
    assert_eq!(fake.waiter_end(0), WaiterEnd::Returned);
}

#[skuld::test]
fn the_os_code_of_a_refused_spawn_is_what_launch_reports(#[fixture(temp_dir)] dir: &Path) {
    const ACCESS_DENIED: i32 = ERROR_ACCESS_DENIED as i32;
    let fake = FakeGuard::new();
    fake.will_make_thread();
    fake.will_refuse_spawn(cosca::error::Error::Io(std::io::Error::from_raw_os_error(
        ACCESS_DENIED,
    )));

    let launched = launch(&spec(dir), &bus(&fake), ID);

    assert_eq!(
        launched.as_ref().err(),
        Some(&Reported::shim_failure(Some(ACCESS_DENIED)))
    );
    // The refusal's text is not the point here; consume the one failure line and the waiter.
    assert_eq!(fake.take_failures().len(), 1);
    assert_eq!(fake.waiter_end(0), WaiterEnd::Returned);
}

#[skuld::test]
fn a_log_file_that_cannot_be_opened_is_logged_and_the_daemon_still_runs(#[fixture(temp_dir)] dir: &Path) {
    let fake = FakeGuard::new();
    let daemon = FakeProcess::new();
    fake.will_make_thread();
    fake.will_spawn(daemon.clone());
    // A directory cannot be created under a regular file.
    let file = dir.join("a-file");
    std::fs::write(&file, "x").expect("a regular file to use as a parent");
    let unopenable = file.join("daemon.log");
    let unopenable_daemon = DaemonSpec {
        logs: Some(unopenable.clone()),
        ..spec(dir)
    };

    let launched = launch(&unopenable_daemon, &bus(&fake), ID);

    assert!(
        launched.is_ok(),
        "an unopenable log file stopped the daemon from running"
    );
    let refusal = open_append(&unopenable).expect_err("the path cannot be opened");
    assert_one_failure(
        &fake.take_failures(),
        ID,
        &format!(
            "open log file {}: {refusal} (this daemon's output will not be captured; it is still being run)",
            unopenable.display()
        ),
    );
    assert_eq!(fake.calls(Op::Spawn), 1, "the log failure stopped the daemon after all");
    assert!(!daemon.dropped());

    // The un-handed waiter ends on drop; consume its end.
    drop(launched);
    assert_eq!(fake.waiter_end(0), WaiterEnd::Returned);
    assert!(daemon.dropped());
}

// run_daemon ==========================================================================================================

#[skuld::test]
fn no_running_report_when_no_thread_can_be_made(#[fixture(temp_dir)] dir: &Path) {
    let fake = FakeGuard::new();
    fake.will_refuse_thread();
    let mut running_reports = 0;

    let reported = run_daemon(&spec(dir), &bus(&fake), || running_reports += 1);

    assert_eq!(
        running_reports, 0,
        "RUNNING was reported for a daemon nothing could wait on"
    );
    assert_eq!(reported, Reported::shim_failure(None));
    assert_one_failure(&fake.take_failures(), ID, &no_thread_failure());
    assert_eq!(fake.calls(Op::Spawn), 0, "a daemon was spawned");
}

#[skuld::test]
fn no_running_report_when_the_spawn_fails(#[fixture(temp_dir)] dir: &Path) {
    let fake = FakeGuard::new();
    fake.will_make_thread();
    fake.will_refuse_spawn(not_found());
    let mut running_reports = 0;

    let reported = run_daemon(&spec(dir), &bus(&fake), || running_reports += 1);

    assert_eq!(
        running_reports, 0,
        "RUNNING was reported for a daemon that never started"
    );
    assert_eq!(reported, Reported::shim_failure(Some(ERROR_FILE_NOT_FOUND as i32)));
    assert_one_failure(&fake.take_failures(), ID, &spawn_failure());
    assert_eq!(fake.waiter_end(0), WaiterEnd::Returned);
}

#[skuld::test]
fn running_is_reported_once_and_after_the_spawn(#[fixture(temp_dir)] dir: &Path) {
    let fake = FakeGuard::new();
    let stop_bus = bus(&fake);
    fake.will_make_thread();
    fake.will_spawn(FakeProcess::running());
    let mut running_reports = Vec::new();

    // The stop ends the pass once the report is made; the daemon runs until then.
    let reported = run_daemon(&spec(dir), &stop_bus, || {
        running_reports.push(fake.calls(Op::Spawn));
        stop_bus.request_stop();
    });

    assert_eq!(
        running_reports,
        [1],
        "RUNNING must be reported exactly once, with one spawn already made"
    );
    assert_eq!(
        reported,
        Reported::Clean,
        "a commanded stop that killed the daemon is clean"
    );
    assert_eq!(fake.waiter_end(0), WaiterEnd::Returned);
}

#[skuld::test]
fn a_daemon_that_exits_nonzero_is_one_pass(#[fixture(temp_dir)] dir: &Path) {
    let fake = FakeGuard::new();
    fake.will_make_thread();
    fake.will_spawn(FakeProcess::new().exited(7).kill_tree(KillReply::Ends));
    // The shim ignores the restart policy: no respawn.
    let daemon = DaemonSpec {
        restart: Restart::OnFailure,
        restart_delay: None,
        ..spec(dir)
    };

    let reported = run_daemon(&daemon, &bus(&fake), || {});

    assert_eq!(reported, Reported::DaemonExit(7));
    assert_eq!(fake.calls(Op::Spawn), 1, "the daemon was spawned again");
    assert_eq!(fake.waiter_end(0), WaiterEnd::Returned);
}

#[skuld::test]
fn a_daemon_that_exits_zero_is_clean(#[fixture(temp_dir)] dir: &Path) {
    let fake = FakeGuard::new();
    fake.will_make_thread();
    fake.will_spawn(FakeProcess::new().exited(0).kill_tree(KillReply::Ends));

    let reported = run_daemon(&spec(dir), &bus(&fake), || {});

    assert_eq!(reported, Reported::Clean);
    assert_eq!(fake.waiter_end(0), WaiterEnd::Returned);
}

#[skuld::test]
fn an_unkillable_daemon_reports_the_incomplete_stop_failure(#[fixture(temp_dir)] dir: &Path) {
    let fake = FakeGuard::new();
    let stop_bus = bus(&fake);
    // Both kills are refused, so the daemon outlives `run_daemon`: the test ends it, which is what
    // lets the detached waiter thread be consumed.
    let daemon = FakeProcess::new()
        .wait(WaitReply::Blocks)
        .kill_tree(KillReply::Refuses)
        .kill(KillReply::Refuses);
    fake.will_make_thread();
    fake.will_spawn(daemon.clone());

    let reported = run_daemon(&spec(dir), &stop_bus, || {
        stop_bus.request_stop();
    });

    assert_eq!(reported, Reported::incomplete_stop());
    assert!(
        !daemon.ended() && !daemon.dropped(),
        "the daemon was ended or dropped though neither kill worked"
    );
    daemon.end(1);
    assert_eq!(fake.waiter_end(0), WaiterEnd::Returned);
    assert_failures(
        &fake.take_failures(),
        ID,
        &[tree_refused_failure(), fallback_refused_failure()],
    );
}

/// The pass `WaitReply::Fails` breaks: the waiter's wait failed, so the daemon is torn down and, once
/// confirmed gone, the shim reports the failure with the wait's own OS error.
#[skuld::test]
fn a_failed_wait_is_a_shim_failure_with_the_os_error_and_the_daemon_is_dead(#[fixture(temp_dir)] dir: &Path) {
    let fake = FakeGuard::new();
    let daemon = FakeProcess::new()
        .wait(WaitReply::Fails(Some(5)))
        .kill_tree(KillReply::Ends);
    fake.will_make_thread();
    fake.will_spawn(daemon.clone());

    let reported = run_daemon(&spec(dir), &bus(&fake), || {});

    assert_eq!(reported, Reported::shim_failure(Some(5)));
    assert!(
        daemon.killed(),
        "a restartable failure was reported over a daemon still running"
    );
    assert_one_failure(
        &fake.take_failures(),
        ID,
        &format!("background wait on child: {}", std::io::Error::from_raw_os_error(5)),
    );
    assert_eq!(fake.waiter_end(0), WaiterEnd::Returned);
}

#[skuld::test]
fn a_failed_wait_with_no_os_error_is_the_generic_shim_failure(#[fixture(temp_dir)] dir: &Path) {
    let fake = FakeGuard::new();
    fake.will_make_thread();
    fake.will_spawn(
        FakeProcess::new()
            .wait(WaitReply::Fails(None))
            .kill_tree(KillReply::Ends),
    );

    let reported = run_daemon(&spec(dir), &bus(&fake), || {});

    assert_eq!(reported, Reported::ShimFailure(ERROR_GEN_FAILURE));
    assert_one_failure(
        &fake.take_failures(),
        ID,
        &format!("background wait on child: {WAIT_FAILED}"),
    );
    assert_eq!(fake.waiter_end(0), WaiterEnd::Returned);
}

/// A daemon whose tree could not be killed must not be reported as a failure SCM restarts: the
/// restart would start a second copy beside it. The exit is decided, so the bus does not accept a
/// later stop either.
#[skuld::test]
fn a_failed_wait_over_an_unkillable_daemon_is_the_incomplete_stop_record(#[fixture(temp_dir)] dir: &Path) {
    let fake = FakeGuard::new();
    let stop_bus = bus(&fake);
    fake.will_make_thread();
    fake.will_spawn(
        FakeProcess::new()
            .wait(WaitReply::Fails(None))
            .kill_tree(KillReply::Refuses)
            .kill(KillReply::Refuses),
    );

    let reported = run_daemon(&spec(dir), &stop_bus, || {});

    assert_eq!(reported, Reported::incomplete_stop());
    assert!(
        !stop_bus.request_stop(),
        "the bus accepted a stop over a daemon that may still be running"
    );
    assert_eq!(fake.waiter_end(0), WaiterEnd::Returned);
    assert_failures(
        &fake.take_failures(),
        ID,
        &[
            format!("background wait on child: {WAIT_FAILED}"),
            tree_refused_failure(),
            fallback_refused_failure(),
        ],
    );
}

/// The daemon has failed, so SCM will restart it: its tree is terminated first, so that the restart
/// never runs beside a tree nobody told to end. When the tree cannot be terminated the restart
/// must not happen.
#[skuld::test]
fn a_failing_daemon_whose_tree_cannot_be_terminated_is_the_incomplete_stop_record(#[fixture(temp_dir)] dir: &Path) {
    let fake = FakeGuard::new();
    let stop_bus = bus(&fake);
    fake.will_make_thread();
    fake.will_spawn(FakeProcess::new().exited(7).kill_tree(KillReply::Refuses));

    let reported = run_daemon(&spec(dir), &stop_bus, || {});

    assert_eq!(reported, Reported::incomplete_stop());
    assert!(
        !stop_bus.request_stop(),
        "the bus accepted a stop over a daemon whose tree may still be running"
    );
    assert_one_failure(&fake.take_failures(), ID, EXITED_TREE_UNKILLED);
    assert_eq!(fake.waiter_end(0), WaiterEnd::Returned);
}

/// `STOPPED` is never reported over a tree that may still be running, whatever ended the daemon: a
/// `start` that lands before the shim exits would run a second copy beside it. When the tree cannot
/// be terminated, the record is the incomplete stop even for a clean exit.
#[skuld::test]
fn a_clean_exit_whose_tree_cannot_be_terminated_is_the_incomplete_stop_record(#[fixture(temp_dir)] dir: &Path) {
    let fake = FakeGuard::new();
    fake.will_make_thread();
    fake.will_spawn(FakeProcess::new().exited(0).kill_tree(KillReply::Refuses));

    let reported = run_daemon(&spec(dir), &bus(&fake), || {});

    assert_eq!(reported, Reported::incomplete_stop());
    assert_one_failure(&fake.take_failures(), ID, EXITED_TREE_UNKILLED);
    assert_eq!(fake.waiter_end(0), WaiterEnd::Returned);
}

/// A clean exit whose tree is terminated is reported as it was.
#[skuld::test]
fn a_clean_exit_whose_tree_is_terminated_is_clean(#[fixture(temp_dir)] dir: &Path) {
    let fake = FakeGuard::new();
    fake.will_make_thread();
    fake.will_spawn(FakeProcess::new().exited(0).kill_tree(KillReply::Ends));

    let reported = run_daemon(&spec(dir), &bus(&fake), || {});

    assert_eq!(reported, Reported::Clean);
    assert_eq!(fake.take_failures(), Vec::<String>::new());
    assert_eq!(fake.waiter_end(0), WaiterEnd::Returned);
}

// STOP_PENDING --------------------------------------------------------------------------------------------------------

/// `run_daemon` with both reports recorded: how many tree kills had been attempted at each
/// `STOP_PENDING` report, so a test can tell whether teardown had begun.
fn run_recording_stopping(
    fake: &FakeGuard,
    daemon: &DaemonSpec,
    stop_bus: &Arc<StopBus>,
    on_running: impl FnOnce(),
) -> (Reported, Vec<usize>) {
    let mut stopping = Vec::new();
    let reported = super::run_daemon(daemon, stop_bus, ID, on_running, || {
        stopping.push(fake.calls(Op::KillTree));
    });
    (reported, stopping)
}

/// The status held while the shim tears the daemon down. SCM refuses a second `STOP` and a `START`
/// while a host holds it (measured), which is what keeps `start` from reading the service as up.
#[skuld::test]
fn the_stopping_status_accepts_no_control_and_records_no_exit() {
    let status = stopping_status();

    assert_eq!(status.dwCurrentState, SERVICE_STOP_PENDING);
    assert_eq!(status.dwControlsAccepted, 0);
    assert_eq!((status.dwWin32ExitCode, status.dwServiceSpecificExitCode), (0, 0));
    assert_eq!((status.dwCheckPoint, status.dwWaitHint), (1, 10_000));
}

#[skuld::test]
fn a_commanded_stop_reports_stopping_once_while_the_daemon_is_still_alive(#[fixture(temp_dir)] dir: &Path) {
    let fake = FakeGuard::new();
    let stop_bus = bus(&fake);
    let daemon = FakeProcess::running();
    fake.will_make_thread();
    fake.will_spawn(daemon.clone());
    let mut alive_when_stopping = Vec::new();

    let reported = super::run_daemon(
        &spec(dir),
        &stop_bus,
        ID,
        || {
            stop_bus.request_stop();
        },
        || {
            alive_when_stopping.push((fake.calls(Op::KillTree), !daemon.ended()));
        },
    );

    assert_eq!(reported, Reported::Clean);
    assert_eq!(
        alive_when_stopping,
        [(0, true)],
        "STOP_PENDING must be reported once, before the teardown begins"
    );
    assert_eq!(fake.calls(Op::KillTree), 1);
    assert_eq!(fake.waiter_end(0), WaiterEnd::Returned);
}

#[skuld::test]
fn a_daemon_that_exits_reports_stopping_before_its_tree_is_terminated(#[fixture(temp_dir)] dir: &Path) {
    let fake = FakeGuard::new();
    fake.will_make_thread();
    fake.will_spawn(FakeProcess::new().exited(7).kill_tree(KillReply::Ends));

    let (reported, stopping) = run_recording_stopping(&fake, &spec(dir), &bus(&fake), || {});

    assert_eq!(reported, Reported::DaemonExit(7));
    assert_eq!(
        stopping,
        [0],
        "STOP_PENDING must be reported once, before any tree kill"
    );
    assert_eq!(fake.calls(Op::KillTree), 1);
    assert_eq!(fake.waiter_end(0), WaiterEnd::Returned);
}

#[skuld::test]
fn a_daemon_that_exits_cleanly_reports_stopping_before_its_tree_is_terminated(#[fixture(temp_dir)] dir: &Path) {
    let fake = FakeGuard::new();
    fake.will_make_thread();
    fake.will_spawn(FakeProcess::new().exited(0).kill_tree(KillReply::Ends));

    let (reported, stopping) = run_recording_stopping(&fake, &spec(dir), &bus(&fake), || {});

    assert_eq!(reported, Reported::Clean);
    assert_eq!(stopping, [0]);
    assert_eq!(fake.waiter_end(0), WaiterEnd::Returned);
}

#[skuld::test]
fn a_failed_wait_reports_stopping_before_the_daemon_is_killed(#[fixture(temp_dir)] dir: &Path) {
    let fake = FakeGuard::new();
    fake.will_make_thread();
    fake.will_spawn(
        FakeProcess::new()
            .wait(WaitReply::Fails(Some(5)))
            .kill_tree(KillReply::Ends),
    );

    let (reported, stopping) = run_recording_stopping(&fake, &spec(dir), &bus(&fake), || {});

    assert_eq!(reported, Reported::shim_failure(Some(5)));
    assert_eq!(stopping, [0]);
    assert_one_failure(
        &fake.take_failures(),
        ID,
        &format!("background wait on child: {}", std::io::Error::from_raw_os_error(5)),
    );
    assert_eq!(fake.waiter_end(0), WaiterEnd::Returned);
}

/// A teardown that fails is still a `STOP_PENDING` followed by the incomplete-stop `STOPPED`: the
/// service must not read as up while the shim tries.
#[skuld::test]
fn a_teardown_that_fails_still_reports_stopping_first(#[fixture(temp_dir)] dir: &Path) {
    let fake = FakeGuard::new();
    fake.will_make_thread();
    fake.will_spawn(FakeProcess::new().exited(7).kill_tree(KillReply::Refuses));

    let (reported, stopping) = run_recording_stopping(&fake, &spec(dir), &bus(&fake), || {});

    assert_eq!(reported, Reported::incomplete_stop());
    assert_eq!(stopping, [0]);
    assert_eq!(fake.take_failures().len(), 1);
    assert_eq!(fake.waiter_end(0), WaiterEnd::Returned);
}

#[skuld::test]
fn no_stopping_report_when_the_daemon_never_ran(#[fixture(temp_dir)] dir: &Path) {
    let fake = FakeGuard::new();
    fake.will_make_thread();
    fake.will_refuse_spawn(not_found());

    let (_, stopping) = run_recording_stopping(&fake, &spec(dir), &bus(&fake), || {});

    assert_eq!(
        stopping,
        Vec::<usize>::new(),
        "STOP_PENDING over a service never RUNNING"
    );
    assert_eq!(fake.take_failures().len(), 1);
    assert_eq!(fake.waiter_end(0), WaiterEnd::Returned);
}

#[skuld::test]
fn no_stopping_report_over_an_uncontained_daemon(#[fixture(temp_dir)] dir: &Path) {
    let fake = FakeGuard::new();
    fake.will_make_thread();
    fake.will_spawn(
        FakeProcess::new()
            .containment(Containment::TreeWalk)
            .kill_tree(KillReply::Ends),
    );

    let (_, stopping) = run_recording_stopping(&fake, &spec(dir), &bus(&fake), || {});

    assert_eq!(
        stopping,
        Vec::<usize>::new(),
        "STOP_PENDING over a service never RUNNING"
    );
    assert_eq!(fake.take_failures().len(), 1);
    assert_eq!(fake.waiter_end(0), WaiterEnd::Returned);
}

/// `type: simple` needs a Job Object to terminate the daemon's whole tree. A daemon contained any
/// other way is killed and the start fails with nothing reported RUNNING. No kill proves a
/// non-job tree gone, so the record is the incomplete stop (never restartable), whatever the kill
/// returned.
#[skuld::test]
fn a_daemon_without_job_object_containment_is_killed_and_reported_possibly_running(#[fixture(temp_dir)] dir: &Path) {
    let fake = FakeGuard::new();
    let daemon = FakeProcess::new()
        .containment(Containment::TreeWalk)
        .kill_tree(KillReply::Ends);
    fake.will_make_thread();
    fake.will_spawn(daemon.clone());
    let mut running_reports = 0;

    let reported = run_daemon(&spec(dir), &bus(&fake), || running_reports += 1);

    assert_eq!(reported, Reported::incomplete_stop());
    assert_eq!(running_reports, 0, "RUNNING was reported for an uncontained daemon");
    assert!(daemon.killed(), "the uncontained daemon was left running");
    assert_one_failure(&fake.take_failures(), ID, NOT_JOB_CONTAINED);
    assert_eq!(fake.waiter_end(0), WaiterEnd::Returned);
}

/// The same when `kill_tree` fails and only the direct child is killed: the descendants were never
/// reached, so this is no more restartable than a kill that reports success.
#[skuld::test]
fn an_uncontained_daemon_whose_tree_kill_failed_is_reported_possibly_running(#[fixture(temp_dir)] dir: &Path) {
    let fake = FakeGuard::new();
    let daemon = FakeProcess::new()
        .containment(Containment::TreeWalk)
        .kill_tree(KillReply::Refuses)
        .kill(KillReply::Ends);
    fake.will_make_thread();
    fake.will_spawn(daemon.clone());

    let reported = run_daemon(&spec(dir), &bus(&fake), || {});

    assert_eq!(reported, Reported::incomplete_stop());
    assert!(daemon.killed());
    assert_eq!(fake.waiter_end(0), WaiterEnd::Returned);
    assert_failures(
        &fake.take_failures(),
        ID,
        &[NOT_JOB_CONTAINED.to_string(), tree_refused_failure()],
    );
}

/// The same, when the uncontained daemon cannot be killed either: it may be running.
#[skuld::test]
fn an_uncontained_daemon_that_cannot_be_killed_is_the_incomplete_stop_record(#[fixture(temp_dir)] dir: &Path) {
    let fake = FakeGuard::new();
    let daemon = FakeProcess::new()
        .containment(Containment::TreeWalk)
        .kill_tree(KillReply::Refuses)
        .kill(KillReply::Refuses);
    fake.will_make_thread();
    fake.will_spawn(daemon.clone());

    let reported = run_daemon(&spec(dir), &bus(&fake), || {});

    assert_eq!(reported, Reported::incomplete_stop());
    assert_eq!(fake.waiter_end(0), WaiterEnd::Returned);
    assert_failures(
        &fake.take_failures(),
        ID,
        &[
            NOT_JOB_CONTAINED.to_string(),
            tree_refused_failure(),
            fallback_refused_failure(),
        ],
    );
    // A real refusal makes dropping the daemon wait with no bound for a process it cannot kill,
    // before anything is reported, so `run_daemon` must let it go undropped.
    assert!(
        daemon.detached(),
        "the shim did not let go of a daemon it could not kill"
    );
    assert!(!daemon.dropped(), "the shim dropped a daemon it could not kill");
    assert!(!daemon.ended(), "the daemon ended though neither kill worked");
}

/// The status query after `ChildExited` can fail; the exit is then a shim failure, not a daemon code.
#[skuld::test]
fn an_exit_status_that_cannot_be_read_is_a_shim_failure(#[fixture(temp_dir)] dir: &Path) {
    let fake = FakeGuard::new();
    fake.will_make_thread();
    fake.will_spawn(
        FakeProcess::new()
            .wait(WaitReply::Exits(7))
            .wait(WaitReply::Fails(None))
            .kill_tree(KillReply::Ends),
    );

    let reported = run_daemon(&spec(dir), &bus(&fake), || {});

    assert_eq!(reported, Reported::ShimFailure(ERROR_GEN_FAILURE));
    assert_one_failure(
        &fake.take_failures(),
        ID,
        &format!("query child exit status: {WAIT_FAILED}"),
    );
    assert_eq!(fake.waiter_end(0), WaiterEnd::Returned);
}

/// The exit is decided once: a stop after the failure is settled does not turn it into a clean stop.
/// SCM acknowledges every `STOP` it delivers whatever the handler answers, so the handler answers
/// `NO_ERROR`.
#[skuld::test]
fn a_stop_after_a_daemon_failure_was_decided_leaves_the_failure(#[fixture(temp_dir)] dir: &Path) {
    let fake = FakeGuard::new();
    let stop_bus = bus(&fake);
    fake.will_make_thread();
    fake.will_spawn(FakeProcess::new().exited(7).kill_tree(KillReply::Ends));

    let reported = run_daemon(&spec(dir), &stop_bus, || {});

    assert_eq!(reported, Reported::DaemonExit(7));
    assert!(
        !stop_bus.request_stop(),
        "a stop was accepted after the failure was decided"
    );
    assert_eq!(handle_control(&stop_bus, SERVICE_CONTROL_STOP), NO_ERROR);
    assert_eq!(stop_bus.settle(Reported::DaemonExit(7)), Reported::DaemonExit(7));
    assert_eq!(fake.waiter_end(0), WaiterEnd::Returned);
}

/// SCM may deliver a control after `STOP`: the handler and the context `leak_handler_context` makes
/// still answer it. That `register_control_handler` passes that context, and not `windows-service`
/// 0.8.1's `register`, is not covered here, or anywhere: `register` frees its closure after any
/// Stop, Shutdown or Preshutdown whatever the handler returned (P16 observed it) while the
/// registration stays live, and only a real SCM shows that. `clippy.toml` forbids the call.
#[skuld::test]
fn a_control_after_a_stop_still_reaches_the_bus() {
    let fake = FakeGuard::new();
    let stop_bus = bus(&fake);
    let context = leak_handler_context(&stop_bus);
    let control = |code| {
        // SAFETY: `context` came from `leak_handler_context` and is never freed.
        unsafe { control_handler(code, 0, std::ptr::null_mut(), context) }
    };

    assert_eq!(control(SERVICE_CONTROL_STOP), NO_ERROR);
    assert_eq!(control(SERVICE_CONTROL_INTERROGATE), NO_ERROR);
    assert_eq!(control(SERVICE_CONTROL_STOP), NO_ERROR);
    assert_eq!(control(SERVICE_CONTROL_PAUSE), ERROR_CALL_NOT_IMPLEMENTED);
    assert_eq!(
        stop_bus.settle(Reported::DaemonExit(7)),
        Reported::Clean,
        "the stop did not reach the bus"
    );
}

/// The other order: the stop is accepted before the exit is decided, so the exit is a clean stop even
/// though the daemon's own code is non-zero.
#[skuld::test]
fn a_stop_accepted_before_a_daemon_failure_was_decided_is_clean(#[fixture(temp_dir)] dir: &Path) {
    let fake = FakeGuard::new();
    let stop_bus = bus(&fake);
    // The accepted stop is what the pass acts on, so the exit status is never read: one wait.
    fake.will_make_thread();
    fake.will_spawn(FakeProcess::new().wait(WaitReply::Exits(7)).kill_tree(KillReply::Ends));

    let reported = run_daemon(&spec(dir), &stop_bus, || {
        assert_eq!(handle_control(&stop_bus, SERVICE_CONTROL_STOP), NO_ERROR)
    });

    assert_eq!(reported, Reported::Clean);
    assert_eq!(fake.waiter_end(0), WaiterEnd::Returned);
}

// Pure helpers --------------------------------------------------------------------------------------------------------

#[skuld::test]
fn an_exit_code_becomes_a_daemon_report() {
    assert_eq!(exit_report(Some(0)), Reported::Clean);
    assert_eq!(exit_report(Some(7)), Reported::DaemonExit(7));
    assert_eq!(exit_report(Some(-1073741819)), Reported::DaemonExit(0xC000_0005));
}

#[skuld::test]
fn an_unobtainable_exit_status_is_a_shim_failure() {
    assert_eq!(
        exit_report(None),
        Reported::ShimFailure(ERROR_GEN_FAILURE),
        "an exit with no code must not read as a daemon's code or as clean"
    );
}

#[skuld::test]
fn each_report_maps_to_the_exit_codes_scm_records() {
    let codes = |reported| {
        let status = stopped_status(reported);
        (status.dwWin32ExitCode, status.dwServiceSpecificExitCode)
    };

    assert_eq!(codes(Reported::Clean), (0, 0));
    assert_eq!(codes(Reported::DaemonExit(7)), (ERROR_SERVICE_SPECIFIC_ERROR, 7));
    assert_eq!(codes(Reported::shim_failure(Some(13))), (13, 0));
    assert_ne!(
        codes(Reported::shim_failure(Some(ERROR_SERVICE_SPECIFIC_ERROR as i32))).0,
        ERROR_SERVICE_SPECIFIC_ERROR,
        "1066 reads as a daemon's code"
    );
}

#[skuld::test]
fn an_incomplete_stop_is_reported_as_win32_zero_with_specific_1003() {
    // Win32 0 is what keeps SCM from starting a second daemon beside the one that would not die;
    // the specific code is the record `sc query` and the manager can read.
    let status = stopped_status(Reported::incomplete_stop());

    assert_eq!(
        (status.dwWin32ExitCode, status.dwServiceSpecificExitCode),
        (0, ERROR_CAN_NOT_COMPLETE)
    );
}

#[skuld::test]
fn a_stopped_report_is_stopped_and_accepts_no_control() {
    let status = stopped_status(Reported::Clean);

    assert_eq!(status.dwCurrentState, SERVICE_STOPPED);
    assert_eq!(status.dwControlsAccepted, 0);
}

#[skuld::test]
fn the_os_error_of_an_io_failure_is_kept_and_other_failures_have_none() {
    assert_eq!(
        os_error_of(&cosca::error::Error::Io(std::io::Error::from_raw_os_error(
            ERROR_ACCESS_DENIED as i32
        ))),
        Some(ERROR_ACCESS_DENIED as i32)
    );
    // A failure without a code has none, whatever its kind.
    assert_eq!(os_error_of(&not_found()), None);
    assert_eq!(
        os_error_of(&cosca::error::Error::Unassessable {
            detail: "query".to_string(),
            source: Some(std::io::Error::from(std::io::ErrorKind::NotFound)),
        }),
        None
    );
    assert_eq!(
        os_error_of(&cosca::error::Error::Io(std::io::Error::other("no os code"))),
        None
    );
    assert_eq!(
        os_error_of(&cosca::error::Error::Containment {
            detail: "attach".to_string()
        }),
        None
    );
}

#[skuld::test]
fn a_refused_spawn_without_a_code_that_is_not_found_is_told_to_scm_as_file_not_found() {
    assert_eq!(spawn_error_code(&not_found()), Some(ERROR_FILE_NOT_FOUND as i32));
    // A real code wins over the kind's: ERROR_PATH_NOT_FOUND is `NotFound` too.
    assert_eq!(
        spawn_error_code(&cosca::error::Error::Io(std::io::Error::from_raw_os_error(
            ERROR_PATH_NOT_FOUND as i32
        ))),
        Some(ERROR_PATH_NOT_FOUND as i32)
    );
    assert_eq!(
        spawn_error_code(&cosca::error::Error::Io(std::io::Error::other("no os code"))),
        None
    );
}

// Services registered by an earlier version ---------------------------------------------------------------------------

fn spec_restarting(restart: Restart) -> DaemonSpec {
    DaemonSpec {
        restart,
        ..spec(Path::new("unused"))
    }
}

#[skuld::test]
fn an_earlier_registration_under_on_failure_names_the_daemon_and_the_remedy() {
    let notice = version_skew_notice("frpc", &spec_restarting(Restart::OnFailure), "0.1.0", "0.2.0");

    assert_eq!(
        notice.as_deref(),
        Some(
            r#"daemon "goetia-shim launch test" was registered by goetia 0.1.0 and this goetia-shim is 0.2.0: its SCM recovery actions may be missing, so it may not restart after a failure. Run `goetia daemon install` to apply them"#
        )
    );
}

#[skuld::test]
fn an_earlier_registration_under_always_names_the_override_and_the_reinstall() {
    let notice = version_skew_notice("frpc", &spec_restarting(Restart::Always), "0.1.0", "0.2.0");

    assert_eq!(
        notice.as_deref(),
        Some(
            r#"daemon "goetia-shim launch test" was registered by goetia 0.1.0 and this goetia-shim is 0.2.0: its SCM recovery actions may be missing, so it may not restart after a failure. After setting `backend-specific: scm: { restart: on-failure }` in the manifest, run `goetia daemon uninstall frpc` and `goetia daemon install` to apply them: `restart: always` is not supported for scm, so `install` refuses this service as it is"#
        )
    );
}

#[skuld::test]
fn a_registration_by_released_0_1_0_warns_under_this_build() {
    let notice = version_skew_notice("frpc", &spec_restarting(Restart::OnFailure), "0.1.0", goetia::version());

    assert!(notice.is_some(), "a service registered by 0.1.0 must get the warning");
}

#[skuld::test]
fn no_notice_for_a_daemon_that_is_never_restarted() {
    assert_eq!(
        version_skew_notice("frpc", &spec_restarting(Restart::Never), "0.1.0", "0.2.0"),
        None
    );
}

#[skuld::test]
fn no_notice_when_the_shim_is_the_version_that_registered_the_service() {
    for restart in [Restart::Never, Restart::OnFailure, Restart::Always] {
        assert_eq!(
            version_skew_notice("frpc", &spec_restarting(restart), "0.2.0", "0.2.0"),
            None,
            "{restart:?}"
        );
    }
}

/// The daemon is running, and every start is a new shim that sees the same condition: a warning,
/// and not the error a failure to run would be.
#[skuld::test]
fn the_version_skew_notice_is_logged_as_a_warning_and_not_as_a_failure() {
    let fake = FakeGuard::new();

    log_version_skew(&fake.log(), ID, &spec_restarting(Restart::OnFailure), "0.1.0", "0.2.0");

    assert!(fake.take_failures().is_empty(), "the notice was logged as a failure");
    let warnings = fake.take_warnings();
    let [line] = warnings.as_slice() else {
        panic!("expected exactly one warning, got {warnings:?}");
    };
    assert!(line.contains("registered by goetia 0.1.0"), "{line}");
}

#[skuld::test]
fn nothing_is_logged_for_a_service_the_shim_has_no_notice_about() {
    let fake = FakeGuard::new();
    let log = fake.log();

    log_version_skew(&log, ID, &spec_restarting(Restart::Never), "0.1.0", "0.2.0");
    log_version_skew(&log, ID, &spec_restarting(Restart::OnFailure), "0.2.0", "0.2.0");

    assert!(fake.take_failures().is_empty());
    assert!(fake.take_warnings().is_empty());
}

/// A `STOPPED` the shim could not report is expected to read as a crash to SCM (1067), with the
/// restart policy possibly applied; the log line says so as an expectation, not a measurement.
#[skuld::test]
fn a_failed_stopped_report_says_scm_is_expected_to_read_a_crash() {
    let text = stopped_report_failure(&std::io::Error::from_raw_os_error(6));
    for part in [
        "report SERVICE_STOPPED to SCM",
        "expected",
        "1067",
        "may apply the restart policy",
    ] {
        assert!(text.contains(part), "missing {part:?}: {text}");
    }
}
