//! Needs no elevation: creating a Job Object and assigning this process's
//! own children to it is an ordinary, unprivileged operation.

use std::sync::Arc;
use std::sync::mpsc;
use std::time::Duration;

use super::test_hook::{CONFIRM_REFUSED, KILL_REFUSED, WAIT_FAILED, Wait, WaiterEnd, waiter_end};
use super::*;
use crate::logging::test_hook::take;
use crate::test_support::{TestId, assert_one_failure};

const PREFIX: &str = "goetia-shim-stop-bus-test";

/// The waiter is joined inside the call, so no later log can arrive.
#[skuld::test]
fn wait_for_child_or_stop_does_not_deadlock_on_stop() {
    let id = TestId::new(PREFIX);
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
    let waiter = bus.waiter(id.as_str()).expect("make the waiter thread");
    let child = Arc::new(cmd.spawn().expect("spawn a long-running child"));

    // `wait_for_child_or_stop` runs on its own, un-scoped `'static` thread
    // (not `std::thread::scope`, which this test itself would then also
    // have to join, defeating the timeout below on exactly the regression
    // it exists to catch): if the teardown inside it ever again happens
    // *after* the call would need to return rather than before — the join
    // deadlock this module's own doc comment on `wait_for_child_or_stop`
    // describes — this thread simply never sends, and `recv` below blocks
    // forever. Both ends are this crate's code; CI's per-binary watchdog is the external bound.
    let (tx, rx) = mpsc::channel();
    {
        let child = Arc::clone(&child);
        let bus = Arc::clone(&bus);
        let id = id.as_str().to_string();
        std::thread::spawn(move || {
            let outcome = bus.wait_for_child_or_stop(waiter, &child, &id);
            let _ = tx.send(outcome);
        });
    }

    // No wait needed before this: `wait_for_child_or_stop`'s internal
    // `Condvar::wait_while` checks its predicate before ever blocking, so a
    // stop requested before the waiter thread has even started its own
    // wait is not missed (see `StopBus::wait_for_child_or_stop`'s own doc
    // comment).
    bus.request_stop();

    let outcome = rx
        .recv()
        .expect("the thread running wait_for_child_or_stop ended without a result");
    assert!(matches!(outcome, WaitOutcome::Stopping));
    assert_eq!(
        take(id.as_str()),
        Vec::<String>::new(),
        "a clean stop reported a failure"
    );
    assert_eq!(waiter_end(id.as_str()), WaiterEnd::Returned);

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
/// carries: the daemon really is still running when it returns, so `DaemonMayRun` is not
/// `Stopping` under another name.
///
/// It also pins that both refused kills are logged, `kill_tree`'s first. The detached waiter must
/// have finished logging before the journal is read, hence the wait on its end.
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
    let id = TestId::new(PREFIX);

    let mut cmd = cosca::run([
        "powershell",
        "-NoProfile",
        "-NonInteractive",
        "-Command",
        "Start-Sleep -Seconds 300",
    ]);
    cmd.contain();
    let bus = Arc::new(StopBus::new());
    let waiter = bus.waiter(id.as_str()).expect("make the waiter thread");
    let child = Arc::new(cmd.spawn().expect("spawn a long-running child"));

    let (tx, rx) = mpsc::channel();
    {
        let child = Arc::clone(&child);
        let bus = Arc::clone(&bus);
        let id = id.as_str().to_string();
        std::thread::spawn(move || {
            // Armed on the thread the kills happen on, which is not this test's — the hook is
            // thread-local, so the cleanup kill at the end of this test is unaffected by it.
            let _refused = test_hook::kills(test_hook::Kill::Refuse);
            let outcome = bus.wait_for_child_or_stop(waiter, &child, &id);
            let _ = tx.send(outcome);
        });
    }

    bus.request_stop();

    // Blocks forever if the call joined a waiter whose wait nothing can make return.
    let outcome = rx
        .recv()
        .expect("the thread running wait_for_child_or_stop ended without a result");
    assert!(
        matches!(outcome, WaitOutcome::DaemonMayRun),
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
    assert_eq!(
        waiter_end(id.as_str()),
        WaiterEnd::Returned,
        "the detached waiter thread panicked"
    );

    let lines = take(id.as_str());
    assert_eq!(lines.len(), 2, "both refused kills must be journalled: {lines:?}");
    // kill_tree's first.
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
///
/// The injected panic precedes any log; waiting for the detached waiter's end settles the journal.
#[skuld::test]
fn a_panic_on_the_stop_path_leaves_the_waiter_detached_and_the_daemon_untorn_down() {
    let id = TestId::new(PREFIX);

    let mut cmd = cosca::run([
        "powershell",
        "-NoProfile",
        "-NonInteractive",
        "-Command",
        "Start-Sleep -Seconds 300",
    ]);
    cmd.contain();
    let bus = Arc::new(StopBus::new());
    let waiter = bus.waiter(id.as_str()).expect("make the waiter thread");
    let child = Arc::new(cmd.spawn().expect("spawn a long-running child"));

    // Requested before the call, so it reaches the kill — and the panic — without blocking first:
    // `wait_while` checks its predicate before ever waiting.
    bus.request_stop();

    let (tx, rx) = mpsc::channel();
    {
        let child = Arc::clone(&child);
        let bus = Arc::clone(&bus);
        let id = id.as_str().to_string();
        std::thread::spawn(move || {
            let _panicking = test_hook::kills(test_hook::Kill::Panic);
            let panicked = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                bus.wait_for_child_or_stop(waiter, &child, &id);
            }))
            .is_err();
            // Dropped before the send, so the reference count asserted below is not a race against
            // this thread's own clone going out of scope.
            drop(child);
            let _ = tx.send(panicked);
        });
    }

    // Blocks forever if the unwind blocks — a join in a `Drop`, say.
    let panicked = rx
        .recv()
        .expect("the thread running the panicking call ended without a result");
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
    assert_eq!(
        waiter_end(id.as_str()),
        WaiterEnd::Returned,
        "the detached waiter thread panicked"
    );
    assert_eq!(
        take(id.as_str()),
        Vec::<String>::new(),
        "the stop path logged around the injected panic"
    );
}

