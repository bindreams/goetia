//! Restarts of `type: simple` daemons. The shim spawns the daemon once and reports its exit; SCM's
//! recovery actions (`restart-delay` is the action delay) start a new shim, which reads the
//! installed spec afresh. Every wait here is for a positive event: the daemon connecting back, or a
//! process exiting. Nothing proves that something did not happen by letting time pass.
//!
//! Not testable here: an incomplete stop (both kills refused). The refusal is a unit-test hook in
//! the shim, so `service_tests.rs` covers it.

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use goetia::backend::scm::exit_status::{ERROR_SERVICE_SPECIFIC_ERROR, RecordedExit};
use goetia::backend::scm::manager::ScmManager;
use goetia::decide::Outcome;
use goetia::manager::{Budget, ServiceManager as _};
use goetia::spec::{DaemonSpec, Restart};
use windows_service::service::{ServiceExitCode, ServiceState};

use crate::common::{
    ShimProcess, fallback_log, fixture_command, kill_process, mk_spec_full, recorded_exit, scm_status,
};
use crate::support::{self, ConnectBack, ELEVATED, ServiceGuard};

const ONE_SECOND: Option<Duration> = Some(Duration::from_secs(1));

/// A version no installed goetia will ever be, standing in for the one that registered a service
/// before this shim existed.
const EARLIER_VERSION: &str = "0.0.0";

fn on_failure(id: &str, command: Vec<String>, restart_delay: Option<Duration>) -> DaemonSpec {
    mk_spec_full(
        id,
        command,
        None,
        BTreeMap::new(),
        Restart::OnFailure,
        restart_delay,
        None,
    )
}

// SCM restarts ========================================================================================================

#[skuld::test(requires = [support::elevated], labels = [ELEVATED])]
fn a_restart_after_an_update_runs_the_updated_spec() {
    let mgr = ScmManager::new();
    let id = support::random_test_id();
    let _guard = ServiceGuard::new(&id);
    let old = ConnectBack::listen();
    let new = ConnectBack::listen();

    let before = on_failure(&id, fixture_command("report", &[&old.port().to_string()]), ONE_SECOND);
    mgr.install(&before, false).expect("install");
    mgr.start(&before.id, Budget::DEFAULT).expect("start");
    let old_daemon = old.accept_line("the first daemon to report its pid");

    let after = on_failure(&id, fixture_command("report", &[&new.port().to_string()]), ONE_SECOND);
    let outcome = mgr.install(&after, false).expect("re-install with a changed command");
    assert!(matches!(outcome, Outcome::Update { .. }), "{outcome:?}");

    // The old shim holds the old spec in memory. If it restarted the daemon itself, the daemon of
    // the old spec would come back and this connection would never arrive.
    kill_process(old_daemon.trim().parse().expect("the daemon reports its pid"));
    new.accept("the restarted daemon of the updated spec to report in");
}

#[skuld::test(requires = [support::elevated], labels = [ELEVATED])]
fn a_killed_shim_is_restarted_by_scm() {
    let mgr = ScmManager::new();
    let id = support::random_test_id();
    let _guard = ServiceGuard::new(&id);
    let reported = ConnectBack::listen();

    let spec = on_failure(
        &id,
        fixture_command("report", &[&reported.port().to_string()]),
        ONE_SECOND,
    );
    mgr.install(&spec, false).expect("install");
    mgr.start(&spec.id, Budget::DEFAULT).expect("start");
    reported.accept("the first daemon to report in");
    // Held for the rest of the test, so the new shim cannot be given the same pid.
    let first_shim = ShimProcess::attach(&id);

    // Killed from outside, so it reports nothing: SCM sees a crash.
    kill_process(first_shim.pid());
    reported.accept("a daemon restarted by SCM after its shim was killed");

    // The daemon is a child of the new shim, so SCM already knows that process.
    let after = scm_status(&id);
    let second_shim = after.process_id.expect("SCM has a process for the restarted service");
    assert_ne!(second_shim, first_shim.pid(), "{after:?}");
    assert!(
        matches!(after.current_state, ServiceState::Running | ServiceState::StartPending),
        "{after:?}"
    );
}

