//! What `launch` and `run_daemon` do, which is the whole of this module that can be tested without
//! a real SCM. `launch` makes the thread that will wait on the daemon before the daemon, so the OS
//! refusing that thread is a start failure with nothing launched rather than an orphaned daemon
//! nothing waits on. `run_daemon` is one pass: it reports running after the spawn, never after a
//! failed launch, and turns the daemon's end into what SCM is told.

use std::collections::BTreeMap;
use std::path::Path;

use cosca::Containment;
use cosca::identity::Liveness;
use goetia::backend::scm::exit_status::{
    ERROR_CAN_NOT_COMPLETE, ERROR_GEN_FAILURE, ERROR_SERVICE_SPECIFIC_ERROR, Reported,
};
use goetia::spec::{Id, Kind, Restart, User};
use windows_sys::Win32::Foundation::{ERROR_CALL_NOT_IMPLEMENTED, NO_ERROR};
use windows_sys::Win32::System::Services::{
    SERVICE_CONTROL_INTERROGATE, SERVICE_CONTROL_PAUSE, SERVICE_CONTROL_STOP, SERVICE_STOP_PENDING, SERVICE_STOPPED,
};

use super::*;
use crate::logging::test_hook::{take, take_warnings};
use crate::stop_bus::test_hook as waiter_threads;
use crate::stop_bus::test_hook::{KILL_REFUSED, Kill, WAIT_FAILED, Wait, WaiterEnd, kills, waiter_end, waits};
use crate::test_support::{TestId, assert_failures, assert_one_failure};

const PREFIX: &str = "goetia-shim-launch-test";

/// [`super::run_daemon`] for the tests that do not look at the `STOP_PENDING` report.
fn run_daemon(spec: &DaemonSpec, stop_bus: &Arc<StopBus>, id: &str, on_running: impl FnOnce()) -> Reported {
    super::run_daemon(spec, stop_bus, id, on_running, || {})
}

/// A `type: simple` daemon that keeps running until it is killed — the case where a lost waiter
/// would leave a real orphan, rather than a process that was about to exit anyway.
///
/// `logs` is set rather than left to `build_command`'s `%ProgramData%\Goetia\logs\` default: a
/// test must not append to the path a really-installed daemon of the same id logs to. That covers
/// the daemon's own output only: `launch`'s failure reporting goes through `logging::log_failure`,
/// which never consults `spec.logs`.
fn spec(dir: &Path, id: &str) -> DaemonSpec {
    DaemonSpec {
        id: Id::try_from(id).expect("a valid daemon id"),
        name: "goetia-shim launch test".to_string(),
        command: vec![
            "powershell".to_string(),
            "-NoProfile".to_string(),
            "-NonInteractive".to_string(),
            "-Command".to_string(),
            "Start-Sleep -Seconds 300".to_string(),
        ],
        cwd: None,
        env: BTreeMap::new(),
        user: User::Root,
        restart: Restart::Always,
        restart_delay: None,
        logs: Some(dir.join("daemon.log")),
        kind: Kind::Simple,
    }
}

#[skuld::test]
fn no_daemon_is_spawned_when_no_thread_can_be_made_to_wait_on_it() {
    let dir = tempfile::tempdir().expect("a temp dir for the daemon's log");
    let id = TestId::new(PREFIX);
    let stop_bus = Arc::new(StopBus::new());
    let spawned_before = test_hook::spawns();
    let _no_thread = waiter_threads::threads(0);

    // Reported, not panicked: a refused thread here surfaces as an `io::Result`, not a
    // `Scope::spawn` panic that would take the whole service process down with the daemon already
    // running.
    let launched = launch(&spec(dir.path(), id.as_str()), &stop_bus, id.as_str());

    assert_eq!(
        launched.as_ref().err(),
        Some(&Reported::shim_failure(None)),
        "launch reported a daemon that nothing could wait on"
    );
    // The failure `launch` reports, journalled rather than written.
    assert_one_failure(
        &take(id.as_str()),
        id.as_str(),
        &format!(
            r#"make the thread that waits on ["powershell", "-NoProfile", "-NonInteractive", "-Command", "Start-Sleep -Seconds 300"]: {}; it was not spawned, so nothing of this daemon is running"#,
            waiter_threads::NO_THREAD
        ),
    );
    assert_eq!(
        test_hook::spawns(),
        spawned_before,
        "the daemon was spawned before the thread that would have waited on it was made — \
         a failure there leaves it running with nothing waiting on it"
    );
    // A witness outside the `#[cfg(test)]` counter above, which on its own would keep passing if
    // that hook ever drifted from the production wiring: `build_command` opens this file with
    // `create(true)` before `start` is ever reached, so its existence is evidence from the
    // filesystem that `launch` got past the waiter. The control test below asserts it does exist
    // when the spawn is reached, so this is not vacuous. `bounded_tests.rs`'s
    // `assert!(!marker.exists(), "…: the child ran")` is the precedent.
    assert!(
        !dir.path().join("daemon.log").exists(),
        "the daemon's log file was created, so launch reached build_command — it was preparing to \
         spawn before the thread that would have waited on the daemon was made"
    );
}