/// The waiter is joined inside the call, so no later log can arrive.
#[skuld::test]
fn wait_for_child_or_stop_returns_child_exited_when_the_child_exits_on_its_own() {
    let id = TestId::new(PREFIX);
    let mut cmd = cosca::run(["powershell", "-NoProfile", "-NonInteractive", "-Command", "exit 0"]);
    cmd.contain();
    let bus = Arc::new(StopBus::new());
    let waiter = bus.waiter(id.as_str()).expect("make the waiter thread");
    let child = Arc::new(cmd.spawn().expect("spawn a short-lived child"));

    // No stop requested: the child exiting on its own is the only wakeup
    // source, so this blocks only as long as the child itself takes to
    // start and exit.
    let outcome = bus.wait_for_child_or_stop(waiter, &child, id.as_str());
    assert!(matches!(outcome, WaitOutcome::ChildExited));
    assert_eq!(
        take(id.as_str()),
        Vec::<String>::new(),
        "a child that exited on its own reported a failure"
    );
    assert_eq!(waiter_end(id.as_str()), WaiterEnd::Returned);
}

/// [`Waiter`]'s "handed nothing — it ends", the one lifetime claim in this module with no other
/// test behind it. `service::launch`'s spawn-failure arm drops an un-handed `Waiter`, which is
/// exactly this — minus the `JoinHandle` needed to observe the outcome, so this destructures
/// rather than drops, keeping the handle to join on. A regression here hangs this test rather
/// than failing it: a thread that never ends leaves no event to bound.
#[skuld::test]
fn a_waiter_that_is_never_handed_a_child_ends_on_its_own() {
    let id = TestId::new(PREFIX);
    let bus = Arc::new(StopBus::new());
    let waiter = bus.waiter(id.as_str()).expect("make the waiter thread");

    // Closing the only `Sender` is the whole mechanism: the thread's first act is `rx.recv()`,
    // which returns `Err` and returns.
    let Waiter { hand, thread, bus: _ } = waiter;
    drop(hand);

    thread.join().expect("the waiter thread panicked instead of ending");
    assert_eq!(waiter_end(id.as_str()), WaiterEnd::Returned);
}

