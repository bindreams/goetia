//! The supervisor's decisions over the `Fake`: how it waits for a daemon and what it does when a
//! stop, an exit, a failed wait or a refused kill arrives. What a real kill or wait does to a real
//! process is `os_tests`'s.

use std::sync::mpsc;

use super::*;
use crate::backend::scm::shim::run::os::fake::{
    Fake, FakeGuard, FakeProcess, KillReply, WAIT_FAILED, WaitReply, WaiterEnd,
};
use crate::backend::scm::shim::run::test_support::{
    assert_failures, assert_one_failure, fallback_refused_failure, supervisor, tree_refused_failure,
};

const ID: &str = "goetia-shim-stop-bus-test";

/// A daemon spawned through the `Fake`, as `service::launch` holds it.
fn spawn_child(fake: &Fake) -> Arc<dyn Spawned> {
    Arc::from(fake.os().spawn(cosca::run(["daemon"])).expect("spawn the daemon"))
}

/// A supervisor, then the waiter, then the daemon: the order `service::launch` makes them in. See
/// [`Waiter`].
fn launched(fake: &FakeGuard, daemon: &FakeProcess) -> (Arc<Supervisor>, Waiter, Arc<dyn Spawned>) {
    let supervisor = supervisor(fake);
    fake.will_make_thread();
    fake.will_spawn(daemon.clone());
    let waiter = supervisor.waiter(ID).expect("make the waiter thread");
    (supervisor, waiter, spawn_child(fake))
}

