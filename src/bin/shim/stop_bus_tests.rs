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

/// The one arm that does not join the waiter: both kills failed, so nothing has told the daemon to
/// exit, no `child.wait()` on it can return, and joining would block for the daemon's whole
/// remaining life — leaving SCM's stop uncompleted and the service neither stoppable nor
/// uninstallable. Returning instead is what this asserts, along with the reduced guarantee it
/// carries: the daemon really is still running when it returns, so `StoppingUnkillable` is not
/// `Stopping` under another name.
///
/// Both failures are injected ([`test_hook::kills`]) because neither can be asked of the OS here:
/// `kill_tree` fails for real only on a child holding no actionable containment mechanism or on a
/// refused kill (see `wait_for_child_or_stop`), and Windows fixes a child handle's access rights at
/// creation, so `TerminateProcess` on a child this process spawned itself does not get refused on
/// demand. The child is contained anyway, so a failure of *this test* does not leave a five-minute
/// sleep behind on the runner: the job object's `KILL_ON_JOB_CLOSE` reaps it where containment
/// reached that mechanism, and `Child::drop`'s own kill covers it where it degraded to TreeWalk.
#[skuld::test]
fn wait_for_child_or_stop_returns_rather_than_waiting_on_a_daemon_it_could_not_kill() {
    const ID: &str = "wait_for_child_or_stop_returns_rather_than_waiting_on_a_daemon_it_could_not_kill";

    let mut cmd = cosca::run([
        "powershell",
        "-NoProfile",
        "-NonInteractive",
        "-Command",
        "Start-Sleep -Seconds 300",
    ]);
    cmd.contain();
    let bus = Arc::new(StopBus::new());
    let waiter = bus.waiter(ID).expect("make the waiter thread");
    let child = Arc::new(cmd.spawn().expect("spawn a long-running child"));

    let (tx, rx) = mpsc::channel();
    {
        let child = Arc::clone(&child);
        let bus = Arc::clone(&bus);
        std::thread::spawn(move || {
            // Armed on the thread the kills happen on, which is not this test's — the hook is
            // thread-local, so the cleanup kill at the end of this test is unaffected by it.
            let _refused = test_hook::kills(test_hook::Kill::Refuse);
            let outcome = bus.wait_for_child_or_stop(waiter, &child, ID);
            let _ = tx.send(outcome);
        });
    }

    bus.request_stop();

    // The same regression bound, for the same reason, as
    // `wait_for_child_or_stop_does_not_deadlock_on_stop` above: the failure this test exists to
    // catch is a call that never returns, which without a bound hangs the whole test binary.
    let outcome = rx.recv_timeout(Duration::from_secs(30)).expect(
        "wait_for_child_or_stop did not return within 30s of a stop whose kills both failed — it joined a \
         waiter whose wait nothing can make return",
    );
    assert!(
        matches!(outcome, WaitOutcome::StoppingUnkillable),
        "a stop with both kills refused reported a reaped child"
    );

    // Still running, which is the whole content of that outcome: an assertion that could not tell
    // it from an ordinary `Stopping` would leave the reduced guarantee unproven.
    let status = child
        .wait_timeout(Duration::ZERO)
        .expect("query the child's exit status");
    assert!(
        status.is_none(),
        "the child exited even though both kills were refused, so this test says nothing about the \
         unkillable case"
    );

    // What the call deliberately did not do. The detached waiter thread is parked in `child.wait()`
    // and ends once this lands.
    child.kill_tree().expect("kill the daemon the call left running");
}

/// A panic between the hand-off and the join detaches the waiter, which still holds an
/// `Arc<Child>` clone — so the caller's own `Arc` going out of scope during the unwind is not the
/// last reference and `cosca::Child::drop`'s kill-tree teardown does not run.
/// [`StopBus::wait_for_child_or_stop`]'s doc comment states this; this is what will notice when
/// someone makes one of its `expect`s recoverable, reorders the kill and the join, or adds the
/// `Drop`-guard join that would turn this unwind into a block.
///
/// The other half of that paragraph — `KILL_ON_JOB_CLOSE` reaping the tree once the shim process
/// exits — is not asserted here and cannot be from inside the process whose exit is the event; it
/// is `cosca`'s guarantee about its own job object, and its tests are where it is proven. It is
/// also only available where containment reached that mechanism: a failed Job Object assignment
/// degrades to TreeWalk, where there is no job object to close and so no such backstop.
#[skuld::test]
fn a_panic_on_the_stop_path_leaves_the_waiter_detached_and_the_daemon_untorn_down() {
    const ID: &str = "a_panic_on_the_stop_path_leaves_the_waiter_detached_and_the_daemon_untorn_down";

    let mut cmd = cosca::run([
        "powershell",
        "-NoProfile",
        "-NonInteractive",
        "-Command",
        "Start-Sleep -Seconds 300",
    ]);
    cmd.contain();
    let bus = Arc::new(StopBus::new());
    let waiter = bus.waiter(ID).expect("make the waiter thread");
    let child = Arc::new(cmd.spawn().expect("spawn a long-running child"));

    // Requested before the call, so it reaches the kill — and the panic — without blocking first:
    // `wait_while` checks its predicate before ever waiting.
    bus.request_stop();

    let (tx, rx) = mpsc::channel();
    {
        let child = Arc::clone(&child);
        let bus = Arc::clone(&bus);
        std::thread::spawn(move || {
            let _panicking = test_hook::kills(test_hook::Kill::Panic);
            let panicked = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                bus.wait_for_child_or_stop(waiter, &child, ID);
            }))
            .is_err();
            // Dropped before the send, so the reference count asserted below is not a race against
            // this thread's own clone going out of scope.
            drop(child);
            let _ = tx.send(panicked);
        });
    }

    let panicked = rx.recv_timeout(Duration::from_secs(30)).expect(
        "the panicking call never returned: an unwind that blocks — a join in a `Drop`, say — is one of the \
         regressions this bounds",
    );
    assert!(panicked, "the kill did not panic, so nothing below is about an unwind");

    // Two `Arc`s: this test's, and the detached thread's, parked in `child.wait()`. The unwind
    // therefore could not have dropped the last one, which is the only thing that runs
    // `Child::drop`'s teardown.
    assert_eq!(
        Arc::strong_count(&child),
        2,
        "the waiter's clone is gone, so the unwind joined or ended it after all and this test no longer \
         describes the detached case"
    );
    let status = child
        .wait_timeout(Duration::ZERO)
        .expect("query the child's exit status");
    assert!(
        status.is_none(),
        "the daemon was torn down during the unwind — which is better, but it is not what this module's \
         doc comment tells the next reader"
    );

    child
        .kill_tree()
        .expect("kill the daemon the panicking call left running");
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
    let Waiter { hand, thread, bus: _ } = waiter;
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