#[skuld::test]
fn a_daemon_is_spawned_once_the_thread_that_waits_on_it_exists() {
    // The control for the assertions above: `launch` does reach the spawn, and both things they
    // assert against — the counter and the daemon's log file — do move when the thread it needs
    // first can be made.
    let dir = tempfile::tempdir().expect("a temp dir for the daemon's log");
    let id = TestId::new(PREFIX);
    let stop_bus = Arc::new(StopBus::new());
    let spawned_before = test_hook::spawns();

    let launched = launch(&spec(dir.path(), id.as_str()), &stop_bus, id.as_str());

    assert!(launched.is_ok(), "launch failed with every thread available");
    assert_eq!(
        take(id.as_str()),
        Vec::<String>::new(),
        "a launch that succeeded reported a failure"
    );
    assert_eq!(
        test_hook::spawns(),
        spawned_before + 1,
        "launch returned a daemon without counting a spawn"
    );
    assert!(
        dir.path().join("daemon.log").exists(),
        "no daemon log file even though the spawn was reached — the assertion above that there is \
         none when it is not reached would then hold for the wrong reason"
    );

    // The waiter was never handed this child (no `wait_for_child_or_stop` here), so dropping it
    // drops the last `Arc`: `cosca::Child::drop` hard-kills the job object and waits for it.
    // Asserted rather than left as prose: this is a real five-minute `Start-Sleep`, and a
    // `Child::drop` that ever became fire-and-forget would leak one per CI run while every
    // assertion above still passed. The identity is kept because the drop takes the handle with
    // it, and it is an identity rather than a bare pid so a recycled pid cannot read as alive.
    let daemon = launched
        .as_ref()
        .map(|(child, _waiter)| child.id())
        .expect("the daemon just asserted launched");
    assert_eq!(
        daemon.is_alive(),
        Liveness::Alive,
        "the daemon is not running even before the drop, so the assertion after it would hold for the \
         wrong reason"
    );
    drop(launched);
    // The un-handed waiter ends on drop; consume its end.
    assert_eq!(
        waiter_threads::waiter_end(id.as_str()),
        waiter_threads::WaiterEnd::Returned
    );
    assert_eq!(
        daemon.is_alive(),
        Liveness::Dead,
        "the daemon outlived the `Child` that owned it: `Child::drop` kills the tree and waits for it, so \
         by here it is dead and reaped"
    );
}

#[skuld::test]
fn a_daemon_that_cannot_be_spawned_is_logged_and_nothing_is_left_running() {
    let dir = tempfile::tempdir().expect("a temp dir for the daemon's log");
    let id = TestId::new(PREFIX);
    let stop_bus = Arc::new(StopBus::new());
    let missing = DaemonSpec {
        command: vec!["goetia-shim-test-no-such-binary".to_string()],
        ..spec(dir.path(), id.as_str())
    };

    let launched = launch(&missing, &stop_bus, id.as_str());

    // The OS's own refusal, asked for independently of the log line under test.
    let refusal = start(&mut build_command(&missing, id.as_str())).expect_err("the binary does not exist");
    assert_eq!(
        launched.as_ref().err(),
        Some(&Reported::shim_failure(os_error_of(&refusal))),
        "launch reported a daemon whose spawn failed, or lost the OS error"
    );
    assert_one_failure(
        &take(id.as_str()),
        id.as_str(),
        &format!("spawn [\"goetia-shim-test-no-such-binary\"]: {refusal}"),
    );
    // The un-handed waiter ends on drop; consume its end.
    assert_eq!(
        waiter_threads::waiter_end(id.as_str()),
        waiter_threads::WaiterEnd::Returned
    );
}

#[skuld::test]
fn a_log_file_that_cannot_be_opened_is_logged_and_the_daemon_still_runs() {
    let dir = tempfile::tempdir().expect("a temp dir for the daemon's log");
    let id = TestId::new(PREFIX);
    let stop_bus = Arc::new(StopBus::new());
    // A directory cannot be created under a regular file.
    let file = dir.path().join("a-file");
    std::fs::write(&file, "x").expect("a regular file to use as a parent");
    let unopenable = file.join("daemon.log");
    let daemon = DaemonSpec {
        logs: Some(unopenable.clone()),
        ..spec(dir.path(), id.as_str())
    };

    let launched = launch(&daemon, &stop_bus, id.as_str());

    assert!(
        launched.is_ok(),
        "an unopenable log file stopped the daemon from running"
    );
    let refusal = logging::open_append(&unopenable).expect_err("the path cannot be opened");
    assert_one_failure(
        &take(id.as_str()),
        id.as_str(),
        &format!(
            "open log file {}: {refusal} (this daemon's output will not be captured; it is still being run)",
            unopenable.display()
        ),
    );

    let daemon = launched
        .as_ref()
        .map(|(child, _waiter)| child.id())
        .expect("the daemon just asserted launched");
    assert_eq!(
        daemon.is_alive(),
        Liveness::Alive,
        "the daemon is not running, so the log failure stopped it after all"
    );
    // The un-handed waiter ends on drop; consume its end.
    drop(launched);
    assert_eq!(
        waiter_threads::waiter_end(id.as_str()),
        waiter_threads::WaiterEnd::Returned
    );
    assert_eq!(
        daemon.is_alive(),
        Liveness::Dead,
        "the daemon outlived the `Child` that owned it"
    );
}

