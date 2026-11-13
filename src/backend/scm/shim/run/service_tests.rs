//! What `launch` and `run_daemon` do, which is the whole of this module that can be tested without
//! a real SCM, over the `Fake`'s stated replies. `launch` makes the thread that will wait on the
//! daemon before the daemon, so the OS refusing that thread is a start failure with nothing
//! launched rather than an orphaned daemon nothing waits on. `run_daemon` is one pass: it reports
//! running after the spawn, never after a failed launch, and turns the daemon's end into what SCM
//! is told.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::backend::scm::exit_status::{
    ERROR_CAN_NOT_COMPLETE, ERROR_GEN_FAILURE, ERROR_INVALID_DATA, ERROR_SERVICE_SPECIFIC_ERROR, Reported, SHIM_REFUSED,
};
use crate::spec::{DaemonFields, DaemonSpec, Protocol, Restart, ServiceFields, ServiceSpec};
use crate::testing::{base_fields, service_fields};
use cosca::Containment;
use cosca::error::ChildFate;
use windows_sys::Win32::Foundation::{
    ERROR_ACCESS_DENIED, ERROR_CALL_NOT_IMPLEMENTED, ERROR_FILE_NOT_FOUND, ERROR_NOT_ENOUGH_MEMORY, NO_ERROR,
};
use windows_sys::Win32::System::Services::{
    SERVICE_CONTROL_INTERROGATE, SERVICE_CONTROL_PAUSE, SERVICE_CONTROL_STOP, SERVICE_STOP_PENDING, SERVICE_STOPPED,
};

use super::*;
use crate::backend::scm::shim::run::os::fake::{
    FakeGuard, FakeProcess, KillReply, NO_THREAD, OPEN_LOG_FAILED, Op, WAIT_FAILED, WaitReply, WaiterEnd,
};
use crate::backend::scm::shim::run::test_support::{
    assert_failures, assert_one_failure, fallback_refused_failure, not_found, supervisor, tree_refused_failure,
};

const ID: &str = "goetia-shim-launch-test";

/// [`super::run_daemon`] for the tests that do not look at the `STOP_PENDING` report.
fn run_daemon(spec: &DaemonSpec, supervisor: &Supervisor, on_running: impl FnOnce()) -> Reported {
    super::run_daemon(spec, supervisor, ID, on_running, || {})
}

/// Where [`spec`]'s daemon logs. The `Fake` opens nothing there.
const DAEMON_LOG: &str = r"C:\goetia-shim-test\daemon.log";

/// The program of [`spec`]'s command, absolute as `ServiceSpec::new` requires. Forward slashes, so the
/// `Debug` text of the command that the expected messages below spell out has no escapes.
const POWERSHELL: &str = "C:/Windows/System32/WindowsPowerShell/v1.0/powershell.exe";

/// A `type: simple` daemon. Nothing runs it: the `Fake` states what happens to it, and opens its
/// log, and takes the shim's failure lines, without touching the file system.
fn spec() -> DaemonSpec {
    let command = [
        POWERSHELL,
        "-NoProfile",
        "-NonInteractive",
        "-Command",
        "Start-Sleep -Seconds 300",
    ];
    ServiceSpec::new({
        let base = base_fields(ID, command.map(str::to_string).to_vec());
        ServiceFields {
            common: DaemonFields {
                name: "goetia-shim launch test".to_string(),
                logs: Some(PathBuf::from(DAEMON_LOG)),
                ..base.common
            },
            restart: Restart::Always,
            protocol: Protocol::Plain,
            ..base
        }
    })
    .map(DaemonSpec::from)
    .unwrap()
}

/// The line logged when no thread can be made to wait on the daemon.
fn no_thread_failure() -> String {
    format!(
        r#"make the thread that waits on ["C:/Windows/System32/WindowsPowerShell/v1.0/powershell.exe", "-NoProfile", "-NonInteractive", "-Command", "Start-Sleep -Seconds 300"]: {NO_THREAD}; it was not spawned, so nothing of this daemon is running"#
    )
}

/// The line logged when the spawn is refused with [`not_found`].
fn spawn_failure() -> String {
    format!(
        r#"spawn ["C:/Windows/System32/WindowsPowerShell/v1.0/powershell.exe", "-NoProfile", "-NonInteractive", "-Command", "Start-Sleep -Seconds 300"]: {}"#,
        cosca::error::Report::new(&not_found())
    )
}

/// The line logged when the daemon is not job-contained, whatever the kill then does.
const NOT_JOB_CONTAINED: &str = r#"the daemon ["C:/Windows/System32/WindowsPowerShell/v1.0/powershell.exe", "-NoProfile", "-NonInteractive", "-Command", "Start-Sleep -Seconds 300"] was contained by process-tree walk, not a job object: `type: simple` needs a job object to terminate its whole tree. The daemon ran from its launch, and no kill of it shows that its descendants are gone, so it is reported as possibly still running"#;

/// The line logged when the daemon's tree cannot be terminated after the daemon ended.
const EXITED_TREE_UNKILLED: &str = "kill_tree after the daemon ended: an I/O operation failed: the kill was refused (fake); its descendants may still be running — SCM is told the service stopped with Win32 exit code 0, so that it does not start a second copy beside it, and with service-specific exit code 1003 as the record of this failure";

