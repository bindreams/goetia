//! The `kill` scenarios every backend's integration suite runs, in one place: what a kill must do
//! is one contract, and only how a daemon is installed and started differs per platform.
//!
//! The daemon is the test binary in a sentinel mode that connects to the test's listener each time
//! it starts (`sentinel::RUN_UNTIL_KILLED`, or the SCM fixture), so a start, and a restart by the
//! manager, are events the test waits for rather than times it guesses at.
//!
//! `kill` returns once the killed process has exited and the manager no longer reports it, on every
//! backend, so no scenario here waits for the manager to catch up: a `status` straight after the kill
//! never shows the killed process.

use cosca::identity::{ProcessId, Resolved};
use goetia::Error;
use goetia::manager::{Budget, ServiceManager, State};
use goetia::spec::Id;

use super::ConnectBack;

/// The identity of the running process `pid`: its start time and unique id along with the number, so
/// that a process that is later given the same number is another.
pub fn identity_of(pid: u32) -> ProcessId {
    match ProcessId::of(pid) {
        Resolved::Found(id) => id,
        other => panic!("precondition: process {pid} is running ({other:?})"),
    }
}

/// Whether the manager's report of `pid` is still of the process `old`, which was running before a kill.
/// Compared by identity: a number the killed process had can be given to the process that replaced it.
pub fn is_still_reported(pid: Option<u32>, old: ProcessId) -> bool {
    pid.is_some_and(|pid| ProcessId::of(pid) == Resolved::Found(old))
}

/// `restart` restarts a killed daemon: it is started, killed, and comes back as a different process.
/// `id` must be installed, not started, with a restart policy that restarts after a kill.
/// `started` hears from the daemon each time it starts.
pub fn a_killed_daemon_comes_back_with_a_new_pid(mgr: &dyn ServiceManager, id: &Id, started: &ConnectBack) {
    mgr.start(id, Budget::DEFAULT).expect("start");
    started.accept("the daemon to report in after start");
    let before = mgr.status(id).expect("status before the kill");
    assert_eq!(
        before.common().state,
        State::Running,
        "precondition: running ({before:?})"
    );
    let old = identity_of(before.common().pid.expect("a running daemon reports a pid"));

    mgr.kill(id).expect("kill");

    let right_after = mgr.status(id).expect("status straight after the kill");
    assert!(
        !is_still_reported(right_after.common().pid, old),
        "kill returned while the manager still reported the killed process ({right_after:?})"
    );
    started.accept("the daemon to be restarted by the manager after the kill");
    let after = mgr.status(id).expect("status after the restart");
    let new_pid = after.common().pid.expect("the restarted daemon reports a pid");
    assert_ne!(
        identity_of(new_pid),
        old,
        "the manager restarted the daemon as a new process ({after:?})"
    );
}

/// A caller that wants the next process calls `start` after `kill`, and it sees the restart the policy
/// made, or makes it now: it returns `Ok` with the daemon running as a new process, whether the policy's
/// restart delay has passed by then or not. Both hold either way, so the scenario does not depend on
/// catching the delay. `id` must be installed, not started, with a restart policy that restarts after a
/// kill. `started` hears from the daemon each time it starts.
pub fn a_start_after_a_kill_sees_the_restart(mgr: &dyn ServiceManager, id: &Id, started: &ConnectBack) {
    mgr.start(id, Budget::DEFAULT).expect("start");
    started.accept("the daemon to report in after start");
    let old_pid = mgr
        .status(id)
        .expect("status before the kill")
        .common()
        .pid
        .expect("a running daemon reports a pid");
    let old = identity_of(old_pid);

    mgr.kill(id).expect("kill");
    mgr.start(id, Budget::DEFAULT)
        .expect("a start after the kill sees the policy's restart");

    let after = mgr.status(id).expect("status after the start");
    assert_eq!(after.common().state, State::Running, "{after:?}");
    let new_pid = after.common().pid.expect("a running daemon reports a pid");
    // The process, not its number: a number the killed process had can be given to the new one.
    assert_ne!(
        identity_of(new_pid),
        old,
        "the start met the killed process ({after:?})"
    );
    started.accept("the restarted daemon to report in");
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
    assert_eq!(
        mgr.status(id).expect("status before the kill").common().state,
        State::Running
    );

    mgr.kill(id).expect("kill");

    // Settled at once: the kill returns when the manager has recorded the process gone and, under this
    // policy, has nothing left to do.
    let status = mgr.status(id).expect("status straight after the kill");
    assert!(
        matches!(status.common().state, State::Stopped | State::Failed),
        "kill returned before the manager recorded the exit ({status:?})"
    );
    assert_eq!(status.common().pid, None, "a killed daemon has no process ({status:?})");
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

    // The CLI half needs a build with the `cli` feature.
    #[cfg(feature = "cli")]
    {
        let args = goetia::cli::kill::Args {
            ids: vec![id.as_str().to_string()],
        };
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let code = goetia::cli::kill::run(&args, get_manager, &|| true, &mut out, &mut err);
        let (out, err) = (String::from_utf8_lossy(&out), String::from_utf8_lossy(&err));
        assert_eq!(code, goetia::cli::ExitCode::Error, "stdout:\n{out}\nstderr:\n{err}");
        assert!(err.contains("not running"), "{err}");
    }
}