/// A failed `child.wait()` says nothing about the child, and a second blocking wait on it would sit
/// outside the bus, deaf to a stop. So the call tears the daemon down as for a stop and reports the
/// wait as failed, with the failure logged. The daemon here is long-running: the wait fails while it
/// lives.
#[skuld::test]
fn a_failed_background_wait_kills_the_daemon_and_is_reported_as_a_wait_failure() {
    let id = TestId::new(PREFIX);
    let mut cmd = cosca::run([
        "powershell",
        "-NoProfile",
        "-NonInteractive",
        "-Command",
        "Start-Sleep -Seconds 300",
    ]);
    cmd.contain();
    let bus = Arc::new(StopBus::new());
    // Armed on this thread, which makes the waiter: the hook is read when it is made.
    let waiter = {
        let _failing = test_hook::waits(Wait::Fail(None));
        bus.waiter(id.as_str()).expect("make the waiter thread")
    };
    let child = Arc::new(cmd.spawn().expect("spawn a long-running child"));

    let outcome = bus.wait_for_child_or_stop(waiter, &child, id.as_str());

    assert!(
        matches!(outcome, WaitOutcome::WaitFailed { os_error: None }),
        "a failed wait was not reported as one"
    );
    // Termination is asynchronous: wait for the exit rather than polling for it. A daemon left
    // running hangs here, for CI's watchdog to report.
    child
        .wait()
        .expect("the daemon ends once its failed wait has killed it");
    assert_one_failure(
        &take(id.as_str()),
        id.as_str(),
        &format!("background wait on child: {WAIT_FAILED}"),
    );
    assert_eq!(waiter_end(id.as_str()), WaiterEnd::Returned);
}

/// The same failed wait over a daemon neither kill reaches: nothing is known to have ended it, so it
/// is the unkillable outcome, which reports a stop SCM does not restart.
#[skuld::test]
fn a_failed_background_wait_over_an_unkillable_daemon_is_the_unkillable_outcome() {
    let id = TestId::new(PREFIX);
    let mut cmd = cosca::run([
        "powershell",
        "-NoProfile",
        "-NonInteractive",
        "-Command",
        "Start-Sleep -Seconds 300",
    ]);
    cmd.contain();
    let bus = Arc::new(StopBus::new());
    let waiter = {
        let _failing = test_hook::waits(Wait::Fail(None));
        bus.waiter(id.as_str()).expect("make the waiter thread")
    };
    let child = Arc::new(cmd.spawn().expect("spawn a long-running child"));

    let outcome = {
        let _refused = test_hook::kills(test_hook::Kill::Refuse);
        bus.wait_for_child_or_stop(waiter, &child, id.as_str())
    };

    assert!(matches!(outcome, WaitOutcome::DaemonMayRun));
    child.kill_tree().expect("kill the daemon the call left running");
    assert_eq!(waiter_end(id.as_str()), WaiterEnd::Returned);
}

/// A waiter that panics is reported by the join, and a stop over it is still a stop.
#[skuld::test]
fn a_panicked_waiter_is_logged_when_the_stop_path_joins_it() {
    let id = TestId::new(PREFIX);
    let mut cmd = cosca::run([
        "powershell",
        "-NoProfile",
        "-NonInteractive",
        "-Command",
        "Start-Sleep -Seconds 300",
    ]);
    cmd.contain();
    let bus = Arc::new(StopBus::new());
    let waiter = {
        let _panicking = test_hook::waits(Wait::Panic);
        bus.waiter(id.as_str()).expect("make the waiter thread")
    };
    let child = Arc::new(cmd.spawn().expect("spawn a long-running child"));
    bus.request_stop();

    let outcome = bus.wait_for_child_or_stop(waiter, &child, id.as_str());

    assert!(
        matches!(outcome, WaitOutcome::Stopping),
        "the stop's kill is real and reaches the child"
    );
    assert_one_failure(
        &take(id.as_str()),
        id.as_str(),
        "the thread waiting on the child panicked",
    );
    assert_eq!(waiter_end(id.as_str()), WaiterEnd::Panicked);
}

/// `kill_tree` fails but the direct-child fallback works: the daemon is dead and reaped, so the
/// call joins the waiter and returns `Stopping`, with only `kill_tree`'s failure logged.
#[skuld::test]
fn a_stop_whose_kill_tree_fails_falls_back_to_killing_the_child() {
    let id = TestId::new(PREFIX);
    let mut cmd = cosca::run([
        "powershell",
        "-NoProfile",
        "-NonInteractive",
        "-Command",
        "Start-Sleep -Seconds 300",
    ]);
    cmd.contain();
    let bus = Arc::new(StopBus::new());
    let waiter = bus.waiter(id.as_str()).expect("make the waiter thread");
    let child = Arc::new(cmd.spawn().expect("spawn a long-running child"));
    bus.request_stop();

    // The call runs on this thread, so the hook is armed on it; the guard drops with the test.
    let _refused = test_hook::kills(test_hook::Kill::RefuseTree);
    let outcome = bus.wait_for_child_or_stop(waiter, &child, id.as_str());

    assert!(
        matches!(outcome, WaitOutcome::Stopping),
        "the fallback kill reaped the child, so the stop is a clean one"
    );
    assert!(
        child
            .wait_timeout(Duration::ZERO)
            .expect("query the child's exit status")
            .is_some(),
        "the fallback kill did not end the child"
    );
    // Only kill_tree's failure: the fallback succeeded.
    assert_one_failure(
        &take(id.as_str()),
        id.as_str(),
        &format!(
            "kill_tree on stop: {KILL_REFUSED}; falling back to killing the direct child only (its own descendants, if any, cannot be reached without a containment mechanism)"
        ),
    );
    assert_eq!(waiter_end(id.as_str()), WaiterEnd::Returned);
}