/// A killed shim is a crash to SCM, which restarts the service whatever the shim reported. The
/// restarted service runs a new shim, so the daemon comes back as a different process.
#[skuld::test(requires = [support::elevated], labels = [ELEVATED])]
fn a_killed_on_failure_simple_daemon_comes_back_with_a_new_pid() {
    let mgr = ScmManager::new();
    let id = support::random_test_id();
    let _guard = ServiceGuard::new(&id);
    let started = ConnectBack::listen();
    let spec = on_failure(
        &id,
        fixture_command("report", &[&started.port().to_string()]),
        ONE_SECOND,
    );
    mgr.install(&spec, false).expect("install");

    support::kill::a_killed_daemon_comes_back_with_a_new_pid(&mgr, &spec.id, &started);
}

#[skuld::test(requires = [support::elevated], labels = [ELEVATED])]
fn on_failure_restarts_are_paced_new_shims() {
    let mgr = ScmManager::new();
    let id = support::random_test_id();
    let _guard = ServiceGuard::new(&id);
    let reported = ConnectBack::listen();

    // No `restart-delay`: the end-to-end wiring of `generate::DEFAULT_RESTART_DELAY` (1s).
    let spec = on_failure(
        &id,
        fixture_command("exit-on-close", &[&reported.port().to_string(), "1"]),
        None,
    );
    mgr.install(&spec, false).expect("install");
    mgr.start(&spec.id, Budget::DEFAULT).expect("start");

    // Held until the end, so no shim can be given the pid of an earlier one.
    let mut shims: Vec<ShimProcess> = Vec::new();
    let mut released_at = None;
    let mut running = None;
    // The first run plus two restarts: a single recovery action repeats for every later failure.
    for _ in 0..3 {
        let stream = reported.accept_stream("the daemon to connect and wait for its release");
        if let Some(released_at) = released_at {
            let elapsed: Duration = Instant::now().duration_since(released_at);
            // A pacing floor with jitter allowed for (the delay is 1s), not a synchronization bound.
            assert!(
                elapsed >= Duration::from_millis(700),
                "restarted after only {elapsed:?}, faster than the 1s restart-delay default allows"
            );
        }
        let shim = ShimProcess::attach(&id);
        assert!(
            shims.iter().all(|earlier| earlier.pid() != shim.pid()),
            "shim {} hosted two runs; every restart must be a new shim process",
            shim.pid()
        );
        shims.push(shim);

        released_at = Some(Instant::now());
        // The last run stays up, so the test ends with a commanded stop rather than a pending restart.
        if shims.len() == 3 {
            running = Some(stream);
        }
    }

    mgr.stop(&spec.id, Budget::DEFAULT).expect("stop");
    drop(running);
}

// What is reported ====================================================================================================

/// Runs `exit-on-close` under `restart: never` (so SCM does nothing after the report), lets the
/// daemon exit with `child_code`, and returns the exit code SCM recorded once the shim is gone.
fn recorded_exit_code(child_code: i32) -> ServiceExitCode {
    let reported = ConnectBack::listen();
    let mgr = ScmManager::new();
    let id = support::random_test_id();
    let _guard = ServiceGuard::new(&id);

    let spec = mk_spec_full(
        &id,
        fixture_command(
            "exit-on-close",
            &[&reported.port().to_string(), &child_code.to_string()],
        ),
        None,
        BTreeMap::new(),
        Restart::Never,
        None,
        None,
    );
    mgr.install(&spec, false).expect("install");
    mgr.start(&spec.id, Budget::DEFAULT).expect("start");
    let stream = reported.accept_stream("the fixture to connect and wait for the test to close its end");
    let shim = ShimProcess::attach(&id);

    drop(stream);
    shim.wait_for_exit();

    let stopped = scm_status(&id);
    assert_eq!(stopped.current_state, ServiceState::Stopped, "{stopped:?}");
    stopped.exit_code
}

#[skuld::test(requires = [support::elevated], labels = [ELEVATED])]
fn a_clean_daemon_exit_is_recorded_as_win32_zero() {
    assert_eq!(recorded_exit_code(0), ServiceExitCode::Win32(0));
}

/// Any daemon exit code, including 3 and 7, is recorded as service-specific.
#[skuld::test(requires = [support::elevated], labels = [ELEVATED])]
fn a_daemon_exit_code_is_recorded_as_service_specific_whatever_its_value() {
    assert_eq!(recorded_exit_code(3), ServiceExitCode::ServiceSpecific(3));
    assert_eq!(recorded_exit_code(7), ServiceExitCode::ServiceSpecific(7));
}