#[skuld::test]
fn no_daemon_is_spawned_when_no_thread_can_be_made_to_wait_on_it() {
    let fake = FakeGuard::new();
    fake.will_refuse_thread(std::io::Error::other(NO_THREAD));

    // Reported, not panicked: a refused thread here surfaces as an `io::Result`, not a
    // `Scope::spawn` panic that would take the whole service process down with the daemon already
    // running.
    let launched = launch(&spec(), &supervisor(&fake), ID);

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
    // A witness beside the spawn count: `build_command` opens the daemon's log before the spawn is
    // reached, so an open shows `launch` got past the waiter. The control test below asserts the
    // log is opened when the spawn is reached, so this is not vacuous.
    assert_eq!(
        fake.calls(Op::OpenLog),
        0,
        "the daemon's log was opened, so launch reached build_command — it was preparing to spawn \
         before the thread that would have waited on the daemon was made"
    );
}

#[skuld::test]
fn a_daemon_is_spawned_once_the_thread_that_waits_on_it_exists() {
    // The control for the assertions above: `launch` does reach the spawn, and both things they
    // assert against — the spawn count and the open of the daemon's log — do move when the thread
    // it needs first can be made.
    let fake = FakeGuard::new();
    let daemon = FakeProcess::new();
    fake.will_make_thread();
    fake.will_spawn(daemon.clone());

    let launched = launch(&spec(), &supervisor(&fake), ID);

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
    assert_eq!(
        fake.calls(Op::OpenLog),
        1,
        "the daemon's log was not opened even though the spawn was reached — the assertion above \
         that it is not opened when the spawn is not reached would then hold for the wrong reason"
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
fn a_daemon_that_cannot_be_spawned_is_logged_and_nothing_is_left_running() {
    let fake = FakeGuard::new();
    fake.will_make_thread();
    fake.will_refuse_spawn(not_found());

    let launched = launch(&spec(), &supervisor(&fake), ID);

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
fn the_os_code_of_a_refused_spawn_is_what_launch_reports() {
    const ACCESS_DENIED: i32 = ERROR_ACCESS_DENIED as i32;
    let fake = FakeGuard::new();
    fake.will_make_thread();
    fake.will_refuse_spawn(cosca::error::Error::Io(std::io::Error::from_raw_os_error(
        ACCESS_DENIED,
    )));

    let launched = launch(&spec(), &supervisor(&fake), ID);

    assert_eq!(
        launched.as_ref().err(),
        Some(&Reported::shim_failure(Some(ACCESS_DENIED)))
    );
    // The refusal's text is not the point here; consume the one failure line and the waiter.
    assert_eq!(fake.take_failures().len(), 1);
    assert_eq!(fake.waiter_end(0), WaiterEnd::Returned);
}

/// A spawn that failed after `CreateProcess` succeeded, with the child's `fate` and a source that
/// carries an OS code.
fn may_have_started(fate: ChildFate) -> cosca::error::Error {
    cosca::error::Error::MayHaveStarted {
        fate,
        wrapper_elevated: false,
        detail: None,
        source: Arc::new(cosca::error::Error::Io(std::io::Error::from_raw_os_error(
            ERROR_ACCESS_DENIED as i32,
        ))),
        unreaped: None,
        cleanup: None,
    }
}

/// A spawn error whose fate says the daemon may be running must not be a failure SCM restarts, or
/// its failure action starts a second copy beside the first.
#[skuld::test]
fn a_spawn_error_that_may_have_left_the_daemon_running_is_the_incomplete_stop_record() {
    for fate in [ChildFate::Running { id: None }, ChildFate::Unknown] {
        let fake = FakeGuard::new();
        fake.will_make_thread();
        fake.will_refuse_spawn(may_have_started(fate));

        let launched = launch(&spec(), &supervisor(&fake), ID);

        assert_eq!(launched.as_ref().err(), Some(&Reported::incomplete_stop()), "{fate:?}");
        let expected = format!(
            r#"spawn ["C:/Windows/System32/WindowsPowerShell/v1.0/powershell.exe", "-NoProfile", "-NonInteractive", "-Command", "Start-Sleep -Seconds 300"]: {}; the daemon may still be running{NOT_RESTARTED}"#,
            cosca::error::Report::new(&may_have_started(fate))
        );
        assert_one_failure(&fake.take_failures(), ID, &expected);
        assert_eq!(fake.waiter_end(0), WaiterEnd::Returned);
    }
}

/// A spawn error whose child cosca ended is a restartable failure, and SCM is told the OS code of the
/// failure inside the `MayHaveStarted`.
#[skuld::test]
fn a_spawn_error_that_ended_the_daemon_is_a_restartable_failure_with_the_code_of_its_source() {
    for fate in [ChildFate::Reaped, ChildFate::Killed, ChildFate::Gone] {
        let fake = FakeGuard::new();
        fake.will_make_thread();
        fake.will_refuse_spawn(may_have_started(fate));

        let launched = launch(&spec(), &supervisor(&fake), ID);

        assert_eq!(
            launched.as_ref().err(),
            Some(&Reported::shim_failure(Some(ERROR_ACCESS_DENIED as i32))),
            "{fate:?}"
        );
        assert_eq!(fake.take_failures().len(), 1, "{fate:?}");
        assert_eq!(fake.waiter_end(0), WaiterEnd::Returned);
    }
}

#[skuld::test]
fn the_os_code_of_a_refused_thread_is_what_launch_reports() {
    const NO_MEMORY: i32 = ERROR_NOT_ENOUGH_MEMORY as i32;
    let fake = FakeGuard::new();
    fake.will_refuse_thread(std::io::Error::from_raw_os_error(NO_MEMORY));

    let launched = launch(&spec(), &supervisor(&fake), ID);

    assert_eq!(launched.as_ref().err(), Some(&Reported::shim_failure(Some(NO_MEMORY))));
    // The refusal's text is not the point here; consume the one failure line.
    assert_eq!(fake.take_failures().len(), 1);
    assert_eq!(fake.calls(Op::Spawn), 0);
}

#[skuld::test]
fn a_log_file_that_cannot_be_opened_is_logged_and_the_daemon_still_runs() {
    let fake = FakeGuard::new();
    let daemon = FakeProcess::new();
    fake.will_make_thread();
    fake.will_spawn(daemon.clone());
    fake.will_fail_open_log(Path::new(DAEMON_LOG));

    let launched = launch(&spec(), &supervisor(&fake), ID);

    assert!(
        launched.is_ok(),
        "an unopenable log file stopped the daemon from running"
    );
    assert_one_failure(
        &fake.take_failures(),
        ID,
        &format!(
            "open log file {DAEMON_LOG}: {OPEN_LOG_FAILED} (this daemon's output will not be captured; it is still being run)"
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
fn no_running_report_when_no_thread_can_be_made() {
    let fake = FakeGuard::new();
    fake.will_refuse_thread(std::io::Error::other(NO_THREAD));
    let mut running_reports = 0;

    let reported = run_daemon(&spec(), &supervisor(&fake), || running_reports += 1);

    assert_eq!(
        running_reports, 0,
        "RUNNING was reported for a daemon nothing could wait on"
    );
    assert_eq!(reported, Reported::shim_failure(None));
    assert_one_failure(&fake.take_failures(), ID, &no_thread_failure());
    assert_eq!(fake.calls(Op::Spawn), 0, "a daemon was spawned");
}

#[skuld::test]
fn no_running_report_when_the_spawn_fails() {
    let fake = FakeGuard::new();
    fake.will_make_thread();
    fake.will_refuse_spawn(not_found());
    let mut running_reports = 0;

    let reported = run_daemon(&spec(), &supervisor(&fake), || running_reports += 1);

    assert_eq!(
        running_reports, 0,
        "RUNNING was reported for a daemon that never started"
    );
    assert_eq!(reported, Reported::shim_failure(Some(ERROR_FILE_NOT_FOUND as i32)));
    assert_one_failure(&fake.take_failures(), ID, &spawn_failure());
    assert_eq!(fake.waiter_end(0), WaiterEnd::Returned);
}

#[skuld::test]
fn running_is_reported_once_and_after_the_spawn() {
    let fake = FakeGuard::new();
    let supervisor = supervisor(&fake);
    fake.will_make_thread();
    fake.will_spawn(FakeProcess::running());
    let mut running_reports = Vec::new();

    // The stop ends the pass once the report is made; the daemon runs until then.
    let reported = run_daemon(&spec(), &supervisor, || {
        running_reports.push(fake.calls(Op::Spawn));
        supervisor.bus.request_stop();
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
fn a_daemon_that_exits_nonzero_is_one_pass() {
    let fake = FakeGuard::new();
    fake.will_make_thread();
    fake.will_spawn(FakeProcess::new().exited(7).kill_tree(KillReply::Ends));
    // The shim ignores the restart policy: no respawn.
    let daemon = ServiceSpec::new(ServiceFields {
        restart: Restart::OnFailure,
        restart_delay: None,
        ..service_fields(spec())
    })
    .map(DaemonSpec::from)
    .unwrap();

    let reported = run_daemon(&daemon, &supervisor(&fake), || {});

    assert_eq!(reported, Reported::DaemonExit(7));
    assert_eq!(fake.calls(Op::Spawn), 1, "the daemon was spawned again");
    assert_eq!(fake.waiter_end(0), WaiterEnd::Returned);
}

#[skuld::test]
fn a_daemon_that_exits_zero_is_clean() {
    let fake = FakeGuard::new();
    fake.will_make_thread();
    fake.will_spawn(FakeProcess::new().exited(0).kill_tree(KillReply::Ends));

    let reported = run_daemon(&spec(), &supervisor(&fake), || {});

    assert_eq!(reported, Reported::Clean);
    assert_eq!(fake.waiter_end(0), WaiterEnd::Returned);
}

#[skuld::test]
fn an_unkillable_daemon_reports_the_incomplete_stop_failure() {
    let fake = FakeGuard::new();
    let supervisor = supervisor(&fake);
    // Both kills are refused, so the daemon outlives `run_daemon`: the test ends it, which is what
    // lets the detached waiter thread be consumed.
    let daemon = FakeProcess::new()
        .wait(WaitReply::Blocks)
        .kill_tree(KillReply::Refuses)
        .kill(KillReply::Refuses);
    fake.will_make_thread();
    fake.will_spawn(daemon.clone());

    let reported = run_daemon(&spec(), &supervisor, || {
        supervisor.bus.request_stop();
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
fn a_failed_wait_is_a_shim_failure_with_the_os_error_and_the_daemon_is_dead() {
    let fake = FakeGuard::new();
    let daemon = FakeProcess::new()
        .wait(WaitReply::Fails(Some(5)))
        .kill_tree(KillReply::Ends);
    fake.will_make_thread();
    fake.will_spawn(daemon.clone());

    let reported = run_daemon(&spec(), &supervisor(&fake), || {});

    assert_eq!(reported, Reported::shim_failure(Some(5)));
    assert!(
        daemon.killed(),
        "a restartable failure was reported over a daemon still running"
    );
    assert_one_failure(
        &fake.take_failures(),
        ID,
        &format!(
            "background wait on child: an I/O operation failed: {}",
            std::io::Error::from_raw_os_error(5)
        ),
    );
    assert_eq!(fake.waiter_end(0), WaiterEnd::Returned);
}

#[skuld::test]
fn a_failed_wait_with_no_os_error_is_the_generic_shim_failure() {
    let fake = FakeGuard::new();
    fake.will_make_thread();
    fake.will_spawn(
        FakeProcess::new()
            .wait(WaitReply::Fails(None))
            .kill_tree(KillReply::Ends),
    );

    let reported = run_daemon(&spec(), &supervisor(&fake), || {});

    assert_eq!(reported, Reported::ShimFailure(ERROR_GEN_FAILURE));
    assert_one_failure(
        &fake.take_failures(),
        ID,
        &format!("background wait on child: an I/O operation failed: {WAIT_FAILED}"),
    );
    assert_eq!(fake.waiter_end(0), WaiterEnd::Returned);
}

/// A daemon whose tree could not be killed must not be reported as a failure SCM restarts: the
/// restart would start a second copy beside it. The exit is decided, so the bus does not accept a
/// later stop either. The shim does not keep a daemon it has given up on: once the waiter has
/// returned the daemon is dropped, though not while a detached waiter still holds it.
#[skuld::test]
fn a_failed_wait_over_an_unkillable_daemon_is_the_incomplete_stop_record() {
    let fake = FakeGuard::new();
    let supervisor = supervisor(&fake);
    let daemon = FakeProcess::new()
        .wait(WaitReply::Fails(None))
        .kill_tree(KillReply::Refuses)
        .kill(KillReply::Refuses);
    fake.will_make_thread();
    fake.will_spawn(daemon.clone());

    let reported = run_daemon(&spec(), &supervisor, || {});

    assert_eq!(reported, Reported::incomplete_stop());
    assert!(
        !supervisor.bus.request_stop(),
        "the bus accepted a stop over a daemon that may still be running"
    );
    assert_eq!(fake.waiter_end(0), WaiterEnd::Returned);
    assert!(!daemon.ended(), "the daemon ended though neither kill worked");
    assert!(daemon.dropped(), "the shim kept a daemon it had given up on");
    assert_failures(
        &fake.take_failures(),
        ID,
        &[
            format!("background wait on child: an I/O operation failed: {WAIT_FAILED}"),
            tree_refused_failure(),
            fallback_refused_failure(),
        ],
    );
}

/// The daemon has failed, so SCM will restart it: its tree is terminated first, so that the restart
/// never runs beside a tree nobody told to end. When the tree cannot be terminated the restart
/// must not happen.
#[skuld::test]
fn a_failing_daemon_whose_tree_cannot_be_terminated_is_the_incomplete_stop_record() {
    let fake = FakeGuard::new();
    let supervisor = supervisor(&fake);
    fake.will_make_thread();
    fake.will_spawn(FakeProcess::new().exited(7).kill_tree(KillReply::Refuses));

    let reported = run_daemon(&spec(), &supervisor, || {});

    assert_eq!(reported, Reported::incomplete_stop());
    assert!(
        !supervisor.bus.request_stop(),
        "the bus accepted a stop over a daemon whose tree may still be running"
    );
    assert_one_failure(&fake.take_failures(), ID, EXITED_TREE_UNKILLED);
    assert_eq!(fake.waiter_end(0), WaiterEnd::Returned);
}

/// `STOPPED` is never reported over a tree that may still be running, whatever ended the daemon: a
/// `start` that lands before the shim exits would run a second copy beside it. When the tree cannot
/// be terminated, the record is the incomplete stop even for a clean exit.
#[skuld::test]
fn a_clean_exit_whose_tree_cannot_be_terminated_is_the_incomplete_stop_record() {
    let fake = FakeGuard::new();
    fake.will_make_thread();
    fake.will_spawn(FakeProcess::new().exited(0).kill_tree(KillReply::Refuses));

    let reported = run_daemon(&spec(), &supervisor(&fake), || {});

    assert_eq!(reported, Reported::incomplete_stop());
    assert_one_failure(&fake.take_failures(), ID, EXITED_TREE_UNKILLED);
    assert_eq!(fake.waiter_end(0), WaiterEnd::Returned);
}

/// A clean exit whose tree is terminated is reported as it was.
#[skuld::test]
fn a_clean_exit_whose_tree_is_terminated_is_clean() {
    let fake = FakeGuard::new();
    fake.will_make_thread();
    fake.will_spawn(FakeProcess::new().exited(0).kill_tree(KillReply::Ends));

    let reported = run_daemon(&spec(), &supervisor(&fake), || {});

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
    supervisor: &Supervisor,
    on_running: impl FnOnce(),
) -> (Reported, Vec<usize>) {
    let mut stopping = Vec::new();
    let reported = super::run_daemon(daemon, supervisor, ID, on_running, || {
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
fn a_commanded_stop_reports_stopping_once_while_the_daemon_is_still_alive() {
    let fake = FakeGuard::new();
    let supervisor = supervisor(&fake);
    let daemon = FakeProcess::running();
    fake.will_make_thread();
    fake.will_spawn(daemon.clone());
    let mut alive_when_stopping = Vec::new();

    let reported = super::run_daemon(
        &spec(),
        &supervisor,
        ID,
        || {
            supervisor.bus.request_stop();
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
fn a_daemon_that_exits_reports_stopping_before_its_tree_is_terminated() {
    let fake = FakeGuard::new();
    fake.will_make_thread();
    fake.will_spawn(FakeProcess::new().exited(7).kill_tree(KillReply::Ends));

    let (reported, stopping) = run_recording_stopping(&fake, &spec(), &supervisor(&fake), || {});

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
fn a_daemon_that_exits_cleanly_reports_stopping_before_its_tree_is_terminated() {
    let fake = FakeGuard::new();
    fake.will_make_thread();
    fake.will_spawn(FakeProcess::new().exited(0).kill_tree(KillReply::Ends));

    let (reported, stopping) = run_recording_stopping(&fake, &spec(), &supervisor(&fake), || {});

    assert_eq!(reported, Reported::Clean);
    assert_eq!(stopping, [0]);
    assert_eq!(fake.waiter_end(0), WaiterEnd::Returned);
}

#[skuld::test]
fn a_failed_wait_reports_stopping_before_the_daemon_is_killed() {
    let fake = FakeGuard::new();
    fake.will_make_thread();
    fake.will_spawn(
        FakeProcess::new()
            .wait(WaitReply::Fails(Some(5)))
            .kill_tree(KillReply::Ends),
    );

    let (reported, stopping) = run_recording_stopping(&fake, &spec(), &supervisor(&fake), || {});

    assert_eq!(reported, Reported::shim_failure(Some(5)));
    assert_eq!(stopping, [0]);
    assert_one_failure(
        &fake.take_failures(),
        ID,
        &format!(
            "background wait on child: an I/O operation failed: {}",
            std::io::Error::from_raw_os_error(5)
        ),
    );
    assert_eq!(fake.waiter_end(0), WaiterEnd::Returned);
}

/// A teardown that fails is still a `STOP_PENDING` followed by the incomplete-stop `STOPPED`: the
/// service must not read as up while the shim tries.
#[skuld::test]
fn a_teardown_that_fails_still_reports_stopping_first() {
    let fake = FakeGuard::new();
    fake.will_make_thread();
    fake.will_spawn(FakeProcess::new().exited(7).kill_tree(KillReply::Refuses));

    let (reported, stopping) = run_recording_stopping(&fake, &spec(), &supervisor(&fake), || {});

    assert_eq!(reported, Reported::incomplete_stop());
    assert_eq!(stopping, [0]);
    assert_eq!(fake.take_failures().len(), 1);
    assert_eq!(fake.waiter_end(0), WaiterEnd::Returned);
}

#[skuld::test]
fn no_stopping_report_when_the_daemon_never_ran() {
    let fake = FakeGuard::new();
    fake.will_make_thread();
    fake.will_refuse_spawn(not_found());

    let (_, stopping) = run_recording_stopping(&fake, &spec(), &supervisor(&fake), || {});

    assert_eq!(
        stopping,
        Vec::<usize>::new(),
        "STOP_PENDING over a service never RUNNING"
    );
    assert_eq!(fake.take_failures().len(), 1);
    assert_eq!(fake.waiter_end(0), WaiterEnd::Returned);
}

#[skuld::test]
fn no_stopping_report_over_an_uncontained_daemon() {
    let fake = FakeGuard::new();
    fake.will_make_thread();
    fake.will_spawn(
        FakeProcess::new()
            .containment(Containment::TreeWalk)
            .kill_tree(KillReply::Ends),
    );

    let (_, stopping) = run_recording_stopping(&fake, &spec(), &supervisor(&fake), || {});

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
fn a_daemon_without_job_object_containment_is_killed_and_reported_possibly_running() {
    let fake = FakeGuard::new();
    let daemon = FakeProcess::new()
        .containment(Containment::TreeWalk)
        .kill_tree(KillReply::Ends);
    fake.will_make_thread();
    fake.will_spawn(daemon.clone());
    let mut running_reports = 0;

    let reported = run_daemon(&spec(), &supervisor(&fake), || running_reports += 1);

    assert_eq!(reported, Reported::incomplete_stop());
    assert_eq!(running_reports, 0, "RUNNING was reported for an uncontained daemon");
    assert!(daemon.killed(), "the uncontained daemon was left running");
    assert_one_failure(&fake.take_failures(), ID, NOT_JOB_CONTAINED);
    assert_eq!(fake.waiter_end(0), WaiterEnd::Returned);
}

/// The same when `kill_tree` fails and only the direct child is killed: the descendants were never
/// reached, so this is no more restartable than a kill that reports success.
#[skuld::test]
fn an_uncontained_daemon_whose_tree_kill_failed_is_reported_possibly_running() {
    let fake = FakeGuard::new();
    let daemon = FakeProcess::new()
        .containment(Containment::TreeWalk)
        .kill_tree(KillReply::Refuses)
        .kill(KillReply::Ends);
    fake.will_make_thread();
    fake.will_spawn(daemon.clone());

    let reported = run_daemon(&spec(), &supervisor(&fake), || {});

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
fn an_uncontained_daemon_that_cannot_be_killed_is_the_incomplete_stop_record() {
    let fake = FakeGuard::new();
    let daemon = FakeProcess::new()
        .containment(Containment::TreeWalk)
        .kill_tree(KillReply::Refuses)
        .kill(KillReply::Refuses);
    fake.will_make_thread();
    fake.will_spawn(daemon.clone());

    let reported = run_daemon(&spec(), &supervisor(&fake), || {});

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
    assert!(!daemon.ended(), "the daemon ended though neither kill worked");
    assert!(daemon.dropped(), "the shim kept a daemon it had given up on");
}

/// The status query after `ChildExited` can fail; the exit is then a shim failure, not a daemon code.
#[skuld::test]
fn an_exit_status_that_cannot_be_read_is_a_shim_failure() {
    let fake = FakeGuard::new();
    fake.will_make_thread();
    fake.will_spawn(
        FakeProcess::new()
            .wait(WaitReply::Exits(7))
            .wait(WaitReply::Fails(None))
            .kill_tree(KillReply::Ends),
    );

    let reported = run_daemon(&spec(), &supervisor(&fake), || {});

    assert_eq!(reported, Reported::ShimFailure(ERROR_GEN_FAILURE));
    assert_one_failure(
        &fake.take_failures(),
        ID,
        &format!("query child exit status: an I/O operation failed: {WAIT_FAILED}"),
    );
    assert_eq!(fake.waiter_end(0), WaiterEnd::Returned);
}

/// The exit is decided once: a stop after the failure is settled does not turn it into a clean stop.
/// SCM acknowledges every `STOP` it delivers whatever the handler answers, so the handler answers
/// `NO_ERROR`.
#[skuld::test]
fn a_stop_after_a_daemon_failure_was_decided_leaves_the_failure() {
    let fake = FakeGuard::new();
    let supervisor = supervisor(&fake);
    fake.will_make_thread();
    fake.will_spawn(FakeProcess::new().exited(7).kill_tree(KillReply::Ends));

    let reported = run_daemon(&spec(), &supervisor, || {});

    assert_eq!(reported, Reported::DaemonExit(7));
    assert!(
        !supervisor.bus.request_stop(),
        "a stop was accepted after the failure was decided"
    );
    assert_eq!(handle_control(&supervisor.bus, SERVICE_CONTROL_STOP), NO_ERROR);
    assert_eq!(supervisor.bus.settle(Reported::DaemonExit(7)), Reported::DaemonExit(7));
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
    let supervisor = supervisor(&fake);
    let context = leak_handler_context(&supervisor.bus);
    let control = |code| {
        // SAFETY: `context` came from `leak_handler_context` and is never freed.
        unsafe { control_handler(code, 0, std::ptr::null_mut(), context) }
    };

    assert_eq!(control(SERVICE_CONTROL_STOP), NO_ERROR);
    assert_eq!(control(SERVICE_CONTROL_INTERROGATE), NO_ERROR);
    assert_eq!(control(SERVICE_CONTROL_STOP), NO_ERROR);
    assert_eq!(control(SERVICE_CONTROL_PAUSE), ERROR_CALL_NOT_IMPLEMENTED);
    assert_eq!(
        supervisor.bus.settle(Reported::DaemonExit(7)),
        Reported::Clean,
        "the stop did not reach the bus"
    );
}

/// The other order: the stop is accepted before the exit is decided, so the exit is a clean stop even
/// though the daemon's own code is non-zero.
#[skuld::test]
fn a_stop_accepted_before_a_daemon_failure_was_decided_is_clean() {
    let fake = FakeGuard::new();
    let supervisor = supervisor(&fake);
    // The accepted stop is what the pass acts on, so the exit status is never read: one wait.
    fake.will_make_thread();
    fake.will_spawn(FakeProcess::new().wait(WaitReply::Exits(7)).kill_tree(KillReply::Ends));

    let reported = run_daemon(&spec(), &supervisor, || {
        assert_eq!(handle_control(&supervisor.bus, SERVICE_CONTROL_STOP), NO_ERROR)
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
    assert_eq!(
        codes(Reported::shim_failure(Some(ERROR_INVALID_DATA as i32))),
        (ERROR_INVALID_DATA, 0)
    );
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

/// A refusal reports Win32 0 with the code that says no daemon ran.
#[skuld::test]
fn a_refusal_is_reported_as_win32_zero_with_its_own_specific_code() {
    let status = stopped_status(Reported::Refused);

    assert_eq!(
        (status.dwWin32ExitCode, status.dwServiceSpecificExitCode),
        (0, SHIM_REFUSED)
    );
}

// The service name SCM passes -----------------------------------------------------------------------------------------

/// A name SCM could pass that is not UTF-8: a lone surrogate, which a Windows service name can hold.
fn non_utf8_name() -> OsString {
    use std::os::windows::ffi::OsStringExt;
    OsString::from_wide(&[u16::from(b'a'), 0xD800])
}

#[skuld::test]
fn the_first_scm_argument_is_the_service_name() {
    let fake = FakeGuard::new();
    // SCM's later arguments are the service's start parameters: they are not looked at.
    let args = [OsString::from(ID), non_utf8_name()];

    assert_eq!(service_name(&fake.log(), &args), Ok(ID));
    assert_eq!(fake.take_failures(), Vec::<String>::new());
}

#[skuld::test]
fn no_scm_arguments_is_a_logged_refusal_with_the_bad_arguments_code() {
    let fake = FakeGuard::new();

    let refused = service_name(&fake.log(), &[]);

    assert_eq!(refused, Err(ERROR_BAD_ARGUMENTS as i32));
    assert_one_failure(
        &fake.take_failures(),
        TABLE_NAME,
        "SCM passed no usable service name: []",
    );
}

#[skuld::test]
fn a_name_that_is_not_utf8_is_a_logged_refusal_with_the_bad_arguments_code() {
    let fake = FakeGuard::new();
    let args = [non_utf8_name(), OsString::from(ID)];

    let refused = service_name(&fake.log(), &args);

    assert_eq!(refused, Err(ERROR_BAD_ARGUMENTS as i32));
    assert_one_failure(
        &fake.take_failures(),
        TABLE_NAME,
        r#"SCM passed no usable service name: ["a\u{d800}", "goetia-shim-launch-test"]"#,
    );
}

#[skuld::test]
fn an_empty_name_is_a_logged_refusal_with_the_bad_arguments_code() {
    let fake = FakeGuard::new();
    let args = [OsString::new()];

    let refused = service_name(&fake.log(), &args);

    assert_eq!(refused, Err(ERROR_BAD_ARGUMENTS as i32));
    assert_one_failure(
        &fake.take_failures(),
        TABLE_NAME,
        r#"SCM passed no usable service name: [""]"#,
    );
}

// What the shim does with its read of the spec ------------------------------------------------------------------------

fn undetermined(source: Error) -> Error {
    Error::Undetermined {
        id: ID.to_string(),
        reason: Box::new(source),
        recovery: "resolve that failure and re-run".to_string(),
    }
}

/// The refusals are the reads that answered that no daemon can run.
#[skuld::test]
fn a_gone_service_a_missing_marker_and_an_undecodable_spec_are_refused() {
    let reads = [
        Err(Error::NotInstalled { id: ID.to_string() }),
        Ok(None),
        Err(Error::Blob {
            message: "invalid base64".to_string(),
            source: None,
        }),
        Err(Error::Invalid {
            daemon: ID.to_string(),
            message: "id is not valid".to_string(),
            source: None,
        }),
        Err(Error::Step {
            what: "read the spec".to_string(),
            source: Box::new(Error::NotInstalled { id: ID.to_string() }),
        }),
        Err(Error::Kept(std::sync::Arc::new(Error::NotInstalled {
            id: ID.to_string(),
        }))),
    ];

    for read in reads {
        let shown = format!("{read:?}");
        let stop = spec_read(ID, read).map(|_| ()).expect_err(&shown);
        assert_eq!(stop.reported, Reported::Refused, "{shown}");
    }
}

#[skuld::test]
fn a_spec_that_was_read_is_the_one_to_run() {
    let blob = crate::blob::Blob {
        schema: crate::blob::SCHEMA,
        version: crate::version().to_string(),
        spec: spec(),
    };

    let read = spec_read(ID, Ok(Some(blob))).expect("a read spec is run");

    assert_eq!(read.spec, spec());
}

/// A read that did not complete says nothing about the record, so the shim fails restartably (SCM
/// retries it) and the message names the registry read, not decoding. The failure carries the OS
/// error the read met, however deep in the chain.
#[skuld::test]
fn a_read_that_did_not_complete_is_a_restartable_failure_naming_the_read() {
    let io = std::io::Error::from_raw_os_error(ERROR_ACCESS_DENIED as i32);
    let read = Err(Error::Step {
        what: "read the spec".to_string(),
        source: Box::new(undetermined(Error::Os {
            what: r"enumerate HKLM\SYSTEM\CurrentControlSet\Services\x\Parameters".to_string(),
            path: None,
            source: Box::new(io),
        })),
    });

    let stop = spec_read(ID, read)
        .map(|_| ())
        .expect_err("an undetermined read stops the shim");

    assert_eq!(stop.reported, Reported::shim_failure(Some(ERROR_ACCESS_DENIED as i32)));
    assert_ne!(stop.reported.scm_codes().0, 0, "SCM restarts a non-zero Win32 code");
    assert!(
        stop.message.starts_with("read the metadata under Services"),
        "{}",
        stop.message
    );
    assert!(stop.message.contains("enumerate"), "{}", stop.message);
    assert!(!stop.message.contains("decode"), "{}", stop.message);
}

/// Refusal is by cause, not by whether the read is in doubt: a definite error that is none of the
/// three causes (here an OS error from a read that failed outright) is still restartable, and
/// carries the real OS error.
#[skuld::test]
fn a_definite_os_error_is_a_restartable_failure_carrying_it() {
    let read = Err(Error::Step {
        what: "read the spec".to_string(),
        source: Box::new(Error::Os {
            what: r"read HKLM\SYSTEM\CurrentControlSet\Services\x\Parameters".to_string(),
            path: None,
            source: Box::new(std::io::Error::from_raw_os_error(ERROR_ACCESS_DENIED as i32)),
        }),
    });

    let stop = spec_read(ID, read)
        .map(|_| ())
        .expect_err("a failed read stops the shim");

    assert_eq!(stop.reported, Reported::shim_failure(Some(ERROR_ACCESS_DENIED as i32)));
    assert!(
        stop.message.starts_with("read the metadata under Services"),
        "{}",
        stop.message
    );
}

/// With no OS error to carry, the failure still restarts: `ERROR_INVALID_DATA`.
#[skuld::test]
fn an_incomplete_read_with_no_os_error_fails_with_invalid_data() {
    let read = Err(undetermined(Error::Blob {
        message: "no code here".to_string(),
        source: None,
    }));

    let stop = spec_read(ID, read)
        .map(|_| ())
        .expect_err("an undetermined read stops the shim");

    assert_eq!(stop.reported, Reported::shim_failure(Some(ERROR_INVALID_DATA as i32)));
}

#[skuld::test]
fn a_stopped_report_is_stopped_and_accepts_no_control() {
    let status = stopped_status(Reported::Clean);

    assert_eq!(status.dwCurrentState, SERVICE_STOPPED);
    assert_eq!(status.dwControlsAccepted, 0);
}

// Services registered by an earlier version ---------------------------------------------------------------------------

fn spec_restarting(restart: Restart) -> DaemonSpec {
    ServiceSpec::new(ServiceFields {
        restart,
        ..service_fields(spec())
    })
    .map(DaemonSpec::from)
    .unwrap()
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
    let notice = version_skew_notice("frpc", &spec_restarting(Restart::OnFailure), "0.1.0", crate::version());

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

// The waiter of a daemon that never ran -------------------------------------------------------------------------------

/// A waiter not handed the daemon ends on its own once dropped, on a thread nothing joins.
/// `launch` waits for it instead.
#[skuld::test]
fn a_waiter_that_is_not_handed_the_daemon_has_ended_when_launch_returns() {
    let refused_spawn = FakeGuard::new();
    refused_spawn.will_make_thread();
    refused_spawn.will_refuse_spawn(not_found());
    let uncontained = FakeGuard::new();
    uncontained.will_make_thread();
    uncontained.will_spawn(
        FakeProcess::new()
            .containment(Containment::TreeWalk)
            .kill_tree(KillReply::Ends),
    );

    for fake in [&refused_spawn, &uncontained] {
        let launched = launch(&spec(), &supervisor(fake), ID);

        assert!(launched.is_err());
        assert!(
            fake.waiter_has_ended(0),
            "launch returned with the waiter still running"
        );
        assert_eq!(fake.waiter_end(0), WaiterEnd::Returned);
        fake.take_failures();
    }
}

/// A daemon whose root was killed is dropped as any other: its waiter was joined, so the child is
/// reaped.
#[skuld::test]
fn a_daemon_whose_root_was_killed_is_dropped() {
    let fake = FakeGuard::new();
    let daemon = FakeProcess::new()
        .wait(WaitReply::Fails(None))
        .kill_tree(KillReply::Refuses)
        .kill(KillReply::Ends);
    fake.will_make_thread();
    fake.will_spawn(daemon.clone());

    let reported = run_daemon(&spec(), &supervisor(&fake), || {});

    assert_eq!(reported, Reported::incomplete_stop());
    assert_eq!(fake.waiter_end(0), WaiterEnd::Returned);
    assert!(daemon.dropped(), "the shim leaked a daemon whose root it had killed");
    fake.take_failures();
}
