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
    let bus = Arc::new(StopBus::new());
    // Made before the spawn, as `service::launch` makes it — see `Waiter`.
    let waiter = bus
        .waiter("wait_for_child_or_stop_does_not_deadlock_on_stop")
        .expect("make the waiter thread");
    let child = Arc::new(cmd.spawn().expect("spawn a long-running child"));

    // `wait_for_child_or_stop` runs on its own, un-scoped `'static` thread
    // (not `std::thread::scope`, which this test itself would then also
    // have to join, defeating the timeout below on exactly the regression
    // it exists to catch): if the teardown inside it ever again happens
    // *after* the call would need to return rather than before — the join
    // deadlock this module's own doc comment on `wait_for_child_or_stop`
    // describes — this thread simply never sends, and `recv_timeout` below
    // reports that as a clear failure instead of hanging the whole test
    // binary.
    let (tx, rx) = mpsc::channel();
    {
        let child = Arc::clone(&child);
        let bus = Arc::clone(&bus);
        std::thread::spawn(move || {
            let outcome =
                bus.wait_for_child_or_stop(waiter, &child, "wait_for_child_or_stop_does_not_deadlock_on_stop");
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
    // returning `Stopping` only after `kill_tree` confirms it), not merely
    // have unblocked the wait.
    let status = child
        .wait_timeout(Duration::ZERO)
        .expect("query the child's exit status");
    assert!(
        status.is_some(),
        "child is still alive after wait_for_child_or_stop returned Stopping"
    );
}

#[skuld::test]
fn wait_for_child_or_stop_returns_child_exited_when_the_child_exits_on_its_own() {
    let mut cmd = cosca::run(["powershell", "-NoProfile", "-NonInteractive", "-Command", "exit 0"]);
    cmd.contain();
    let bus = Arc::new(StopBus::new());
    let waiter = bus
        .waiter("wait_for_child_or_stop_returns_child_exited_when_the_child_exits_on_its_own")
        .expect("make the waiter thread");
    let child = Arc::new(cmd.spawn().expect("spawn a short-lived child"));

    // No stop requested: the child exiting on its own is the only wakeup
    // source, so this blocks only as long as the child itself takes to
    // start and exit.
    let outcome = bus.wait_for_child_or_stop(
        waiter,
        &child,
        "wait_for_child_or_stop_returns_child_exited_when_the_child_exits_on_its_own",
    );
    assert!(matches!(outcome, WaitOutcome::ChildExited));
}

/// [`Waiter`]'s "handed nothing — it ends", the one lifetime claim in this module with no other
/// test behind it. `service::launch`'s spawn-failure arm drops an un-handed `Waiter`, which is
/// exactly this — minus the `JoinHandle` needed to observe the outcome, so this destructures
/// rather than drops, keeping the handle to join on. A regression here hangs this test rather
/// than failing it: a thread that never ends leaves no event to bound.
#[skuld::test]
fn a_waiter_that_is_never_handed_a_child_ends_on_its_own() {
    let bus = Arc::new(StopBus::new());
    let waiter = bus
        .waiter("a_waiter_that_is_never_handed_a_child_ends_on_its_own")
        .expect("make the waiter thread");

    // Closing the only `Sender` is the whole mechanism: the thread's first act is `rx.recv()`,
    // which now returns `Err` and returns.
    let Waiter { hand, thread } = waiter;
    drop(hand);

    thread.join().expect("the waiter thread panicked instead of ending");
}

#[skuld::test]
fn wait_or_stop_returns_true_when_a_stop_is_requested_before_the_delay_elapses() {
    let bus = StopBus::new();
    // A 30s delay against an essentially-instant `request_stop()` call from
    // another thread proves the composite `Condvar::wait_timeout_while`
    // wakes on the stop rather than merely on the (much longer) timeout —
    // the delay itself is not a synchronization mechanism this test relies
    // on completing.
    let stopped_early = std::thread::scope(|scope| {
        scope.spawn(|| bus.request_stop());
        bus.wait_or_stop(Duration::from_secs(30))
    });
    assert!(stopped_early, "wait_or_stop did not observe the stop request");
}

#[skuld::test]
fn wait_or_stop_returns_false_when_the_delay_elapses_with_no_stop() {
    let bus = StopBus::new();
    // Real elapsed time here is the behavior under test — `wait_or_stop`'s
    // whole contract is "wait up to `delay`" — not a stand-in for a missing
    // signal.
    let stopped = bus.wait_or_stop(Duration::from_millis(50));
    assert!(!stopped, "wait_or_stop reported a stop that was never requested");
}