#[skuld::test(requires = [support::elevated], labels = [ELEVATED])]
fn a_daemon_that_cannot_be_spawned_fails_start_under_every_policy() {
    let mgr = ScmManager::new();
    let missing = vec![r"C:\goetia-test-no-such-dir\no-such-binary.exe".to_string()];
    // A restart delay that cannot elapse during the test, so the record read below is the first
    // failure's and not a restarted run's.
    let hour = Some(Duration::from_secs(3600));

    for (restart, restart_delay) in [(Restart::Never, None), (Restart::OnFailure, hour)] {
        let id = support::random_test_id();
        let _guard = ServiceGuard::new(&id);
        let spec = mk_spec_full(
            &id,
            missing.clone(),
            None,
            BTreeMap::new(),
            restart,
            restart_delay,
            None,
        );
        mgr.install(&spec, false).expect("install");

        mgr.start(&spec.id, Budget::DEFAULT)
            .expect_err("a daemon that cannot be spawned must fail the start");

        let status = scm_status(&id);
        assert_eq!(status.current_state, ServiceState::Stopped, "{restart:?}: {status:?}");
        match status.exit_code {
            ServiceExitCode::Win32(code) => assert!(
                code != 0 && code != ERROR_SERVICE_SPECIFIC_ERROR,
                "{restart:?}: a failed spawn must be a non-zero Win32 code that is not 1066, got {code}: {status:?}"
            ),
            other => panic!("{restart:?}: a failed spawn is a shim failure, a Win32 code; got {other:?}"),
        }
    }
}

/// `start` confirms that the shim launched the daemon, so a daemon that exits at once is a started
/// service whether the wait sees `RUNNING` or the `STOPPED` after it.
#[skuld::test(requires = [support::elevated], labels = [ELEVATED])]
fn start_confirms_the_launch_of_a_daemon_that_exits_at_once() {
    let mgr = ScmManager::new();
    // A delay that cannot elapse during the test, so `on-failure` does not restart the daemon.
    let hour = Some(Duration::from_secs(3600));

    for (restart, code) in [(Restart::Never, 7), (Restart::OnFailure, 7), (Restart::Never, 0)] {
        let id = support::random_test_id();
        let _guard = ServiceGuard::new(&id);
        let reported = ConnectBack::listen();
        let spec = mk_spec_full(
            &id,
            fixture_command("exit", &[&reported.port().to_string(), &code.to_string()]),
            None,
            BTreeMap::new(),
            restart,
            hour,
            None,
        );
        mgr.install(&spec, false).expect("install");

        mgr.start(&spec.id, Budget::DEFAULT)
            .unwrap_or_else(|e| panic!("{restart:?}, exit {code}: a launched daemon must make start succeed: {e}"));
        reported.accept("the daemon to have run");
    }
}

#[skuld::test(requires = [support::elevated], labels = [ELEVATED])]
fn a_commanded_stop_reports_win32_zero() {
    let mgr = ScmManager::new();
    let id = support::random_test_id();
    let _guard = ServiceGuard::new(&id);
    let reported = ConnectBack::listen();

    // `on-failure`, so a stop wrongly reported as a failure would also queue a restart.
    let spec = on_failure(
        &id,
        fixture_command("report", &[&reported.port().to_string()]),
        ONE_SECOND,
    );
    mgr.install(&spec, false).expect("install");
    mgr.start(&spec.id, Budget::DEFAULT).expect("start");
    reported.accept("the daemon to report in");

    mgr.stop(&spec.id, Budget::DEFAULT).expect("stop");

    let stopped = scm_status(&id);
    assert_eq!(stopped.current_state, ServiceState::Stopped, "{stopped:?}");
    assert_eq!(stopped.exit_code, ServiceExitCode::Win32(0), "{stopped:?}");
    // Win32 0 alone would also be an incomplete stop, which adds service-specific 1003.
    assert_eq!(
        recorded_exit(&id),
        RecordedExit { win32: 0, specific: 0 },
        "{stopped:?}"
    );
}

// A stop that meets a restart SCM has queued ==========================================================================