// run_daemon ==========================================================================================================

/// A daemon that runs a PowerShell script.
fn powershell(dir: &Path, id: &str, script: &str) -> DaemonSpec {
    DaemonSpec {
        command: ["powershell", "-NoProfile", "-NonInteractive", "-Command", script]
            .map(String::from)
            .to_vec(),
        ..spec(dir, id)
    }
}

#[skuld::test]
fn no_running_report_when_no_thread_can_be_made() {
    let dir = tempfile::tempdir().expect("a temp dir for the daemon's log");
    let id = TestId::new(PREFIX);
    let stop_bus = Arc::new(StopBus::new());
    let spawned_before = test_hook::spawns();
    let _no_thread = waiter_threads::threads(0);
    let mut running_reports = 0;

    let reported = run_daemon(&spec(dir.path(), id.as_str()), &stop_bus, id.as_str(), || {
        running_reports += 1
    });

    assert_eq!(
        running_reports, 0,
        "RUNNING was reported for a daemon nothing could wait on"
    );
    assert_eq!(reported, Reported::shim_failure(None));
    assert_one_failure(
        &take(id.as_str()),
        id.as_str(),
        &format!(
            r#"make the thread that waits on ["powershell", "-NoProfile", "-NonInteractive", "-Command", "Start-Sleep -Seconds 300"]: {}; it was not spawned, so nothing of this daemon is running"#,
            waiter_threads::NO_THREAD
        ),
    );
    assert_eq!(test_hook::spawns(), spawned_before, "a daemon was spawned");
}

