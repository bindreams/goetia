//! The `kill` scenarios every backend's integration suite runs, in one place: what a kill must do
//! is one contract, and only how a daemon is installed and started differs per platform.
//!
//! The daemon is the test binary in a sentinel mode that connects to the test's listener each time
//! it starts (`sentinel::RUN_UNTIL_KILLED`, or the SCM fixture), so a start, and a restart by the
//! manager, are events the test waits for rather than times it guesses at.

use goetia::Error;
use goetia::manager::{Budget, ServiceManager, State};
use goetia::spec::Id;

use super::ConnectBack;

/// Re-check `condition` until it holds, with no cap: it is the state being waited for that ends the
/// wait. A state that never arrives hangs the test for CI's watchdog to report.
pub fn until(mut condition: impl FnMut() -> bool) {
    while !condition() {
        std::thread::yield_now();
    }
}

/// `restart` restarts a killed daemon: it is started, killed, and comes back as a different process.
/// `id` must be installed, not started, with a restart policy that restarts after a kill.
/// `started` hears from the daemon each time it starts.
pub fn a_killed_daemon_comes_back_with_a_new_pid(mgr: &dyn ServiceManager, id: &Id, started: &ConnectBack) {
    mgr.start(id, Budget::DEFAULT).expect("start");
    started.accept("the daemon to report in after start");
    let before = mgr.status(id).expect("status before the kill");
    assert_eq!(before.state, State::Running, "precondition: running ({before:?})");
    let old_pid = before.pid.expect("a running daemon reports a pid");

    mgr.kill(id).expect("kill");

    started.accept("the daemon to be restarted by the manager after the kill");
    let after = mgr.status(id).expect("status after the restart");
    let new_pid = after.pid.expect("the restarted daemon reports a pid");
    assert_ne!(
        new_pid, old_pid,
        "the manager restarted the daemon as a new process ({after:?})"
    );
}

/// A daemon whose policy is not to restart stays down after a kill, in a state that says it is not
/// running. `id` must be installed, not started, with `restart: never`.
///
/// `no_restart_configured` asserts what the platform's manager itself says about restarting this
/// service, so that "down" is a standing fact and not a snapshot of one moment.
pub fn a_killed_daemon_that_never_restarts_stays_down(
    mgr: &dyn ServiceManager,
    id: &Id,
    started: &ConnectBack,
    no_restart_configured: &dyn Fn(),
) {
    mgr.start(id, Budget::DEFAULT).expect("start");
    started.accept("the daemon to report in after start");
    assert_eq!(mgr.status(id).expect("status before the kill").state, State::Running);

    mgr.kill(id).expect("kill");

    // Settled: the manager has recorded the process gone and, under this policy, has nothing left to
    // do. Anything earlier could still be a restart that has not happened yet.
    until(|| matches!(mgr.status(id).expect("status").state, State::Stopped | State::Failed));
    let status = mgr.status(id).expect("status after the kill");
    assert_eq!(status.pid, None, "a killed daemon has no process ({status:?})");
    no_restart_configured();
}

/// Killing a daemon with no process is `NotRunning`, at the library and as a CLI exit `1` that
/// says so. `id` must be installed, not started.
pub fn killing_a_stopped_daemon_is_not_running(
    get_manager: &dyn Fn() -> goetia::Result<Box<dyn ServiceManager>>,
    id: &Id,
) {
    let mgr = get_manager().expect("manager");
    let result = mgr.kill(id);
    assert!(
        matches!(&result, Err(Error::NotRunning { id: named }) if named == id.as_str()),
        "{result:?}"
    );

    let args = goetia::cli::kill::Args {
        ids: vec![id.as_str().to_string()],
    };
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let code = goetia::cli::kill::run(&args, get_manager, &|| true, &mut out, &mut err);
    let (out, err) = (String::from_utf8_lossy(&out), String::from_utf8_lossy(&err));
    assert_eq!(code, 1, "stdout:\n{out}\nstderr:\n{err}");
    assert!(err.contains("not running"), "{err}");
}