/// Installs an `on-failure` daemon that exits 7 once the test closes its connection, starts it, and
/// returns once the shim has reported the failure and gone, so SCM holds a `STOPPED` with a
/// non-zero code and a restart queued after `restart_delay`. The listener sees every run.
fn fail_once(mgr: &ScmManager, id: &str, restart_delay: Duration) -> (ConnectBack, DaemonSpec) {
    let reported = ConnectBack::listen();
    let spec = on_failure(
        id,
        fixture_command("exit-on-close", &[&reported.port().to_string(), "7"]),
        Some(restart_delay),
    );
    mgr.install(&spec, false).expect("install");
    mgr.start(&spec.id, Budget::DEFAULT).expect("start");
    let stream = reported.accept_stream("the first run to connect and wait for the test to close its end");
    let shim = ShimProcess::attach(id);

    drop(stream);
    shim.wait_for_exit();
    (reported, spec)
}

/// `stop` refuses a pending restart at once, under every budget (`--no-timeout` included): nothing
/// cancels it, so there is nothing to wait for. The delay is an hour, so the restart cannot fire
/// during the test and the record read below is the first failure's.
#[skuld::test(requires = [support::elevated], labels = [ELEVATED])]
fn a_stop_during_the_restart_delay_is_refused_under_every_budget() {
    let mgr = ScmManager::new();
    let id = support::random_test_id();
    let _guard = ServiceGuard::new(&id);
    let (_reported, spec) = fail_once(&mgr, &id, Duration::from_secs(3600));
    let failed = RecordedExit {
        win32: ERROR_SERVICE_SPECIFIC_ERROR,
        specific: 7,
    };
    assert_eq!(recorded_exit(&id), failed, "the daemon's failure is what SCM holds");

    for budget in [
        Budget::Immediate,
        Budget::Bounded(Duration::from_secs(1)),
        Budget::Unbounded,
    ] {
        let e = mgr
            .stop(&spec.id, budget)
            .expect_err("a stop cannot cancel a pending restart");
        assert!(matches!(e, goetia::Error::RestartPending { .. }), "{budget:?}: {e:?}");
        // Nothing was commanded: the record is still the daemon's failure.
        assert_eq!(recorded_exit(&id), failed, "{budget:?}");
    }
}

/// `uninstall` does not wait for a queued restart: deleting the service cancels it. The delay is an
/// hour, so a stop that waited for the restart would not return, and the service would not be gone.
/// After the uninstall returns, `sc` finds no record (1060, `ERROR_SERVICE_DOES_NOT_EXIST`), so the
/// restart has nothing to start.
#[skuld::test(requires = [support::elevated], labels = [ELEVATED])]
fn uninstall_during_the_restart_delay_deletes_the_service_without_waiting_for_the_restart() {
    let mgr = ScmManager::new();
    let id = support::random_test_id();
    let _guard = ServiceGuard::new(&id);
    let (_reported, spec) = fail_once(&mgr, &id, Duration::from_secs(3600));
    let failed = RecordedExit {
        win32: ERROR_SERVICE_SPECIFIC_ERROR,
        specific: 7,
    };
    assert_eq!(recorded_exit(&id), failed, "the daemon's failure is what SCM holds");

    mgr.uninstall(&spec.id).expect("uninstall");

    let query = support::cmd::run("sc.exe", &["query", &id]);
    assert_eq!(query.code, Some(1060), "the service should no longer exist:\n{query}");
}

// Services registered by an earlier version ===========================================================================

fn parameters_key(id: &str) -> winreg::RegKey {
    use winreg::RegKey;
    use winreg::enums::{HKEY_LOCAL_MACHINE, KEY_READ, KEY_WRITE};

    let path = format!(r"SYSTEM\CurrentControlSet\Services\{id}\Parameters");
    RegKey::predef(HKEY_LOCAL_MACHINE)
        .open_subkey_with_flags(&path, KEY_READ | KEY_WRITE)
        .unwrap_or_else(|e| panic!("open {path}: {e}"))
}

/// Replaces `from` with `to` in the blob JSON under `Parameters\Spec`, which must hold `from`.
fn replace_in_blob(id: &str, from: &str, to: &str) {
    let key = parameters_key(id);
    let encoded: String = key.get_value("Spec").expect("read Spec");
    let json = String::from_utf8(BASE64.decode(encoded).expect("Spec is base64")).expect("Spec is UTF-8 JSON");
    assert!(json.contains(from), "no {from} in the blob: {json}");
    key.set_value("Spec", &BASE64.encode(json.replace(from, to)))
        .expect("write Spec");
}