#[skuld::test]
fn no_running_report_when_the_spawn_fails() {
    let dir = tempfile::tempdir().expect("a temp dir for the daemon's log");
    let id = TestId::new(PREFIX);
    let stop_bus = Arc::new(StopBus::new());
    let missing = DaemonSpec {
        command: vec!["goetia-shim-test-no-such-binary".to_string()],
        ..spec(dir.path(), id.as_str())
    };
    let mut running_reports = 0;

    let reported = run_daemon(&missing, &stop_bus, id.as_str(), || running_reports += 1);

    assert_eq!(
        running_reports, 0,
        "RUNNING was reported for a daemon that never started"
    );
    let refusal = start(&mut build_command(&missing, id.as_str())).expect_err("the binary does not exist");
    assert_eq!(reported, Reported::shim_failure(os_error_of(&refusal)));
    assert_one_failure(
        &take(id.as_str()),
        id.as_str(),
        &format!(r#"spawn ["goetia-shim-test-no-such-binary"]: {refusal}"#),
    );
    assert_eq!(waiter_end(id.as_str()), WaiterEnd::Returned);
}

#[skuld::test]
fn running_is_reported_once_and_after_the_spawn() {
    let dir = tempfile::tempdir().expect("a temp dir for the daemon's log");
    let id = TestId::new(PREFIX);
    let stop_bus = Arc::new(StopBus::new());
    let spawned_before = test_hook::spawns();
    let mut running_reports = Vec::new();

    // The stop ends the pass once the report is made; the daemon sleeps until then.
    let reported = run_daemon(&spec(dir.path(), id.as_str()), &stop_bus, id.as_str(), || {
        running_reports.push(test_hook::spawns() - spawned_before);
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
    assert_eq!(waiter_end(id.as_str()), WaiterEnd::Returned);
}

#[skuld::test]
fn a_daemon_that_exits_nonzero_is_one_pass() {
    let dir = tempfile::tempdir().expect("a temp dir for the daemon's log");
    let id = TestId::new(PREFIX);
    let stop_bus = Arc::new(StopBus::new());
    let spawned_before = test_hook::spawns();
    // The shim ignores the restart policy: no respawn.
    let daemon = DaemonSpec {
        restart: Restart::OnFailure,
        restart_delay: None,
        ..powershell(dir.path(), id.as_str(), "exit 7")
    };

    let reported = run_daemon(&daemon, &stop_bus, id.as_str(), || {});

    assert_eq!(reported, Reported::DaemonExit(7));
    assert_eq!(test_hook::spawns(), spawned_before + 1, "the daemon was spawned again");
    assert_eq!(waiter_end(id.as_str()), WaiterEnd::Returned);
}

#[skuld::test]
fn a_daemon_that_exits_zero_is_clean() {
    let dir = tempfile::tempdir().expect("a temp dir for the daemon's log");
    let id = TestId::new(PREFIX);
    let stop_bus = Arc::new(StopBus::new());

    let reported = run_daemon(
        &powershell(dir.path(), id.as_str(), "exit 0"),
        &stop_bus,
        id.as_str(),
        || {},
    );

    assert_eq!(reported, Reported::Clean);
    assert_eq!(waiter_end(id.as_str()), WaiterEnd::Returned);
}

#[skuld::test]
fn an_unkillable_daemon_reports_the_incomplete_stop_failure() {
    let dir = tempfile::tempdir().expect("a temp dir for the daemon's log");
    let id = TestId::new(PREFIX);
    let stop_bus = Arc::new(StopBus::new());
    // Both kills are refused, so the daemon outlives `run_daemon`: it ends when the test creates
    // this file, which is what lets the detached waiter thread be consumed.
    let release = dir.path().join("release");
    let daemon = powershell(
        dir.path(),
        id.as_str(),
        &format!(
            "while (-not (Test-Path -LiteralPath '{}')) {{ Start-Sleep -Milliseconds 100 }}",
            release.display()
        ),
    );

    let reported = {
        let _refused = kills(Kill::Refuse);
        run_daemon(&daemon, &stop_bus, id.as_str(), || {
            stop_bus.request_stop();
        })
    };

    assert_eq!(reported, Reported::incomplete_stop());
    std::fs::write(&release, "").expect("release the daemon");
    assert_eq!(waiter_end(id.as_str()), WaiterEnd::Returned);
    assert_failures(
        &take(id.as_str()),
        id.as_str(),
        &[tree_refused_failure(), fallback_refused_failure()],
    );
}

/// The pass `Wait::Fail` breaks: the waiter's wait failed, so the daemon is torn down and, once
/// confirmed gone, the shim reports the failure with the wait's own OS error.
#[skuld::test]
fn a_failed_wait_is_a_shim_failure_with_the_os_error_and_the_daemon_is_dead() {
    let dir = tempfile::tempdir().expect("a temp dir for the daemon's log");
    let id = TestId::new(PREFIX);
    let stop_bus = Arc::new(StopBus::new());

    let reported = {
        let _failing = waits(Wait::Fail(Some(5)));
        run_daemon(&spec(dir.path(), id.as_str()), &stop_bus, id.as_str(), || {})
    };

    assert_eq!(reported, Reported::shim_failure(Some(5)));
    assert_eq!(
        test_hook::spawned().expect("the daemon was spawned").is_alive(),
        Liveness::Dead,
        "a restartable failure was reported over a daemon still running"
    );
    assert_one_failure(
        &take(id.as_str()),
        id.as_str(),
        &format!("background wait on child: {}", std::io::Error::from_raw_os_error(5)),
    );
    assert_eq!(waiter_end(id.as_str()), WaiterEnd::Returned);
}

#[skuld::test]
fn a_failed_wait_with_no_os_error_is_the_generic_shim_failure() {
    let dir = tempfile::tempdir().expect("a temp dir for the daemon's log");
    let id = TestId::new(PREFIX);
    let stop_bus = Arc::new(StopBus::new());

    let reported = {
        let _failing = waits(Wait::Fail(None));
        run_daemon(&spec(dir.path(), id.as_str()), &stop_bus, id.as_str(), || {})
    };

    assert_eq!(reported, Reported::ShimFailure(ERROR_GEN_FAILURE));
    assert_one_failure(
        &take(id.as_str()),
        id.as_str(),
        &format!("background wait on child: {WAIT_FAILED}"),
    );
    assert_eq!(waiter_end(id.as_str()), WaiterEnd::Returned);
}

/// A daemon whose tree could not be killed must not be reported as a failure SCM restarts: the
/// restart would start a second copy beside it. The exit is decided, so the bus does not accept a
/// later stop either.
#[skuld::test]
fn a_failed_wait_over_an_unkillable_daemon_is_the_incomplete_stop_record() {
    let dir = tempfile::tempdir().expect("a temp dir for the daemon's log");
    let id = TestId::new(PREFIX);
    let stop_bus = Arc::new(StopBus::new());

    let reported = {
        let _failing = waits(Wait::Fail(None));
        let _refused = kills(Kill::Refuse);
        run_daemon(&spec(dir.path(), id.as_str()), &stop_bus, id.as_str(), || {})
    };

    assert_eq!(reported, Reported::incomplete_stop());
    assert!(
        !stop_bus.request_stop(),
        "the bus accepted a stop over a daemon that may still be running"
    );
    assert_eq!(waiter_end(id.as_str()), WaiterEnd::Returned);
    assert_failures(
        &take(id.as_str()),
        id.as_str(),
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
fn a_failing_daemon_whose_tree_cannot_be_terminated_is_the_incomplete_stop_record() {
    let dir = tempfile::tempdir().expect("a temp dir for the daemon's log");
    let id = TestId::new(PREFIX);
    let stop_bus = Arc::new(StopBus::new());

    let reported = {
        let _refused = kills(Kill::Refuse);
        run_daemon(
            &powershell(dir.path(), id.as_str(), "exit 7"),
            &stop_bus,
            id.as_str(),
            || {},
        )
    };

    assert_eq!(reported, Reported::incomplete_stop());
    assert!(
        !stop_bus.request_stop(),
        "the bus accepted a stop over a daemon whose tree may still be running"
    );
    assert_one_failure(
        &take(id.as_str()),
        id.as_str(),
        &format!(
            "kill_tree after the daemon ended: {KILL_REFUSED}; its descendants may still be running — SCM is told the service stopped with Win32 exit code 0, so that it does not start a second copy beside it, and with service-specific exit code 1003 as the record of this failure"
        ),
    );
    assert_eq!(waiter_end(id.as_str()), WaiterEnd::Returned);
}

/// `STOPPED` is never reported over a tree that may still be running, whatever ended the daemon: a
/// `start` that lands before the shim exits would run a second copy beside it. When the tree cannot
/// be terminated, the record is the incomplete stop even for a clean exit.
#[skuld::test]
fn a_clean_exit_whose_tree_cannot_be_terminated_is_the_incomplete_stop_record() {
    let dir = tempfile::tempdir().expect("a temp dir for the daemon's log");
    let id = TestId::new(PREFIX);
    let stop_bus = Arc::new(StopBus::new());

    let reported = {
        let _refused = kills(Kill::Refuse);
        run_daemon(
            &powershell(dir.path(), id.as_str(), "exit 0"),
            &stop_bus,
            id.as_str(),
            || {},
        )
    };

    assert_eq!(reported, Reported::incomplete_stop());
    assert_one_failure(
        &take(id.as_str()),
        id.as_str(),
        &format!(
            "kill_tree after the daemon ended: {KILL_REFUSED}; its descendants may still be running — SCM is told the service stopped with Win32 exit code 0, so that it does not start a second copy beside it, and with service-specific exit code 1003 as the record of this failure"
        ),
    );
    assert_eq!(waiter_end(id.as_str()), WaiterEnd::Returned);
}

/// A clean exit whose tree is terminated is reported as it was.
#[skuld::test]
fn a_clean_exit_whose_tree_is_terminated_is_clean() {
    let dir = tempfile::tempdir().expect("a temp dir for the daemon's log");
    let id = TestId::new(PREFIX);
    let stop_bus = Arc::new(StopBus::new());

    let reported = run_daemon(
        &powershell(dir.path(), id.as_str(), "exit 0"),
        &stop_bus,
        id.as_str(),
        || {},
    );

    assert_eq!(reported, Reported::Clean);
    assert_eq!(take(id.as_str()), Vec::<String>::new());
    assert_eq!(waiter_end(id.as_str()), WaiterEnd::Returned);
}

/// The line logged when `kill_tree` is refused and the direct child's kill is tried next.
fn tree_refused_failure() -> String {
    format!(
        "kill_tree: {KILL_REFUSED}; falling back to killing the direct child only (its own descendants, if any, cannot be reached without a containment mechanism)"
    )
}

/// The line logged when the direct child's kill is refused too.
fn fallback_refused_failure() -> String {
    format!(
        "fallback kill: {KILL_REFUSED}; the daemon was neither killed nor reaped and may still be running — SCM is told the service stopped with Win32 exit code 0, so that it does not start a second copy beside it, and with service-specific exit code 1003 as the record of this failure"
    )
}

// STOP_PENDING --------------------------------------------------------------------------------------------------------

/// `run_daemon` with both reports recorded: how many tree kills had been attempted at each
/// `STOP_PENDING` report, so a test can tell whether teardown had begun.
fn run_recording_stopping(
    daemon: &DaemonSpec,
    stop_bus: &Arc<StopBus>,
    id: &str,
    on_running: impl FnOnce(),
) -> (Reported, Vec<usize>) {
    let before = waiter_threads::tree_kills();
    let mut stopping = Vec::new();
    let reported = super::run_daemon(daemon, stop_bus, id, on_running, || {
        stopping.push(waiter_threads::tree_kills() - before);
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
    let dir = tempfile::tempdir().expect("a temp dir for the daemon's log");
    let id = TestId::new(PREFIX);
    let stop_bus = Arc::new(StopBus::new());
    let before = waiter_threads::tree_kills();
    let mut alive_when_stopping = Vec::new();

    let reported = super::run_daemon(
        &spec(dir.path(), id.as_str()),
        &stop_bus,
        id.as_str(),
        || {
            stop_bus.request_stop();
        },
        || {
            alive_when_stopping.push((
                waiter_threads::tree_kills() - before,
                test_hook::spawned().expect("the daemon was spawned").is_alive(),
            ));
        },
    );

    assert_eq!(reported, Reported::Clean);
    assert_eq!(
        alive_when_stopping,
        [(0, Liveness::Alive)],
        "STOP_PENDING must be reported once, before the teardown begins"
    );
    assert_eq!(waiter_threads::tree_kills() - before, 1);
    assert_eq!(waiter_end(id.as_str()), WaiterEnd::Returned);
}

#[skuld::test]
fn a_daemon_that_exits_reports_stopping_before_its_tree_is_terminated() {
    let dir = tempfile::tempdir().expect("a temp dir for the daemon's log");
    let id = TestId::new(PREFIX);
    let stop_bus = Arc::new(StopBus::new());
    let before = waiter_threads::tree_kills();

    let (reported, stopping) = run_recording_stopping(
        &powershell(dir.path(), id.as_str(), "exit 7"),
        &stop_bus,
        id.as_str(),
        || {},
    );

    assert_eq!(reported, Reported::DaemonExit(7));
    assert_eq!(
        stopping,
        [0],
        "STOP_PENDING must be reported once, before any tree kill"
    );
    assert_eq!(waiter_threads::tree_kills() - before, 1);
    assert_eq!(waiter_end(id.as_str()), WaiterEnd::Returned);
}

#[skuld::test]
fn a_daemon_that_exits_cleanly_reports_stopping_before_its_tree_is_terminated() {
    let dir = tempfile::tempdir().expect("a temp dir for the daemon's log");
    let id = TestId::new(PREFIX);
    let stop_bus = Arc::new(StopBus::new());

    let (reported, stopping) = run_recording_stopping(
        &powershell(dir.path(), id.as_str(), "exit 0"),
        &stop_bus,
        id.as_str(),
        || {},
    );

    assert_eq!(reported, Reported::Clean);
    assert_eq!(stopping, [0]);
    assert_eq!(waiter_end(id.as_str()), WaiterEnd::Returned);
}

#[skuld::test]
fn a_failed_wait_reports_stopping_before_the_daemon_is_killed() {
    let dir = tempfile::tempdir().expect("a temp dir for the daemon's log");
    let id = TestId::new(PREFIX);
    let stop_bus = Arc::new(StopBus::new());

    let (reported, stopping) = {
        let _failing = waits(Wait::Fail(Some(5)));
        run_recording_stopping(&spec(dir.path(), id.as_str()), &stop_bus, id.as_str(), || {})
    };

    assert_eq!(reported, Reported::shim_failure(Some(5)));
    assert_eq!(stopping, [0]);
    assert_one_failure(
        &take(id.as_str()),
        id.as_str(),
        &format!("background wait on child: {}", std::io::Error::from_raw_os_error(5)),
    );
    assert_eq!(waiter_end(id.as_str()), WaiterEnd::Returned);
}

/// A teardown that fails is still a `STOP_PENDING` followed by the incomplete-stop `STOPPED`: the
/// service must not read as up while the shim tries.
#[skuld::test]
fn a_teardown_that_fails_still_reports_stopping_first() {
    let dir = tempfile::tempdir().expect("a temp dir for the daemon's log");
    let id = TestId::new(PREFIX);
    let stop_bus = Arc::new(StopBus::new());

    let (reported, stopping) = {
        let _refused = kills(Kill::Refuse);
        run_recording_stopping(
            &powershell(dir.path(), id.as_str(), "exit 7"),
            &stop_bus,
            id.as_str(),
            || {},
        )
    };

    assert_eq!(reported, Reported::incomplete_stop());
    assert_eq!(stopping, [0]);
    assert_eq!(take(id.as_str()).len(), 1);
    assert_eq!(waiter_end(id.as_str()), WaiterEnd::Returned);
}

#[skuld::test]
fn no_stopping_report_when_the_daemon_never_ran() {
    let dir = tempfile::tempdir().expect("a temp dir for the daemon's log");
    let id = TestId::new(PREFIX);
    let stop_bus = Arc::new(StopBus::new());
    let missing = DaemonSpec {
        command: vec!["goetia-shim-test-no-such-binary".to_string()],
        ..spec(dir.path(), id.as_str())
    };

    let (_, stopping) = run_recording_stopping(&missing, &stop_bus, id.as_str(), || {});

    assert_eq!(
        stopping,
        Vec::<usize>::new(),
        "STOP_PENDING over a service never RUNNING"
    );
    assert_eq!(take(id.as_str()).len(), 1);
    assert_eq!(waiter_end(id.as_str()), WaiterEnd::Returned);
}

#[skuld::test]
fn no_stopping_report_over_an_uncontained_daemon() {
    let dir = tempfile::tempdir().expect("a temp dir for the daemon's log");
    let id = TestId::new(PREFIX);
    let stop_bus = Arc::new(StopBus::new());

    let (_, stopping) = {
        let _degraded = test_hook::containment_is(Containment::TreeWalk);
        run_recording_stopping(&spec(dir.path(), id.as_str()), &stop_bus, id.as_str(), || {})
    };

    assert_eq!(
        stopping,
        Vec::<usize>::new(),
        "STOP_PENDING over a service never RUNNING"
    );
    assert_eq!(take(id.as_str()).len(), 1);
    assert_eq!(waiter_end(id.as_str()), WaiterEnd::Returned);
}

/// The line logged when the daemon is not job-contained, whatever the kill then does.
const NOT_JOB_CONTAINED: &str = r#"the daemon ["powershell", "-NoProfile", "-NonInteractive", "-Command", "Start-Sleep -Seconds 300"] was contained by process-tree walk, not a job object: `type: simple` needs a job object to terminate its whole tree. The daemon ran from its launch, and no kill of it shows that its descendants are gone, so it is reported as possibly still running"#;

/// `type: simple` needs a Job Object to terminate the daemon's whole tree. A daemon contained any
/// other way is killed and the start fails with nothing reported RUNNING. No kill proves a
/// non-job tree gone, so the record is the incomplete stop (never restartable), whatever the kill
/// returned.
#[skuld::test]
fn a_daemon_without_job_object_containment_is_killed_and_reported_possibly_running() {
    let dir = tempfile::tempdir().expect("a temp dir for the daemon's log");
    let id = TestId::new(PREFIX);
    let stop_bus = Arc::new(StopBus::new());
    let mut running_reports = 0;

    let reported = {
        let _degraded = test_hook::containment_is(Containment::TreeWalk);
        run_daemon(&spec(dir.path(), id.as_str()), &stop_bus, id.as_str(), || {
            running_reports += 1
        })
    };

    assert_eq!(reported, Reported::incomplete_stop());
    assert_eq!(running_reports, 0, "RUNNING was reported for an uncontained daemon");
    assert_eq!(
        test_hook::spawned().expect("the daemon was spawned").is_alive(),
        Liveness::Dead,
        "the uncontained daemon was left running"
    );
    assert_one_failure(&take(id.as_str()), id.as_str(), NOT_JOB_CONTAINED);
    assert_eq!(waiter_end(id.as_str()), WaiterEnd::Returned);
}

/// The same when `kill_tree` fails and only the direct child is killed: the descendants were never
/// reached, so this is no more restartable than a kill that reports success.
#[skuld::test]
fn an_uncontained_daemon_whose_tree_kill_failed_is_reported_possibly_running() {
    let dir = tempfile::tempdir().expect("a temp dir for the daemon's log");
    let id = TestId::new(PREFIX);
    let stop_bus = Arc::new(StopBus::new());

    let reported = {
        let _degraded = test_hook::containment_is(Containment::TreeWalk);
        let _tree_refused = kills(Kill::RefuseTree);
        run_daemon(&spec(dir.path(), id.as_str()), &stop_bus, id.as_str(), || {})
    };

    assert_eq!(reported, Reported::incomplete_stop());
    assert_eq!(waiter_end(id.as_str()), WaiterEnd::Returned);
    assert_failures(
        &take(id.as_str()),
        id.as_str(),
        &[NOT_JOB_CONTAINED.to_string(), tree_refused_failure()],
    );
}

/// The same, when the uncontained daemon cannot be killed either: it may be running.
#[skuld::test]
fn an_uncontained_daemon_that_cannot_be_killed_is_the_incomplete_stop_record() {
    let dir = tempfile::tempdir().expect("a temp dir for the daemon's log");
    let id = TestId::new(PREFIX);
    let stop_bus = Arc::new(StopBus::new());

    let reported = {
        let _degraded = test_hook::containment_is(Containment::TreeWalk);
        let _refused = kills(Kill::Refuse);
        run_daemon(&spec(dir.path(), id.as_str()), &stop_bus, id.as_str(), || {})
    };

    assert_eq!(reported, Reported::incomplete_stop());
    assert_eq!(waiter_end(id.as_str()), WaiterEnd::Returned);
    assert_failures(
        &take(id.as_str()),
        id.as_str(),
        &[
            NOT_JOB_CONTAINED.to_string(),
            tree_refused_failure(),
            fallback_refused_failure(),
        ],
    );
    // A real refusal makes `Child::drop` wait with no bound for a daemon it cannot kill, before
    // anything is reported, so `run_daemon` must let it go undropped. The hook refuses only the
    // shim's own kills: a drop would have killed it by here.
    let daemon = cosca::Process::from_id(test_hook::spawned().expect("the daemon was spawned"));
    assert_eq!(
        daemon.is_alive(),
        Liveness::Alive,
        "the shim dropped a daemon it could not kill"
    );
    daemon.kill().expect("kill the daemon the shim let go");
    daemon.wait().expect("wait for the daemon the shim let go");
}

/// The status query after `ChildExited` can fail; the exit is then a shim failure, not a daemon code.
#[skuld::test]
fn an_exit_status_that_cannot_be_read_is_a_shim_failure() {
    let dir = tempfile::tempdir().expect("a temp dir for the daemon's log");
    let id = TestId::new(PREFIX);
    let stop_bus = Arc::new(StopBus::new());

    let reported = {
        let _unreadable = test_hook::exit_status_fails();
        run_daemon(
            &powershell(dir.path(), id.as_str(), "exit 7"),
            &stop_bus,
            id.as_str(),
            || {},
        )
    };

    assert_eq!(reported, Reported::ShimFailure(ERROR_GEN_FAILURE));
    assert_one_failure(
        &take(id.as_str()),
        id.as_str(),
        &format!("query child exit status: {}", test_hook::EXIT_STATUS_UNREADABLE),
    );
    assert_eq!(waiter_end(id.as_str()), WaiterEnd::Returned);
}

/// The exit is decided once: a stop after the failure is settled does not turn it into a clean stop.
/// SCM acknowledges every `STOP` it delivers whatever the handler answers, so the handler answers
/// `NO_ERROR`.
#[skuld::test]
fn a_stop_after_a_daemon_failure_was_decided_leaves_the_failure() {
    let dir = tempfile::tempdir().expect("a temp dir for the daemon's log");
    let id = TestId::new(PREFIX);
    let stop_bus = Arc::new(StopBus::new());

    let reported = run_daemon(
        &powershell(dir.path(), id.as_str(), "exit 7"),
        &stop_bus,
        id.as_str(),
        || {},
    );

    assert_eq!(reported, Reported::DaemonExit(7));
    assert!(
        !stop_bus.request_stop(),
        "a stop was accepted after the failure was decided"
    );
    assert_eq!(handle_control(&stop_bus, SERVICE_CONTROL_STOP), NO_ERROR);
    assert_eq!(stop_bus.settle(Reported::DaemonExit(7)), Reported::DaemonExit(7));
    assert_eq!(waiter_end(id.as_str()), WaiterEnd::Returned);
}

/// SCM may deliver a control after `STOP`: the handler and the context `leak_handler_context` makes
/// still answer it. That `register_control_handler` passes that context, and not `windows-service`
/// 0.8.1's `register`, is not covered here, or anywhere: `register` frees its closure after any
/// Stop, Shutdown or Preshutdown whatever the handler returned (P16 observed it) while the
/// registration stays live, and only a real SCM shows that. `clippy.toml` forbids the call.
#[skuld::test]
fn a_control_after_a_stop_still_reaches_the_bus() {
    let stop_bus = Arc::new(StopBus::new());
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
fn a_stop_accepted_before_a_daemon_failure_was_decided_is_clean() {
    let dir = tempfile::tempdir().expect("a temp dir for the daemon's log");
    let id = TestId::new(PREFIX);
    let stop_bus = Arc::new(StopBus::new());

    let reported = run_daemon(
        &powershell(dir.path(), id.as_str(), "exit 7"),
        &stop_bus,
        id.as_str(),
        || assert_eq!(handle_control(&stop_bus, SERVICE_CONTROL_STOP), NO_ERROR),
    );

    assert_eq!(reported, Reported::Clean);
    assert_eq!(waiter_end(id.as_str()), WaiterEnd::Returned);
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
        os_error_of(&cosca::error::Error::Io(std::io::Error::from_raw_os_error(5))),
        Some(5)
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

// Services registered by an earlier version ---------------------------------------------------------------------------

fn spec_restarting(restart: Restart) -> DaemonSpec {
    DaemonSpec {
        restart,
        ..spec(Path::new("unused"), "earlier-registration")
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
    let id = TestId::new(PREFIX);

    log_version_skew(id.as_str(), &spec_restarting(Restart::OnFailure), "0.1.0", "0.2.0");

    assert!(take(id.as_str()).is_empty(), "the notice was logged as a failure");
    let warnings = take_warnings(id.as_str());
    let [line] = warnings.as_slice() else {
        panic!("expected exactly one warning, got {warnings:?}");
    };
    assert!(line.contains("registered by goetia 0.1.0"), "{line}");
}

#[skuld::test]
fn nothing_is_logged_for_a_service_the_shim_has_no_notice_about() {
    let id = TestId::new(PREFIX);

    log_version_skew(id.as_str(), &spec_restarting(Restart::Never), "0.1.0", "0.2.0");
    log_version_skew(id.as_str(), &spec_restarting(Restart::OnFailure), "0.2.0", "0.2.0");

    assert!(take(id.as_str()).is_empty());
    assert!(take_warnings(id.as_str()).is_empty());
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
