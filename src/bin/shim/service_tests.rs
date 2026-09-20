//! `launch`'s ordering, which is the whole of this module that can be tested without a real SCM:
//! the thread that will wait on the daemon is made before the daemon is, so the OS refusing that
//! thread is a start failure with nothing launched rather than an orphaned daemon nothing
//! supervises.

use std::collections::BTreeMap;
use std::path::Path;

use goetia::spec::{Id, Kind, User};

use super::*;
use crate::stop_bus::test_hook as waiter_threads;

/// A `type: simple` daemon that keeps running until it is killed — the case where a lost waiter
/// would leave a real orphan, rather than a process that was about to exit anyway.
///
/// `logs` is set rather than left to `build_command`'s `%ProgramData%\Goetia\logs\` default: a
/// test must not append to the path a really-installed daemon of the same id logs to.
fn spec(dir: &Path) -> DaemonSpec {
    DaemonSpec {
        id: Id::try_from("goetia-shim-launch-test").expect("a valid daemon id"),
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
    let stop_bus = Arc::new(StopBus::new());
    let spawned_before = test_hook::spawns();
    let _no_thread = waiter_threads::threads(0);

    // Reported, not panicked: `std::thread::Scope::spawn` — what this used to be — has no
    // `Result` at all, so reaching this state at the old call site (after the spawn) took the
    // whole service process down with the daemon already running.
    let launched = launch(&spec(dir.path()), &stop_bus, "goetia-shim-launch-test");

    assert!(
        launched.is_none(),
        "launch reported a daemon that nothing could wait on"
    );
    assert_eq!(
        test_hook::spawns(),
        spawned_before,
        "the daemon was spawned before the thread that would have waited on it was made — \
         a failure there leaves it running with nothing supervising it"
    );
}

#[skuld::test]
fn a_daemon_is_spawned_once_the_thread_that_waits_on_it_exists() {
    // The control for the assertion above: `launch` does reach the spawn, and the counter it is
    // asserted against does move, when the thread it needs first can be made.
    let dir = tempfile::tempdir().expect("a temp dir for the daemon's log");
    let stop_bus = Arc::new(StopBus::new());
    let spawned_before = test_hook::spawns();

    let launched = launch(&spec(dir.path()), &stop_bus, "goetia-shim-launch-test");

    assert!(launched.is_some(), "launch failed with every thread available");
    assert_eq!(test_hook::spawns(), spawned_before + 1);

    // The waiter was never handed this child (no `wait_for_child_or_stop` here), so dropping it
    // drops the last `Arc`: `cosca::Child::drop` hard-kills the job object and waits for it.
    drop(launched);
}
