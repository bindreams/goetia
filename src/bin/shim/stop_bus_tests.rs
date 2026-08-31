//! Needs no elevation: creating a Job Object and assigning this process's
//! own children to it is an ordinary, unprivileged operation.

use std::sync::Arc;
use std::sync::mpsc;
use std::time::Duration;

use super::*;

#[skuld::test]
fn wait_for_child_or_stop_does_not_deadlock_on_stop() {
    let mut cmd = cosca::run([
        "powershell",
        "-NoProfile",
        "-NonInteractive",
        "-Command",
        "Start-Sleep -Seconds 300",
    ]);
    cmd.contain();
    let child = Arc::new(cmd.spawn().expect("spawn a long-running child"));
    let bus = Arc::new(StopBus::new());

    // `wait_for_child_or_stop` runs on its own, un-scoped `'static` thread
    // (not `std::thread::scope`, which this test itself would then also
    // have to join, defeating the timeout below on exactly the regression
    // it exists to catch): if the teardown inside it ever again happens
    // *after* the call would need to return rather than before — the
    // `std::thread::scope` join deadlock this module's own doc comment on
    // `wait_for_child_or_stop` describes — this thread simply never sends,
    // and `recv_timeout` below reports that as a clear failure instead of
    // hanging the whole test binary.
    let (tx, rx) = mpsc::channel();
    {
        let child = Arc::clone(&child);
        let bus = Arc::clone(&bus);
        std::thread::spawn(move || {
            let outcome = bus.wait_for_child_or_stop(&child, "wait_for_child_or_stop_does_not_deadlock_on_stop");
            let _ = tx.send(outcome);
        });
    }

    // No wait needed before this: `wait_for_child_or_stop`'s internal
    // `Condvar::wait_while` checks its predicate before ever blocking, so a
    // stop requested before the waiter thread has even started its own
    // wait is not missed (see `StopBus::wait_for_child_or_stop`'s own doc
    // comment).
    bus.request_stop();

    // A real regression-detection bound, not a synchronization mechanism —
    // see the comment on the `thread::spawn` call above for why a deadlock
    // here would otherwise hang the whole test binary rather than fail one
    // test.
    let outcome = rx
        .recv_timeout(Duration::from_secs(30))
        .expect("wait_for_child_or_stop did not return within 30s of a stop request — it likely deadlocked");
    assert!(matches!(outcome, WaitOutcome::Stopping));

    // The child must actually be dead by now (the whole point of the call
    // returning `Stopping` only after `kill_tree`/`wait_tree` confirm it),
    // not merely have unblocked the wait.
    let status = child
        .wait_timeout(Duration::ZERO)
        .expect("query the child's exit status");
    assert!(
        status.is_some(),
        "child is still alive after wait_for_child_or_stop returned Stopping"
    );
}
