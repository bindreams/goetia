//! `launch`'s ordering, which is the whole of this module that can be tested without a real SCM:
//! the thread that will wait on the daemon is made before the daemon is, so the OS refusing that
//! thread is a start failure with nothing launched rather than an orphaned daemon nothing
//! supervises.

use std::collections::BTreeMap;
use std::path::Path;

use cosca::identity::Liveness;
use goetia::spec::{Id, Kind, User};
// rand 0.10 moved `random` off `Rng` onto `RngExt`.
use rand::RngExt as _;

use super::*;
use crate::stop_bus::test_hook as waiter_threads;

/// A per-run daemon id, and the removal of the one file these tests write outside any tempdir.
///
/// `launch`'s failure arm reports through `logging::log_failure`, which never consults
/// `spec.logs`: it appends to `%ProgramData%\Goetia\logs\<id>.log`, creating the directory, and
/// writes a Windows Event Log entry. Random rather than fixed for the same reason
/// `tests/support`'s `random_test_id` is — a straggler from a crashed run cannot collide with a
/// live one, and a path derived from a random id can never be a really-installed daemon's log —
/// and removed on drop so an elevated CI runner does not accumulate one file per run. (That
/// module is `tests/`-only; a binary crate cannot reach it, hence the local copy.) The event log
/// entry cannot be avoided from here: it is one entry per run that reaches a failure arm.
struct TestId(String);

impl TestId {
    fn new() -> Self {
        Self(format!("goetia-shim-launch-test-{:016x}", rand::rng().random::<u64>()))
    }

    fn as_str(&self) -> &str {
        &self.0
    }
}

impl Drop for TestId {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(logging::default_log_path(&self.0));
    }
}

/// A `type: simple` daemon that keeps running until it is killed — the case where a lost waiter
/// would leave a real orphan, rather than a process that was about to exit anyway.
///
/// `logs` is set rather than left to `build_command`'s `%ProgramData%\Goetia\logs\` default: a
/// test must not append to the path a really-installed daemon of the same id logs to. That covers
/// the daemon's own output only — `launch`'s failure reporting writes elsewhere; see [`TestId`].
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
    let id = TestId::new();
    let stop_bus = Arc::new(StopBus::new());
    let spawned_before = test_hook::spawns();
    let _no_thread = waiter_threads::threads(0);

    // Reported, not panicked: a refused thread here surfaces as an `io::Result`, not a
    // `Scope::spawn` panic that would take the whole service process down with the daemon already
    // running.
    let launched = launch(&spec(dir.path(), id.as_str()), &stop_bus, id.as_str());

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
    let id = TestId::new();
    let stop_bus = Arc::new(StopBus::new());
    let spawned_before = test_hook::spawns();

    let launched = launch(&spec(dir.path(), id.as_str()), &stop_bus, id.as_str());

    assert!(launched.is_some(), "launch failed with every thread available");
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
    drop(launched);
    assert_eq!(
        daemon.is_alive(),
        Liveness::Dead,
        "the daemon outlived the `Child` that owned it: `Child::drop` kills the tree and waits for it, so \
         by here it is dead and reaped"
    );
}