/// A waiter that panics before it reports must still wake the call: with no stop requested nothing
/// else would, and the call would hang on a daemon nothing watches. It is a wait failure, so the
/// daemon is torn down and confirmed gone, and the panic is logged by the join.
#[skuld::test]
fn a_panicked_waiter_is_a_wait_failure_with_no_stop_requested() {
    let id = TestId::new(PREFIX);
    let mut cmd = sleeper();
    let bus = Arc::new(StopBus::new());
    let waiter = {
        let _panicking = test_hook::waits(Wait::Panic);
        bus.waiter(id.as_str()).expect("make the waiter thread")
    };
    let child = Arc::new(cmd.spawn().expect("spawn a long-running child"));

    let outcome = bus.wait_for_child_or_stop(waiter, &child, id.as_str());

    assert!(
        matches!(outcome, WaitOutcome::WaitFailed { os_error: None }),
        "a waiter that panicked was not reported as a failed wait"
    );
    assert_dead(&child);
    assert_one_failure(
        &take(id.as_str()),
        id.as_str(),
        "the thread waiting on the child panicked",
    );
    assert_eq!(waiter_end(id.as_str()), WaiterEnd::Panicked);
}

/// The OS error of a failed wait reaches the outcome, which `service` reports as the shim failure.
#[skuld::test]
fn a_failed_background_wait_carries_its_os_error() {
    let id = TestId::new(PREFIX);
    let mut cmd = sleeper();
    let bus = Arc::new(StopBus::new());
    let waiter = {
        let _failing = test_hook::waits(Wait::Fail(Some(5)));
        bus.waiter(id.as_str()).expect("make the waiter thread")
    };
    let child = Arc::new(cmd.spawn().expect("spawn a long-running child"));

    let outcome = bus.wait_for_child_or_stop(waiter, &child, id.as_str());

    assert!(
        matches!(outcome, WaitOutcome::WaitFailed { os_error: Some(5) }),
        "the OS error of the failed wait was lost"
    );
    assert_dead(&child);
    assert_one_failure(
        &take(id.as_str()),
        id.as_str(),
        &format!("background wait on child: {}", io::Error::from_raw_os_error(5)),
    );
}

/// A stop requested over a failed wait wins, as it does over an exit.
#[skuld::test]
fn a_stop_wins_over_a_failed_background_wait() {
    let id = TestId::new(PREFIX);
    let mut cmd = sleeper();
    let bus = Arc::new(StopBus::new());
    let waiter = {
        let _failing = test_hook::waits(Wait::Fail(None));
        bus.waiter(id.as_str()).expect("make the waiter thread")
    };
    let child = Arc::new(cmd.spawn().expect("spawn a long-running child"));
    bus.request_stop();

    let outcome = bus.wait_for_child_or_stop(waiter, &child, id.as_str());

    assert!(matches!(outcome, WaitOutcome::Stopping));
    assert_dead(&child);
}

