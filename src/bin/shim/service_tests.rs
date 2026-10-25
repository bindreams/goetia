//! What `launch` and `run_daemon` do, which is the whole of this module that can be tested without
//! a real SCM. `launch` makes the thread that will wait on the daemon before the daemon, so the OS
//! refusing that thread is a start failure with nothing launched rather than an orphaned daemon
//! nothing waits on. `run_daemon` is one pass: it reports running after the spawn, never after a
//! failed launch, and turns the daemon's end into what SCM is told.

use std::collections::BTreeMap;
use std::path::Path;

use cosca::identity::Liveness;
use goetia::backend::scm::exit_status::{
    ERROR_CAN_NOT_COMPLETE, ERROR_GEN_FAILURE, ERROR_SERVICE_SPECIFIC_ERROR, Reported,
};
use goetia::spec::{Id, Kind, Restart, User};
use windows_sys::Win32::System::Services::SERVICE_STOPPED;

use super::*;
use crate::logging::test_hook::{take, take_warnings};
use crate::stop_bus::test_hook as waiter_threads;
use crate::stop_bus::test_hook::{KILL_REFUSED, Kill, WaiterEnd, kills, waiter_end};
use crate::test_support::{TestId, assert_one_failure};

const PREFIX: &str = "goetia-shim-launch-test";

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

    let reported = run_daemon(
        &spec(dir.path(), id.as_str()),
        &stop_bus,
        id.as_str(),
        |_| Ok(()),
        || running_reports += 1,
        |_| {},
    );

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

    let reported = run_daemon(
        &missing,
        &stop_bus,
        id.as_str(),
        |_| Ok(()),
        || running_reports += 1,
        |_| {},
    );

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
    let reported = run_daemon(
        &spec(dir.path(), id.as_str()),
        &stop_bus,
        id.as_str(),
        |_| Ok(()),
        || {
            running_reports.push(test_hook::spawns() - spawned_before);
            stop_bus.request_stop();
        },
        |_| {},
    );

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

    let reported = run_daemon(&daemon, &stop_bus, id.as_str(), |_| Ok(()), || {}, |_| {});

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
        |_| Ok(()),
        || {},
        |_| {},
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

    let mut launched = Vec::new();
    let mut gone = Vec::new();
    let reported = {
        let _refused = kills(Kill::Refuse);
        run_daemon(
            &daemon,
            &stop_bus,
            id.as_str(),
            |daemon| {
                launched.push(daemon);
                Ok(())
            },
            || stop_bus.request_stop(),
            |daemon| gone.push(daemon),
        )
    };

    assert_eq!(reported, Reported::incomplete_stop());
    // The identity recorded at the launch is the daemon's own, it is the process that outlived the
    // stop, and it is not forgotten: the record is what tells goetia a daemon survived.
    assert_eq!(launched.len(), 1, "{launched:?}");
    assert_eq!(launched[0].is_alive(), Liveness::Alive, "{launched:?}");
    assert!(gone.is_empty(), "a daemon that still runs was forgotten: {gone:?}");
    std::fs::write(&release, "").expect("release the daemon");
    assert_eq!(waiter_end(id.as_str()), WaiterEnd::Returned);
    let lines = take(id.as_str());
    assert_eq!(lines.len(), 2, "both refused kills must be journalled: {lines:?}");
    assert_one_failure(
        &lines[..1],
        id.as_str(),
        &format!(
            "kill_tree on stop: {KILL_REFUSED}; falling back to killing the direct child only (its own descendants, if any, cannot be reached without a containment mechanism)"
        ),
    );
    assert_one_failure(
        &lines[1..],
        id.as_str(),
        &format!(
            "fallback kill on stop: {KILL_REFUSED}; the daemon was neither killed nor reaped and may still be running — SCM is told the service stopped with Win32 exit code 0, so that it does not start a second copy beside it, and with service-specific exit code 1003 as the record of this failure"
        ),
    );
}

// Pure helpers --------------------------------------------------------------------------------------------------------

#[skuld::test]
fn a_stop_wins_over_a_daemon_that_failed_at_the_same_moment() {
    assert_eq!(exit_report(true, Some(7)), Reported::Clean);
    assert_eq!(exit_report(true, None), Reported::Clean);
}

#[skuld::test]
fn an_exit_code_becomes_a_daemon_report() {
    assert_eq!(exit_report(false, Some(0)), Reported::Clean);
    assert_eq!(exit_report(false, Some(7)), Reported::DaemonExit(7));
    assert_eq!(exit_report(false, Some(-1073741819)), Reported::DaemonExit(0xC000_0005));
}