/// Makes the installed service look as an earlier goetia left it: that version in `Parameters` and
/// in the blob, and no recovery actions (the earlier shim did every restart itself).
fn register_as_earlier_version(id: &str, current_version: &str) {
    replace_in_blob(
        id,
        &format!(r#""version":"{current_version}""#),
        &format!(r#""version":"{EARLIER_VERSION}""#),
    );
    parameters_key(id)
        .set_value("Version", &EARLIER_VERSION.to_string())
        .expect("write Version");

    support::cmd::run("sc.exe", &["failureflag", id, "0"]).expect_ok();
    support::cmd::run("sc.exe", &["failure", id, "reset=", "0", "actions=", ""]).expect_ok();
}

#[skuld::test(requires = [support::elevated], labels = [ELEVATED])]
fn a_new_shim_runs_a_service_registered_by_an_earlier_version() {
    let mgr = ScmManager::new();
    let id = support::random_test_id();
    let _guard = ServiceGuard::new(&id);
    let reported = ConnectBack::listen();

    let spec = on_failure(
        &id,
        fixture_command("report", &[&reported.port().to_string()]),
        ONE_SECOND,
    );
    mgr.install(&spec, false).expect("install");
    register_as_earlier_version(&id, goetia::version());

    mgr.start(&spec.id, Budget::DEFAULT).expect("start");
    reported.accept("the daemon of a service registered by an earlier version to report in");

    // The shim logs the notice before it spawns the daemon, so it is in the log by now.
    let logged = fallback_log(&id);
    assert!(
        logged.contains(&format!("registered by goetia {EARLIER_VERSION}")) && logged.contains("goetia daemon install"),
        "the shim did not log the version-skew notice:\n{logged}"
    );

    mgr.stop(&spec.id, Budget::DEFAULT).expect("stop");
}

/// A service installed by 0.1.0 under `restart: always` keeps a blob that says so. Only `install`
/// refuses `always` for scm, so the service must stay operable.
#[skuld::test(requires = [support::elevated], labels = [ELEVATED])]
fn a_service_whose_blob_says_always_can_still_be_started_stopped_and_uninstalled() {
    let mgr = ScmManager::new();
    let id = support::random_test_id();
    let _guard = ServiceGuard::new(&id);
    let reported = ConnectBack::listen();

    let spec = on_failure(
        &id,
        fixture_command("report", &[&reported.port().to_string()]),
        ONE_SECOND,
    );
    mgr.install(&spec, false).expect("install");
    replace_in_blob(&id, r#""restart":"on-failure""#, r#""restart":"always""#);

    mgr.start(&spec.id, Budget::DEFAULT).expect("start");
    reported.accept("the daemon of a service whose blob says always to report in");
    mgr.stop(&spec.id, Budget::DEFAULT).expect("stop");
    mgr.uninstall(&spec.id).expect("uninstall");

    let query = support::cmd::run("sc.exe", &["query", &id]);
    assert_eq!(query.code, Some(1060), "the service should no longer exist:\n{query}");
}

#[skuld::test(requires = [support::elevated], labels = [ELEVATED])]
fn reinstalling_an_earlier_version_simple_service_applies_the_recovery_actions() {
    let mgr = ScmManager::new();
    let id = support::random_test_id();
    let _guard = ServiceGuard::new(&id);

    let spec = on_failure(&id, fixture_command("report", &["1"]), ONE_SECOND);
    mgr.install(&spec, false).expect("install");
    register_as_earlier_version(&id, goetia::version());

    let outcome = mgr
        .install(&spec, false)
        .expect("re-install over the earlier registration");
    assert_eq!(
        outcome,
        Outcome::Stale {
            from_version: EARLIER_VERSION.to_string()
        },
        "the missing recovery actions must read as a version refresh, not as a hand-edit"
    );

    // `render` includes the recovery actions, so an `UpToDate` here is the read-back proving they
    // were written.
    let again = mgr.install(&spec, false).expect("install again");
    assert_eq!(again, Outcome::UpToDate);
}