/// The waiter is joined inside the call, so no later log can arrive.
#[skuld::test]
fn wait_for_child_or_stop_does_not_deadlock_on_stop() {
    let fake = FakeGuard::new();
    let daemon = FakeProcess::running();
    let (supervisor, waiter, child) = launched(&fake, &daemon);

    // `wait_for_child_or_stop` runs on its own, un-scoped `'static` thread (not
    // `std::thread::scope`, which this test itself would then also have to join): if the teardown
    // inside it ever again happens *after* the call would need to return rather than before — the
    // join deadlock this module's own doc comment on `wait_for_child_or_stop` describes — this
    // thread simply never sends, and `recv` below blocks forever. CI's per-binary watchdog is the
    // external bound.
    let (tx, rx) = mpsc::channel();
    {
        let child = Arc::clone(&child);
        let supervisor = Arc::clone(&supervisor);
        std::thread::spawn(move || {
            let outcome = supervisor.wait_for_child_or_stop(waiter, &child, ID, || {});
            let _ = tx.send(outcome);
        });
    }

    // No wait needed before this: `wait_for_child_or_stop`'s internal `Condvar::wait_while` checks
    // its predicate before ever blocking, so a stop requested before the waiter thread has even
    // started its own wait is not missed.
    supervisor.bus.request_stop();

    let outcome = rx
        .recv()
        .expect("the thread running wait_for_child_or_stop ended without a result");
    assert!(matches!(outcome, WaitOutcome::Stopping));
    assert_eq!(
        fake.take_failures(),
        Vec::<String>::new(),
        "a clean stop reported a failure"
    );
    assert_eq!(fake.waiter_end(0), WaiterEnd::Returned);
    // The daemon must actually be dead by now (the whole point of the call returning `Stopping`
    // only after `kill_tree` confirms it), not merely have unblocked the wait.
    assert!(
        daemon.killed(),
        "the daemon was not killed though wait_for_child_or_stop returned Stopping"
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
/// have finished logging before the lines are read, hence the wait on its end.
#[skuld::test]
fn wait_for_child_or_stop_returns_rather_than_waiting_on_a_daemon_it_could_not_kill() {
    let fake = FakeGuard::new();
    let daemon = FakeProcess::new()
        .wait(WaitReply::Blocks)
        .kill_tree(KillReply::Refuses)
        .kill(KillReply::Refuses);
    let (supervisor, waiter, child) = launched(&fake, &daemon);

    let (tx, rx) = mpsc::channel();
    {
        let child = Arc::clone(&child);
        let supervisor = Arc::clone(&supervisor);
        std::thread::spawn(move || {
            let outcome = supervisor.wait_for_child_or_stop(waiter, &child, ID, || {});
            let _ = tx.send(outcome);
        });
    }

    supervisor.bus.request_stop();

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
    assert!(
        !daemon.ended() && !daemon.killed(),
        "the daemon ended even though both kills were refused, so this test says nothing about the \
         unkillable case"
    );

    // What the call deliberately did not do. The detached waiter thread is parked in `wait` and ends
    // once this lands.
    daemon.end(1);
    assert_eq!(
        fake.waiter_end(0),
        WaiterEnd::Returned,
        "the detached waiter thread panicked"
    );
    assert_failures(
        &fake.take_failures(),
        ID,
        &[tree_refused_failure(), fallback_refused_failure()],
    );
}

/// A panic between the hand-off and the join detaches the waiter, which still holds an
/// `Arc<dyn Spawned>` clone — so the caller's own `Arc` going out of scope during the unwind is not
/// the last reference and the daemon's drop, which kills its tree, does not run.
/// [`StopBus::wait_for_child_or_stop`]'s doc comment states this; this is what will notice when
/// someone makes one of its `expect`s recoverable, reorders the kill and the join, or adds the
/// `Drop`-guard join that would turn this unwind into a block.
///
/// The other half of that paragraph — `KILL_ON_JOB_CLOSE` reaping the tree once the shim process
/// exits — is not asserted here and cannot be from inside the process whose exit is the event; it
/// is `cosca`'s guarantee about its own job object, and its tests are where it is proven.
///
/// The injected panic precedes any log; waiting for the detached waiter's end settles the lines.
#[skuld::test]
fn a_panic_on_the_stop_path_leaves_the_waiter_detached_and_the_daemon_untorn_down() {
    let fake = FakeGuard::new();
    let daemon = FakeProcess::new().wait(WaitReply::Blocks).kill_tree(KillReply::Panics);
    let (supervisor, waiter, child) = launched(&fake, &daemon);

    // Requested before the call, so it reaches the kill — and the panic — without blocking first:
    // `wait_while` checks its predicate before ever waiting.
    supervisor.bus.request_stop();

    let (tx, rx) = mpsc::channel();
    {
        let child = Arc::clone(&child);
        let supervisor = Arc::clone(&supervisor);
        std::thread::spawn(move || {
            let panicked = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                supervisor.wait_for_child_or_stop(waiter, &child, ID, || {});
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

    // Two `Arc`s: this test's, and the detached thread's, parked in `wait`. The unwind therefore
    // could not have dropped the last one, which is the only thing that drops the daemon.
    assert_eq!(
        Arc::strong_count(&child),
        2,
        "the waiter's clone is gone, so the unwind joined or ended it after all and this test no longer \
         describes the detached case"
    );
    assert!(
        !daemon.ended() && !daemon.dropped(),
        "the daemon was torn down during the unwind — which is better, but it is not what this module's \
         doc comment tells the next reader"
    );

    daemon.end(1);
    assert_eq!(
        fake.waiter_end(0),
        WaiterEnd::Returned,
        "the detached waiter thread panicked"
    );
    assert_eq!(
        fake.take_failures(),
        Vec::<String>::new(),
        "the stop path logged around the injected panic"
    );
}

/// The waiter is joined inside the call, so no later log can arrive.
#[skuld::test]
fn wait_for_child_or_stop_returns_child_exited_when_the_child_exits_on_its_own() {
    let fake = FakeGuard::new();
    let daemon = FakeProcess::new().wait(WaitReply::Exits(0));
    let (supervisor, waiter, child) = launched(&fake, &daemon);

    // No stop requested: the child exiting on its own is the only wakeup source.
    let outcome = supervisor.wait_for_child_or_stop(waiter, &child, ID, || {});

    assert!(matches!(outcome, WaitOutcome::ChildExited));
    assert_eq!(
        fake.take_failures(),
        Vec::<String>::new(),
        "a child that exited on its own reported a failure"
    );
    assert_eq!(fake.waiter_end(0), WaiterEnd::Returned);
}

/// [`Waiter`]'s "handed nothing — it ends", the one lifetime claim in this module with no other
/// test behind it. `service::launch`'s spawn-failure arm drops an un-handed `Waiter`, which is
/// exactly this — minus the `JoinHandle` needed to observe the outcome, so this destructures
/// rather than drops, keeping the handle to join on. A regression here hangs this test rather
/// than failing it: a thread that never ends leaves no event to bound.
#[skuld::test]
fn a_waiter_that_is_never_handed_a_child_ends_on_its_own() {
    let fake = FakeGuard::new();
    let supervisor = supervisor(&fake);
    fake.will_make_thread();
    let waiter = supervisor.waiter(ID).expect("make the waiter thread");

    // Closing the only `Sender` is the whole mechanism: the thread's first act is `rx.recv()`,
    // which returns `Err` and returns.
    let Waiter {
        standby,
        thread,
        bus: _,
    } = waiter;
    drop(standby);

    thread.join().expect("the waiter thread panicked instead of ending");
    assert_eq!(fake.waiter_end(0), WaiterEnd::Returned);
}

/// A failed `child.wait()` says nothing about the child, and a second blocking wait on it would sit
/// outside the bus, deaf to a stop. So the call tears the daemon down as for a stop and reports the
/// wait as failed, with the failure logged. The daemon here is long-running: the wait fails while it
/// lives.
#[skuld::test]
fn a_failed_background_wait_kills_the_daemon_and_is_reported_as_a_wait_failure() {
    let fake = FakeGuard::new();
    let daemon = FakeProcess::new()
        .wait(WaitReply::Fails(None))
        .kill_tree(KillReply::Ends);
    let (supervisor, waiter, child) = launched(&fake, &daemon);

    let outcome = supervisor.wait_for_child_or_stop(waiter, &child, ID, || {});

    assert!(
        matches!(outcome, WaitOutcome::WaitFailed { os_error: None }),
        "a failed wait was not reported as one"
    );
    assert!(daemon.killed(), "the daemon was left running after its failed wait");
    assert_one_failure(
        &fake.take_failures(),
        ID,
        &format!("background wait on child: {WAIT_FAILED}"),
    );
    assert_eq!(fake.waiter_end(0), WaiterEnd::Returned);
}

/// The same failed wait over a daemon neither kill reaches: nothing is known to have ended it, so it
/// is the unkillable outcome, which reports a stop SCM does not restart.
#[skuld::test]
fn a_failed_background_wait_over_an_unkillable_daemon_is_the_unkillable_outcome() {
    let fake = FakeGuard::new();
    let daemon = FakeProcess::new()
        .wait(WaitReply::Fails(None))
        .kill_tree(KillReply::Refuses)
        .kill(KillReply::Refuses);
    let (supervisor, waiter, child) = launched(&fake, &daemon);

    let outcome = supervisor.wait_for_child_or_stop(waiter, &child, ID, || {});

    assert!(matches!(outcome, WaitOutcome::DaemonMayRun));
    assert!(!daemon.ended(), "the daemon ended though neither kill worked");
    assert_eq!(fake.waiter_end(0), WaiterEnd::Returned);
    assert_failures(&fake.take_failures(), ID, &unkillable_failures());
}

/// A waiter that panics is reported by the join, and a stop over it is still a stop.
#[skuld::test]
fn a_panicked_waiter_is_logged_when_the_stop_path_joins_it() {
    let fake = FakeGuard::new();
    let daemon = FakeProcess::new().wait(WaitReply::Panics).kill_tree(KillReply::Ends);
    let (supervisor, waiter, child) = launched(&fake, &daemon);
    supervisor.bus.request_stop();

    let outcome = supervisor.wait_for_child_or_stop(waiter, &child, ID, || {});

    assert!(
        matches!(outcome, WaitOutcome::Stopping),
        "the stop's kill reaches the child"
    );
    assert_one_failure(&fake.take_failures(), ID, "the thread waiting on the child panicked");
    assert_eq!(fake.waiter_end(0), WaiterEnd::Panicked);
}

/// `kill_tree` fails but the direct-child fallback works: the child is dead and reaped, so the call
/// joins the waiter, but its descendants may run, as after a failed wait whose tree kill failed.
#[skuld::test]
fn a_stop_whose_kill_tree_fails_kills_the_child_and_reports_the_daemon_possibly_running() {
    let fake = FakeGuard::new();
    let daemon = FakeProcess::new()
        .wait(WaitReply::Blocks)
        .kill_tree(KillReply::Refuses)
        .kill(KillReply::Ends);
    let (supervisor, waiter, child) = launched(&fake, &daemon);
    supervisor.bus.request_stop();

    let outcome = supervisor.wait_for_child_or_stop(waiter, &child, ID, || {});

    assert!(
        matches!(outcome, WaitOutcome::DaemonMayRun),
        "the tree kill failed, so the daemon's descendants may still run"
    );
    assert!(daemon.killed(), "the fallback kill did not end the child");
    assert_failures(
        &fake.take_failures(),
        ID,
        &[tree_refused_failure(), TREE_UNKILLED.to_string()],
    );
    assert_eq!(fake.waiter_end(0), WaiterEnd::Returned);
}

/// A waiter that panics before it reports must still wake the call: with no stop requested nothing
/// else would, and the call would hang on a daemon nothing watches. It is a wait failure, so the
/// daemon is torn down and confirmed gone, and the panic is logged by the join.
#[skuld::test]
fn a_panicked_waiter_is_a_wait_failure_with_no_stop_requested() {
    let fake = FakeGuard::new();
    let daemon = FakeProcess::new().wait(WaitReply::Panics).kill_tree(KillReply::Ends);
    let (supervisor, waiter, child) = launched(&fake, &daemon);

    let outcome = supervisor.wait_for_child_or_stop(waiter, &child, ID, || {});

    assert!(
        matches!(outcome, WaitOutcome::WaitFailed { os_error: None }),
        "a waiter that panicked was not reported as a failed wait"
    );
    assert!(daemon.killed());
    assert_one_failure(&fake.take_failures(), ID, "the thread waiting on the child panicked");
    assert_eq!(fake.waiter_end(0), WaiterEnd::Panicked);
}

/// The OS error of a failed wait reaches the outcome, which `service` reports as the shim failure.
#[skuld::test]
fn a_failed_background_wait_carries_its_os_error() {
    let fake = FakeGuard::new();
    let daemon = FakeProcess::new()
        .wait(WaitReply::Fails(Some(5)))
        .kill_tree(KillReply::Ends);
    let (supervisor, waiter, child) = launched(&fake, &daemon);

    let outcome = supervisor.wait_for_child_or_stop(waiter, &child, ID, || {});

    assert!(
        matches!(outcome, WaitOutcome::WaitFailed { os_error: Some(5) }),
        "the OS error of the failed wait was lost"
    );
    assert!(daemon.killed());
    assert_eq!(fake.waiter_end(0), WaiterEnd::Returned);
    assert_one_failure(
        &fake.take_failures(),
        ID,
        &format!("background wait on child: {}", io::Error::from_raw_os_error(5)),
    );
}

/// A stop requested over a failed wait wins, as it does over an exit.
#[skuld::test]
fn a_stop_wins_over_a_failed_background_wait() {
    let fake = FakeGuard::new();
    let daemon = FakeProcess::new()
        .wait(WaitReply::Fails(None))
        .kill_tree(KillReply::Ends);
    let (supervisor, waiter, child) = launched(&fake, &daemon);
    supervisor.bus.request_stop();

    let outcome = supervisor.wait_for_child_or_stop(waiter, &child, ID, || {});

    assert!(matches!(outcome, WaitOutcome::Stopping));
    assert!(daemon.killed());
    assert_eq!(fake.waiter_end(0), WaiterEnd::Returned);
    assert_one_failure(
        &fake.take_failures(),
        ID,
        &format!("background wait on child: {WAIT_FAILED}"),
    );
}

/// With only the direct child killed, the daemon's descendants were not reached, so the failed wait
/// is not a restartable failure.
#[skuld::test]
fn a_failed_background_wait_whose_tree_kill_failed_is_not_a_restartable_failure() {
    let fake = FakeGuard::new();
    let daemon = FakeProcess::new()
        .wait(WaitReply::Fails(None))
        .kill_tree(KillReply::Refuses)
        .kill(KillReply::Ends);
    let (supervisor, waiter, child) = launched(&fake, &daemon);

    let outcome = supervisor.wait_for_child_or_stop(waiter, &child, ID, || {});

    assert!(matches!(outcome, WaitOutcome::DaemonMayRun));
    assert!(daemon.killed());
    assert_eq!(fake.waiter_end(0), WaiterEnd::Returned);
    assert_failures(&fake.take_failures(), ID, &tree_unkilled_failures());
}

/// A failed wait that ends in `DaemonMayRun` is a decided failure: a stop arriving afterwards must be
/// refused, not acknowledged over a daemon that may be running.
#[skuld::test]
fn a_failed_wait_over_an_unkillable_daemon_refuses_a_later_stop() {
    let fake = FakeGuard::new();
    let daemon = FakeProcess::new()
        .wait(WaitReply::Fails(None))
        .kill_tree(KillReply::Refuses)
        .kill(KillReply::Refuses);
    let (supervisor, waiter, child) = launched(&fake, &daemon);

    let outcome = supervisor.wait_for_child_or_stop(waiter, &child, ID, || {});

    assert!(matches!(outcome, WaitOutcome::DaemonMayRun));
    assert!(
        !supervisor.bus.request_stop(),
        "a stop was acknowledged over a daemon that may still be running"
    );
    assert_eq!(fake.waiter_end(0), WaiterEnd::Returned);
    assert_failures(&fake.take_failures(), ID, &unkillable_failures());
}

/// The same when only the tree kill is refused and the direct child's kill works.
#[skuld::test]
fn a_failed_wait_whose_tree_kill_was_refused_refuses_a_later_stop() {
    let fake = FakeGuard::new();
    let daemon = FakeProcess::new()
        .wait(WaitReply::Fails(None))
        .kill_tree(KillReply::Refuses)
        .kill(KillReply::Ends);
    let (supervisor, waiter, child) = launched(&fake, &daemon);

    let outcome = supervisor.wait_for_child_or_stop(waiter, &child, ID, || {});

    assert!(matches!(outcome, WaitOutcome::DaemonMayRun));
    assert!(
        !supervisor.bus.request_stop(),
        "a stop was acknowledged over a daemon whose tree may still be running"
    );
    assert!(daemon.killed());
    assert_eq!(fake.waiter_end(0), WaiterEnd::Returned);
    assert_failures(&fake.take_failures(), ID, &tree_unkilled_failures());
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

/// The lines of a failed wait whose `kill_tree` was refused and whose direct-child kill worked.
fn tree_unkilled_failures() -> [String; 3] {
    [
        format!("background wait on child: {WAIT_FAILED}"),
        tree_refused_failure(),
        TREE_UNKILLED.to_string(),
    ]
}

/// The line logged when `kill_tree` failed and only the direct child was killed.
const TREE_UNKILLED: &str = "the daemon's descendants were not killed: only the direct child was killed; the daemon may still be running — SCM is told the service stopped with Win32 exit code 0, so that it does not start a second copy beside it, and with service-specific exit code 1003 as the record of this failure";

/// The lines of a failed wait whose `kill_tree` and direct-child kill were both refused.
fn unkillable_failures() -> [String; 3] {
    [
        format!("background wait on child: {WAIT_FAILED}"),
        tree_refused_failure(),
        fallback_refused_failure(),
    ]
}