/// The failed wait said nothing about the daemon, and the kill's return does not say it is gone: a
/// restartable failure is reported only after a fresh wait on the process returns. When that wait
/// fails too the daemon may still be running, and SCM must not start a second copy.
#[skuld::test]
fn a_failed_background_wait_whose_kill_could_not_be_confirmed_is_not_a_restartable_failure() {
    let id = TestId::new(PREFIX);
    let mut cmd = sleeper();
    let bus = Arc::new(StopBus::new());
    let waiter = {
        let _failing = test_hook::waits(Wait::Fail(None));
        bus.waiter(id.as_str()).expect("make the waiter thread")
    };
    let child = Arc::new(cmd.spawn().expect("spawn a long-running child"));

    let outcome = {
        let _unconfirmed = test_hook::confirmations_fail();
        bus.wait_for_child_or_stop(waiter, &child, id.as_str())
    };

    assert!(
        matches!(outcome, WaitOutcome::DaemonMayRun),
        "a daemon not confirmed gone was reported as a restartable failure"
    );
    // The kill itself worked; only the confirmation was refused.
    assert_dead(&child);
    let lines = take(id.as_str());
    assert_eq!(lines.len(), 2, "the wait failure and the unconfirmed exit: {lines:?}");
    assert_one_failure(
        &lines[..1],
        id.as_str(),
        &format!("background wait on child: {WAIT_FAILED}"),
    );
    assert_one_failure(
        &lines[1..],
        id.as_str(),
        &format!(
            "confirm the daemon ended after the background wait failed: {CONFIRM_REFUSED}; the daemon may still be running — SCM is told the service stopped with Win32 exit code 0, so that it does not start a second copy beside it, and with service-specific exit code 1003 as the record of this failure"
        ),
    );
}

/// With only the direct child killed, the daemon's descendants were not reached, so the tree is not
/// confirmed gone and the failed wait is not a restartable failure.
#[skuld::test]
fn a_failed_background_wait_whose_tree_kill_failed_is_not_a_restartable_failure() {
    let id = TestId::new(PREFIX);
    let mut cmd = sleeper();
    let bus = Arc::new(StopBus::new());
    let waiter = {
        let _failing = test_hook::waits(Wait::Fail(None));
        bus.waiter(id.as_str()).expect("make the waiter thread")
    };
    let child = Arc::new(cmd.spawn().expect("spawn a long-running child"));

    let outcome = {
        let _refused = test_hook::kills(test_hook::Kill::RefuseTree);
        bus.wait_for_child_or_stop(waiter, &child, id.as_str())
    };

    assert!(matches!(outcome, WaitOutcome::DaemonMayRun));
    assert_dead(&child);
    let lines = take(id.as_str());
    assert_eq!(lines.len(), 3, "{lines:?}");
    assert_one_failure(
        &lines[..1],
        id.as_str(),
        &format!("background wait on child: {WAIT_FAILED}"),
    );
    assert_one_failure(
        &lines[1..2],
        id.as_str(),
        &format!(
            "kill_tree on stop: {KILL_REFUSED}; falling back to killing the direct child only (its own descendants, if any, cannot be reached without a containment mechanism)"
        ),
    );
    assert_one_failure(
        &lines[2..],
        id.as_str(),
        "the daemon's descendants were not confirmed ended after the background wait failed: only the direct child was killed; the daemon may still be running — SCM is told the service stopped with Win32 exit code 0, so that it does not start a second copy beside it, and with service-specific exit code 1003 as the record of this failure",
    );
}

// Fate ----------------------------------------------------------------------------------------------------------------

#[skuld::test]
fn a_stop_after_a_failure_was_decided_is_refused() {
    let bus = StopBus::new();

    assert_eq!(bus.settle(Reported::DaemonExit(7)), Reported::DaemonExit(7));

    assert!(
        !bus.request_stop(),
        "a stop was acknowledged after the failure was decided"
    );
}

#[skuld::test]
fn a_stop_before_a_failure_was_decided_turns_the_failure_into_a_clean_stop() {
    let bus = StopBus::new();

    assert!(bus.request_stop());

    assert_eq!(bus.settle(Reported::DaemonExit(7)), Reported::Clean);
    assert_eq!(bus.settle(Reported::shim_failure(Some(5))), Reported::Clean);
}

#[skuld::test]
fn a_clean_exit_does_not_refuse_a_later_stop() {
    let bus = StopBus::new();

    assert_eq!(bus.settle(Reported::Clean), Reported::Clean);

    assert!(bus.request_stop());
}

#[skuld::test]
fn a_repeated_stop_is_accepted_again() {
    let bus = StopBus::new();

    assert!(bus.request_stop());
    assert!(bus.request_stop());
}

fn sleeper() -> cosca::Command {
    let mut cmd = cosca::run([
        "powershell",
        "-NoProfile",
        "-NonInteractive",
        "-Command",
        "Start-Sleep -Seconds 300",
    ]);
    cmd.contain();
    cmd
}

/// Termination is asynchronous, so this waits for the exit rather than polling for it; a daemon left
/// running hangs here, for CI's watchdog to report.
fn assert_dead(child: &Child) {
    child.wait().expect("the daemon ends once the call has killed it");
}