#[skuld::test]
fn an_unobtainable_exit_status_is_a_shim_failure() {
    assert_eq!(
        exit_report(false, None),
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
    let notice = version_skew_notice(&spec_restarting(Restart::OnFailure), "0.1.0", "0.2.0");

    assert_eq!(
        notice.as_deref(),
        Some(
            r#"daemon "goetia-shim launch test" was registered by goetia 0.1.0 and this goetia-shim is 0.2.0: its SCM recovery actions may be missing, so it may not restart after a failure. Run `goetia daemon install` to apply them"#
        )
    );
}

#[skuld::test]
fn an_earlier_registration_under_always_also_names_the_manifest_change() {
    let notice = version_skew_notice(&spec_restarting(Restart::Always), "0.1.0", "0.2.0");

    assert_eq!(
        notice.as_deref(),
        Some(
            r#"daemon "goetia-shim launch test" was registered by goetia 0.1.0 and this goetia-shim is 0.2.0: its SCM recovery actions may be missing, so it may not restart after a failure. Run `goetia daemon install` to apply them, after setting `backend-specific: scm: { restart: on-failure }` in the manifest: `restart: always` is not supported for scm"#
        )
    );
}

#[skuld::test]
fn a_registration_by_released_0_1_0_warns_under_this_build() {
    let notice = version_skew_notice(&spec_restarting(Restart::OnFailure), "0.1.0", goetia::version());

    assert!(notice.is_some(), "a service registered by 0.1.0 must get the warning");
}

#[skuld::test]
fn no_notice_for_a_daemon_that_is_never_restarted() {
    assert_eq!(
        version_skew_notice(&spec_restarting(Restart::Never), "0.1.0", "0.2.0"),
        None
    );
}

#[skuld::test]
fn no_notice_when_the_shim_is_the_version_that_registered_the_service() {
    for restart in [Restart::Never, Restart::OnFailure, Restart::Always] {
        assert_eq!(
            version_skew_notice(&spec_restarting(restart), "0.2.0", "0.2.0"),
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

// the record of the daemon ============================================================================================

#[skuld::test]
fn the_daemon_is_recorded_before_the_service_is_reported_running_and_forgotten_after_it_ended() {
    let dir = tempfile::tempdir().expect("a temp dir for the daemon's log");
    let id = TestId::new(PREFIX);
    let stop_bus = Arc::new(StopBus::new());
    let steps = std::cell::RefCell::new(Vec::new());
    let mut launched = None;

    let reported = run_daemon(
        &spec(dir.path(), id.as_str()),
        &stop_bus,
        id.as_str(),
        |daemon| {
            steps.borrow_mut().push("launched");
            launched = Some(daemon);
            Ok(())
        },
        || {
            steps.borrow_mut().push("running");
            stop_bus.request_stop();
        },
        |daemon| {
            // Forgotten only once it is dead: the record is what says it is not running.
            assert_eq!(daemon.is_alive(), Liveness::Dead);
            steps.borrow_mut().push("gone");
        },
    );

    assert_eq!(reported, Reported::Clean);
    assert_eq!(*steps.borrow(), ["launched", "running", "gone"]);
}

#[skuld::test]
fn a_daemon_that_exits_on_its_own_is_forgotten_whatever_its_exit_code() {
    for (script, expected) in [("exit 0", Reported::Clean), ("exit 7", Reported::DaemonExit(7))] {
        let dir = tempfile::tempdir().expect("a temp dir for the daemon's log");
        let id = TestId::new(PREFIX);
        let stop_bus = Arc::new(StopBus::new());
        let mut gone = Vec::new();

        let reported = run_daemon(
            &powershell(dir.path(), id.as_str(), script),
            &stop_bus,
            id.as_str(),
            |_| Ok(()),
            || {},
            |daemon| gone.push(daemon),
        );

        assert_eq!(reported, expected, "{script}");
        assert_eq!(gone.len(), 1, "{script}: {gone:?}");
    }
}

#[skuld::test]
fn a_daemon_that_cannot_be_recorded_is_killed_and_the_service_fails_without_ever_running() {
    let dir = tempfile::tempdir().expect("a temp dir for the daemon's log");
    let id = TestId::new(PREFIX);
    let stop_bus = Arc::new(StopBus::new());
    let mut launched = None;
    let mut running_reports = 0;
    let mut gone = 0;

    let reported = run_daemon(
        &spec(dir.path(), id.as_str()),
        &stop_bus,
        id.as_str(),
        |daemon| {
            launched = Some(daemon);
            Err(Some(112))
        },
        || running_reports += 1,
        |_| gone += 1,
    );

    assert_eq!(reported, Reported::shim_failure(Some(112)));
    assert_eq!(
        running_reports, 0,
        "RUNNING was reported for a daemon goetia cannot see"
    );
    assert_eq!(gone, 0, "a daemon that was never recorded has nothing to forget");
    let daemon = launched.expect("the daemon was launched");
    assert_eq!(
        daemon.is_alive(),
        Liveness::Dead,
        "an unrecorded daemon was left running"
    );
    assert_eq!(waiter_end(id.as_str()), WaiterEnd::Returned);
}

#[skuld::test]
fn a_daemon_that_cannot_be_recorded_nor_killed_is_the_incomplete_stop() {
    let dir = tempfile::tempdir().expect("a temp dir for the daemon's log");
    let id = TestId::new(PREFIX);
    let stop_bus = Arc::new(StopBus::new());
    let release = dir.path().join("release");
    let daemon = powershell(
        dir.path(),
        id.as_str(),
        &format!(
            "while (-not (Test-Path -LiteralPath '{}')) {{ Start-Sleep -Milliseconds 100 }}",
            release.display()
        ),
    );
    let mut launched = None;

    let reported = {
        let _refused = kills(Kill::Refuse);
        run_daemon(
            &daemon,
            &stop_bus,
            id.as_str(),
            |daemon| {
                launched = Some(daemon);
                Err(None)
            },
            || {},
            |_| {},
        )
    };

    // Nothing records the daemon and nothing kills it: the status record of the incomplete stop is
    // all goetia has, and it reads that as a daemon that may be running.
    assert_eq!(reported, Reported::incomplete_stop());
    assert_eq!(launched.expect("launched").is_alive(), Liveness::Alive);
    std::fs::write(&release, "").expect("release the daemon");
    assert_eq!(waiter_end(id.as_str()), WaiterEnd::Returned);
}
