//! The supervisor's decisions over the `Fake`: how it waits for a daemon and what it does when a
//! stop, an exit, a failed wait or a refused kill arrives. What a real kill or wait does to a real
//! process is `os_tests`'s.

use std::sync::mpsc;

use super::*;
use crate::backend::scm::shim::run::os::fake::{
    AskReply, Fake, FakeGuard, FakeProcess, KillReply, Op, ProcessCall, TreeWaitReply, WaitReply, WaiterEnd, ask_io,
    ask_may_have_started, ask_unassessable, ask_unsupported, tree_kill_refused, try_wait_failed, wait_failed,
    wait_failed_with_code, wait_tree_failed,
};
use crate::backend::scm::shim::run::test_support::{
    assert_failures, assert_one_failure, began, fallback_refused_failure, supervisor, tree_refused_failure,
};

const ID: &str = "goetia-shim-stop-bus-test";

/// Whether the outcome is a stop of a root that had exited by itself, whether or not its status was read.
fn root_exited(outcome: &WaitOutcome) -> bool {
    matches!(outcome, WaitOutcome::Stopping(RootExit::Status(_) | RootExit::Unread))
}

/// A daemon spawned through the `Fake`, as `service::launch` holds it.
fn spawn_child(fake: &Fake) -> Arc<dyn Spawned> {
    Arc::from(fake.os().spawn(cosca::run(["daemon"])).expect("spawn the daemon"))
}

/// A supervisor, then the waiter, then the daemon: the order `service::launch` makes them in. See
/// [`Waiter`].
fn launched(fake: &FakeGuard, daemon: &FakeProcess) -> (Arc<Supervisor>, Waiter, Arc<dyn Spawned>) {
    launched_with_period(fake, daemon, PROGRESS_PERIOD)
}

/// [`launched`] with slices of the wait for the tree no longer than `period`.
fn launched_with_period(
    fake: &FakeGuard,
    daemon: &FakeProcess,
    period: Duration,
) -> (Arc<Supervisor>, Waiter, Arc<dyn Spawned>) {
    let supervisor = Arc::new(Supervisor::new(fake.os(), fake.log(), period));
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
    // thread simply never sends, and `recv` below blocks forever.
    let (tx, rx) = mpsc::channel();
    {
        let child = Arc::clone(&child);
        let supervisor = Arc::clone(&supervisor);
        std::thread::spawn(move || {
            let outcome = supervisor.wait_for_child_or_stop(waiter, &child, ID, StopPlan::AtOnce, |_| {});
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
    assert!(matches!(outcome, WaitOutcome::Stopping { .. }));
    assert_eq!(
        fake.take_failures(),
        Vec::<String>::new(),
        "a clean stop reported a failure"
    );
    assert_eq!(fake.waiter_end(0), WaiterEnd::Returned);
    // The daemon must actually be dead by now (the call returns `Stopping` only after joining the
    // waiter on the root, which returns when the root ends), not merely have unblocked the wait.
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
/// It also pins that both refused kills are logged, `send_kill_tree`'s first. The detached waiter must
/// have finished logging before the lines are read, hence the wait on its end.
#[skuld::test]
fn wait_for_child_or_stop_returns_rather_than_waiting_on_a_daemon_it_could_not_kill() {
    let fake = FakeGuard::new();
    let daemon = FakeProcess::new()
        .wait(WaitReply::Blocks)
        .send_kill_tree(KillReply::Refuses)
        .kill(KillReply::Refuses);
    let (supervisor, waiter, child) = launched(&fake, &daemon);

    let (tx, rx) = mpsc::channel();
    {
        let child = Arc::clone(&child);
        let supervisor = Arc::clone(&supervisor);
        std::thread::spawn(move || {
            let outcome = supervisor.wait_for_child_or_stop(waiter, &child, ID, StopPlan::AtOnce, |_| {});
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
/// [`Supervisor::wait_for_child_or_stop`]'s doc comment states this; this is what will notice when
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
    let daemon = FakeProcess::new()
        .wait(WaitReply::Blocks)
        .send_kill_tree(KillReply::Panics);
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
                supervisor.wait_for_child_or_stop(waiter, &child, ID, StopPlan::AtOnce, |_| {});
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
    let outcome = supervisor.wait_for_child_or_stop(waiter, &child, ID, StopPlan::AtOnce, |_| {});

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
        .send_kill_tree(KillReply::Ends);
    let (supervisor, waiter, child) = launched(&fake, &daemon);

    let outcome = supervisor.wait_for_child_or_stop(waiter, &child, ID, StopPlan::AtOnce, |_| {});

    assert!(
        matches!(outcome, WaitOutcome::WaitFailed { os_error: None }),
        "a failed wait was not reported as one"
    );
    assert!(daemon.killed(), "the daemon was left running after its failed wait");
    assert_one_failure(
        &fake.take_failures(),
        ID,
        &format!("background wait on child: {}", Chain(&wait_failed())),
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
        .send_kill_tree(KillReply::Refuses)
        .kill(KillReply::Refuses);
    let (supervisor, waiter, child) = launched(&fake, &daemon);

    let outcome = supervisor.wait_for_child_or_stop(waiter, &child, ID, StopPlan::AtOnce, |_| {});

    assert!(matches!(outcome, WaitOutcome::DaemonMayRun));
    assert!(!daemon.ended(), "the daemon ended though neither kill worked");
    assert_eq!(fake.waiter_end(0), WaiterEnd::Returned);
    assert_failures(&fake.take_failures(), ID, &unkillable_failures());
}

/// A waiter that panics is reported by the join, and a stop over it is still a stop.
#[skuld::test]
fn a_panicked_waiter_is_logged_when_the_stop_path_joins_it() {
    let fake = FakeGuard::new();
    let daemon = FakeProcess::new()
        .wait(WaitReply::Panics)
        .send_kill_tree(KillReply::Ends);
    let (supervisor, waiter, child) = launched(&fake, &daemon);
    supervisor.bus.request_stop();

    let outcome = supervisor.wait_for_child_or_stop(waiter, &child, ID, StopPlan::AtOnce, |_| {});

    assert!(
        matches!(outcome, WaitOutcome::Stopping { .. }),
        "the stop's kill reaches the child"
    );
    assert_one_failure(&fake.take_failures(), ID, "the thread waiting on the child panicked");
    assert_eq!(fake.waiter_end(0), WaiterEnd::Panicked);
}

/// `send_kill_tree` fails but the direct-child fallback works: the child is dead and reaped, so the call
/// joins the waiter, but its descendants may run, as after a failed wait whose tree kill failed.
#[skuld::test]
fn a_stop_whose_send_kill_tree_fails_kills_the_child_and_reports_the_daemon_possibly_running() {
    let fake = FakeGuard::new();
    let daemon = FakeProcess::new()
        .wait(WaitReply::Blocks)
        .send_kill_tree(KillReply::Refuses)
        .kill(KillReply::Ends);
    let (supervisor, waiter, child) = launched(&fake, &daemon);
    supervisor.bus.request_stop();

    let outcome = supervisor.wait_for_child_or_stop(waiter, &child, ID, StopPlan::AtOnce, |_| {});

    assert!(
        matches!(outcome, WaitOutcome::TreeMayRun),
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
    let daemon = FakeProcess::new()
        .wait(WaitReply::Panics)
        .send_kill_tree(KillReply::Ends);
    let (supervisor, waiter, child) = launched(&fake, &daemon);

    let outcome = supervisor.wait_for_child_or_stop(waiter, &child, ID, StopPlan::AtOnce, |_| {});

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
        .send_kill_tree(KillReply::Ends);
    let (supervisor, waiter, child) = launched(&fake, &daemon);

    let outcome = supervisor.wait_for_child_or_stop(waiter, &child, ID, StopPlan::AtOnce, |_| {});

    assert!(
        matches!(outcome, WaitOutcome::WaitFailed { os_error: Some(5) }),
        "the OS error of the failed wait was lost"
    );
    assert!(daemon.killed());
    assert_eq!(fake.waiter_end(0), WaiterEnd::Returned);
    assert_one_failure(
        &fake.take_failures(),
        ID,
        &format!("background wait on child: {}", Chain(&wait_failed_with_code(5))),
    );
}

/// A stop requested over a failed wait wins, as it does over an exit.
#[skuld::test]
fn a_stop_wins_over_a_failed_background_wait() {
    let fake = FakeGuard::new();
    let daemon = FakeProcess::new()
        .wait(WaitReply::Fails(None))
        .send_kill_tree(KillReply::Ends);
    let (supervisor, waiter, child) = launched(&fake, &daemon);
    supervisor.bus.request_stop();

    let outcome = supervisor.wait_for_child_or_stop(waiter, &child, ID, StopPlan::AtOnce, |_| {});

    assert!(
        matches!(outcome, WaitOutcome::Stopping(RootExit::NotSeen)),
        "a failed wait is no exit"
    );
    assert!(daemon.killed());
    assert_eq!(fake.waiter_end(0), WaiterEnd::Returned);
    assert_one_failure(
        &fake.take_failures(),
        ID,
        &format!("background wait on child: {}", Chain(&wait_failed())),
    );
}

/// With only the direct child killed, the daemon's descendants were not reached, so the failed wait
/// is not a restartable failure.
#[skuld::test]
fn a_failed_background_wait_whose_tree_kill_failed_is_not_a_restartable_failure() {
    let fake = FakeGuard::new();
    let daemon = FakeProcess::new()
        .wait(WaitReply::Fails(None))
        .send_kill_tree(KillReply::Refuses)
        .kill(KillReply::Ends);
    let (supervisor, waiter, child) = launched(&fake, &daemon);

    let outcome = supervisor.wait_for_child_or_stop(waiter, &child, ID, StopPlan::AtOnce, |_| {});

    assert!(matches!(outcome, WaitOutcome::TreeMayRun));
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
        .send_kill_tree(KillReply::Refuses)
        .kill(KillReply::Refuses);
    let (supervisor, waiter, child) = launched(&fake, &daemon);

    let outcome = supervisor.wait_for_child_or_stop(waiter, &child, ID, StopPlan::AtOnce, |_| {});

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
        .send_kill_tree(KillReply::Refuses)
        .kill(KillReply::Ends);
    let (supervisor, waiter, child) = launched(&fake, &daemon);

    let outcome = supervisor.wait_for_child_or_stop(waiter, &child, ID, StopPlan::AtOnce, |_| {});

    assert!(matches!(outcome, WaitOutcome::TreeMayRun));
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

/// The lines of a failed wait whose `send_kill_tree` was refused and whose direct-child kill worked.
fn tree_unkilled_failures() -> [String; 3] {
    [
        format!("background wait on child: {}", Chain(&wait_failed())),
        tree_refused_failure(),
        TREE_UNKILLED.to_string(),
    ]
}

/// The line logged when `send_kill_tree` failed and only the direct child was killed.
const TREE_UNKILLED: &str = "the daemon's descendants were not killed: only the direct child was killed; the daemon may still be running — SCM is told the service stopped with Win32 exit code 0, so that it does not start a second copy beside it, and with service-specific exit code 1003 as the record of this failure";

/// The lines of a failed wait whose `send_kill_tree` and direct-child kill were both refused.
fn unkillable_failures() -> [String; 3] {
    [
        format!("background wait on child: {}", Chain(&wait_failed())),
        tree_refused_failure(),
        fallback_refused_failure(),
    ]
}

// Stop plans ----------------------------------------------------------------------------------------------------------

/// A service whose stop is `signal` and `timeout`.
fn service_stopping_by(signal: crate::spec::StopSignal, timeout: Option<Duration>) -> DaemonSpec {
    use crate::spec::{Protocol, ServiceFields, ServiceSpec, StopAction};
    ServiceSpec::new(ServiceFields {
        protocol: Protocol::Plain,
        stop_action: StopAction {
            send_signal: signal,
            timeout: timeout.filter(|_| signal != crate::spec::StopSignal::Kill),
        },
        ..crate::testing::base_fields(ID, vec!["C:/daemon.exe".to_string()])
    })
    .map(DaemonSpec::from)
    .expect("a valid service")
}

/// The `timeout` the ask-and-wait tests state: far from any wait a test makes, so what is left of it is
/// read off the duration the tree wait was given.
const HOUR: Duration = Duration::from_secs(60 * 60);

/// A `timeout` shorter than [`PROGRESS_PERIOD`]: one slice of the wait for the tree is the whole wait. The `Fake`'s
/// `Expires` blocks for that slice, so a test that waits it out takes this long.
const BRIEF: Duration = Duration::from_millis(50);

/// One accepted stop over the `Fake`, in the order `service::launch` makes the pieces: the stop is accepted
/// before the wait, so it is seen at once. What `Began` told, and the outcome.
fn stopped_by(fake: &FakeGuard, daemon: &FakeProcess, plan: StopPlan) -> (WaitOutcome, Vec<WaitEnd>) {
    let (supervisor, waiter, child) = launched(fake, daemon);
    assert!(supervisor.bus.request_stop());
    let mut told = Vec::new();
    let outcome = supervisor.wait_for_child_or_stop(waiter, &child, ID, plan, |event| told.extend(began(event)));
    assert_eq!(fake.waiter_end(0), WaiterEnd::Returned);
    (outcome, told)
}

/// [`stopped_by`], where the daemon has exited, and its exit is reported, by the time the stop is seen
/// (`Began` is told between the wait's end and the plan, and the daemon ends there).
fn stopped_after_exit(fake: &FakeGuard, daemon: &FakeProcess, plan: StopPlan) -> (WaitOutcome, Vec<WaitEnd>) {
    let (supervisor, waiter, child) = launched(fake, daemon);
    assert!(supervisor.bus.request_stop());
    let mut told = Vec::new();
    let bus = Arc::clone(&supervisor.bus);
    let outcome = supervisor.wait_for_child_or_stop(waiter, &child, ID, plan, |event| {
        told.extend(began(event));
        daemon.end(0);
        bus.await_exit_report();
    });
    assert_eq!(fake.waiter_end(0), WaiterEnd::Returned);
    (outcome, told)
}

fn ask_and_wait(timeout: Option<Duration>) -> StopPlan {
    ask_and_wait_with(cosca::CtrlEvent::CtrlC, timeout)
}

fn ask_and_wait_with(event: cosca::CtrlEvent, timeout: Option<Duration>) -> StopPlan {
    StopPlan::AskAndWait { event, timeout }
}

/// The start of the line a stop logs before it asks, for the event named; what follows is how long the daemon is given.
fn asking_prefix(event: &str) -> String {
    format!("goetia-shim[{ID}]: asking the daemon to exit with {event}; the daemon and what it started have ")
}

/// The line an asked stop with no `timeout` logs before it asks.
fn asking_without_a_limit_line(event: &str) -> String {
    format!(
        "goetia-shim[{ID}]: asking the daemon to exit with {event}; the daemon and what it started are waited for with no time limit"
    )
}

/// What a stop with a `timeout` logged: the line before the first ask, which says how long the daemon is given (what
/// is left of the `timeout`, counted from when the stop was seen), and the lines after it. Both are returned.
fn told_asking(mut infos: Vec<String>, event: &str) -> (Duration, Vec<String>) {
    assert!(!infos.is_empty(), "no line told the ask of {event}");
    let first = infos.remove(0);
    let prefix = asking_prefix(event);
    let given = first
        .strip_prefix(&prefix)
        .and_then(|rest| rest.strip_suffix(" to exit"))
        .unwrap_or_else(|| panic!("not the line before an ask of {event}: {first:?}"));
    let given = humantime::parse_duration(given).unwrap_or_else(|e| panic!("{given:?} is no duration: {e}"));
    (given, infos)
}

/// [`told_asking`] for a `timeout`: what is left of it is at most it, as it counts from the moment the stop was seen.
fn told_asking_within(timeout: Duration, infos: Vec<String>, event: &str) -> Vec<String> {
    let (given, rest) = told_asking(infos, event);
    assert!(
        given <= timeout,
        "the line says {given:?} are given of {timeout:?}, which counts from the moment the stop was seen"
    );
    rest
}

/// [`told_asking_within`] a `timeout` of an hour, of which more than half is left: nothing a stop does before its first
/// ask takes half an hour.
fn told_asking_within_the_hour(infos: Vec<String>, event: &str) -> Vec<String> {
    let (given, rest) = told_asking(infos, event);
    assert!(
        given <= HOUR && given > HOUR / 2,
        "the line says {given:?} are given of {HOUR:?}, which counts from the moment the stop was seen"
    );
    rest
}

/// The line of an asked stop whose daemon exited before the event could be sent.
fn exited_unasked_line() -> String {
    format!("goetia-shim[{ID}]: the daemon exited before it could be asked")
}

fn exited_line() -> String {
    format!("goetia-shim[{ID}]: the daemon and what it started exited within the wait")
}

/// The warning of an asked stop whose daemon's tree had not been seen to exit by the end of its `timeout`.
fn did_not_exit_line(timeout: Duration) -> String {
    format!(
        "goetia-shim[{ID}]: the daemon's process tree had not been seen to exit when its timeout of {} was over; it is being ended",
        humantime::format_duration(timeout)
    )
}

/// The warning of an ask that may have sent `event`, whose answer was `error`.
fn may_have_sent_line(event: &str, error: &cosca::error::Error) -> String {
    format!(
        "goetia-shim[{ID}]: the stop may have sent {event} to ask the daemon to exit: {}; waiting for it as if it had been asked",
        Chain(error)
    )
}

/// A daemon that runs until its tree is killed, and that the ask `reply`s to.
fn asked(reply: AskReply) -> FakeProcess {
    FakeProcess::new()
        .wait(WaitReply::Blocks)
        .ask(reply)
        .send_kill_tree(KillReply::Ends)
}

#[skuld::test]
fn began_is_told_a_stop_from_an_exit() {
    let fake = FakeGuard::new();
    let (_, told) = stopped_by(&fake, &FakeProcess::running(), StopPlan::AtOnce);
    assert_eq!(told, [WaitEnd::Stop]);

    let fake = FakeGuard::new();
    let daemon = FakeProcess::new().wait(WaitReply::Exits(0));
    let (supervisor, waiter, child) = launched(&fake, &daemon);
    let mut told = Vec::new();
    let outcome = supervisor.wait_for_child_or_stop(waiter, &child, ID, ask_and_wait(Some(HOUR)), |event| {
        told.extend(began(event))
    });

    assert!(matches!(outcome, WaitOutcome::ChildExited));
    assert_eq!(told, [WaitEnd::Exit], "the program's own exit was told as a stop");
    assert_eq!(fake.waiter_end(0), WaiterEnd::Returned);
}

/// A stop and an exit at the wake: the process is gone when the stop is seen, so the callback is told the daemon's
/// exit, not a stop, and no plan runs for it: it is neither asked nor waited for, and its tree is ended as after any
/// exit. The waiter's report of the exit lags (it is held until the process itself is read), so it is the process that
/// is checked.
#[skuld::test]
fn a_stop_that_finds_the_daemon_gone_is_told_as_its_exit_and_runs_no_plan() {
    for plan in [ask_and_wait(Some(HOUR)), StopPlan::WaitOnly] {
        let fake = FakeGuard::new();
        let daemon = FakeProcess::new()
            .exited_unreported(0)
            .wait(WaitReply::Blocks)
            .send_kill_tree(KillReply::Ends);

        let (outcome, told) = stopped_by(&fake, &daemon, plan);

        assert!(root_exited(&outcome), "{plan:?}");
        assert_eq!(told, [WaitEnd::Exit], "{plan:?}");
        assert_eq!(daemon.calls(), [ProcessCall::SendKillTree], "{plan:?}");
        assert_eq!(fake.take_failures(), Vec::<String>::new());
        assert_eq!(fake.take_infos(), Vec::<String>::new(), "{plan:?}: an ask was told");
    }
}

/// The read of the process fails, and the waiter has reported the exit by the time it does (the effect stands for
/// that): the daemon is gone all the same. It is told as its exit, neither asked nor waited for, and its tree is
/// ended as after any exit; the failed read is not told, since the report says what it could not.
#[skuld::test]
fn a_stop_whose_read_of_the_daemon_fails_after_its_exit_was_reported_finds_it_gone() {
    for plan in [ask_and_wait(Some(HOUR)), StopPlan::WaitOnly] {
        let fake = FakeGuard::new();
        let daemon = FakeProcess::new()
            .wait(WaitReply::Exits(0))
            .try_wait_fails()
            .send_kill_tree(KillReply::Ends);
        let (supervisor, waiter, child) = launched(&fake, &daemon);
        let bus = Arc::clone(&supervisor.bus);
        let _ = daemon.clone().when(Op::TryWait, move || bus.await_exit_report());
        assert!(supervisor.bus.request_stop());
        let mut told = Vec::new();

        let outcome = supervisor.wait_for_child_or_stop(waiter, &child, ID, plan, |event| told.extend(began(event)));
        assert_eq!(fake.waiter_end(0), WaiterEnd::Returned);

        assert!(root_exited(&outcome), "{plan:?}");
        assert_eq!(told, [WaitEnd::Exit], "{plan:?}");
        assert_eq!(daemon.calls(), [ProcessCall::SendKillTree], "{plan:?}");
        assert_eq!(fake.take_failures(), Vec::<String>::new());
        assert_eq!(
            fake.take_warnings(),
            Vec::<String>::new(),
            "{plan:?}: the report settles it, so a read that failed has nothing to say"
        );
        assert_eq!(fake.take_infos(), Vec::<String>::new(), "{plan:?}: an ask was told");
    }
}

/// The read at the stop gave the exit status, so that is the root's exit and the teardown is the exited tree's, whatever
/// the waiter reports after the read: here its wait fails. The root is not killed, as it is known gone.
#[skuld::test]
fn a_stop_that_read_the_daemon_gone_keeps_its_exit_when_the_waiter_fails_after_the_read() {
    for plan in [ask_and_wait(Some(HOUR)), StopPlan::WaitOnly, StopPlan::AtOnce] {
        let fake = FakeGuard::new();
        let daemon = FakeProcess::new()
            .exited_unreported(5)
            .wait(WaitReply::FailsWhenEnded)
            .send_kill_tree(KillReply::Refuses);

        let (outcome, told) = stopped_by(&fake, &daemon, plan);

        assert!(
            matches!(outcome, WaitOutcome::TreeMayRun),
            "{plan:?}: the exited tree's teardown, whose refusal is a tree left"
        );
        assert_eq!(told, [WaitEnd::Exit], "{plan:?}");
        assert_eq!(
            daemon.calls(),
            [ProcessCall::SendKillTree],
            "{plan:?}: the root was killed"
        );
        // The waiter's line and the teardown's are made on two threads, in no fixed order.
        let mut failures = fake.take_failures();
        failures.sort();
        let mut expected = vec![
            format!("goetia-shim[{ID}]: background wait on child: {}", Chain(&wait_failed())),
            format!(
                "goetia-shim[{ID}]: terminating the process tree after the daemon ended failed: {}; its descendants may still be running{NOT_RESTARTED}",
                Chain(&tree_kill_refused())
            ),
        ];
        expected.sort();
        assert_eq!(failures, expected, "{plan:?}");
    }
}

/// As above, with the tree kill sent: the outcome carries the status the read gave.
#[skuld::test]
fn a_stop_that_read_the_daemon_gone_reports_the_status_it_read() {
    for plan in [ask_and_wait(Some(HOUR)), StopPlan::WaitOnly, StopPlan::AtOnce] {
        let fake = FakeGuard::new();
        let daemon = FakeProcess::new()
            .exited_unreported(5)
            .wait(WaitReply::FailsWhenEnded)
            .send_kill_tree(KillReply::Ends);

        let (outcome, _) = stopped_by(&fake, &daemon, plan);

        let WaitOutcome::Stopping(RootExit::Status(status)) = outcome else {
            panic!("{plan:?}: not a stop of a root that exited with a status");
        };
        assert_eq!(status.code(), Some(5), "{plan:?}");
        assert_eq!(daemon.calls(), [ProcessCall::SendKillTree], "{plan:?}");
        assert_eq!(fake.take_failures().len(), 1, "{plan:?}: the waiter's failure");
    }
}

/// The same where the read that found the daemon gone is one the ask makes before it asks again, not the one at the stop.
#[skuld::test]
fn a_read_before_a_second_ask_that_finds_the_daemon_gone_keeps_its_exit_when_the_waiter_fails() {
    let fake = FakeGuard::new();
    let daemon = FakeProcess::new()
        .wait(WaitReply::FailsWhenEnded)
        .ask(AskReply::UnsupportedAndExitedUnreported(5))
        .send_kill_tree(KillReply::Ends);

    let (outcome, told) = stopped_by(&fake, &daemon, ask_and_wait(Some(HOUR)));

    let WaitOutcome::Stopping(RootExit::Status(status)) = outcome else {
        panic!("not a stop of a root that exited with a status");
    };
    assert_eq!(status.code(), Some(5));
    assert_eq!(told, [WaitEnd::Stop]);
    assert_eq!(daemon.calls(), [ProcessCall::Ask, ProcessCall::SendKillTree]);
    assert_eq!(fake.take_failures().len(), 1, "the waiter's failure");
    assert_eq!(fake.take_warnings(), [unsupported_warning()]);
    assert_eq!(
        told_asking_within_the_hour(fake.take_infos(), "Ctrl+C"),
        [exited_unasked_line()]
    );
}

/// The stop is seen with the daemon alive and the waiter fails afterwards, while the plan waits: nothing says the
/// daemon ended, so it is killed, and the stop's record is `Stopped`, with no exit.
#[skuld::test]
fn a_waiter_that_fails_while_a_wait_only_plan_waits_gets_the_daemon_killed() {
    let fake = FakeGuard::new();
    let daemon = FakeProcess::new()
        .wait(WaitReply::Blocks)
        .send_kill_tree(KillReply::Ends);
    let (supervisor, waiter, child) = launched(&fake, &daemon);
    assert!(supervisor.bus.request_stop());
    let mut told = Vec::new();

    let outcome = supervisor.wait_for_child_or_stop(waiter, &child, ID, StopPlan::WaitOnly, |event| {
        told.extend(began(event));
        daemon.fail_wait();
    });

    assert_eq!(told, [WaitEnd::Stop]);
    assert!(
        matches!(outcome, WaitOutcome::Stopping(RootExit::NotSeen)),
        "the daemon is not known to have exited"
    );
    assert_eq!(daemon.calls(), [ProcessCall::SendKillTree]);
    assert!(daemon.killed(), "the daemon was left running");
    assert_eq!(fake.waiter_end(0), WaiterEnd::Returned);
    assert_eq!(
        fake.take_failures(),
        [format!(
            "goetia-shim[{ID}]: background wait on child: {}",
            Chain(&wait_failed())
        )]
    );
}

/// The daemon exits right after the stop is seen, before anything is asked: the stop was seen with the daemon alive,
/// so it is told as a stop, and the exit is only what the teardown choice rests on.
#[skuld::test]
fn a_daemon_that_exited_after_the_stop_was_seen_is_neither_asked_nor_waited_for() {
    let fake = FakeGuard::new();
    let daemon = FakeProcess::new()
        .wait(WaitReply::Blocks)
        .send_kill_tree(KillReply::Ends);

    let (outcome, told) = stopped_after_exit(&fake, &daemon, ask_and_wait(Some(HOUR)));

    assert!(root_exited(&outcome));
    assert_eq!(told, [WaitEnd::Stop]);
    assert_eq!(daemon.calls(), [ProcessCall::SendKillTree]);
    assert_eq!(fake.take_failures(), Vec::<String>::new());
}

/// The teardown of a daemon that exited is the exited-tree one: no direct-child kill to fall back to, and
/// no claim that only the direct child was killed.
#[skuld::test]
fn a_daemon_that_exited_after_the_stop_was_seen_gets_the_exited_tree_teardown() {
    let fake = FakeGuard::new();
    let daemon = FakeProcess::new()
        .wait(WaitReply::Blocks)
        .send_kill_tree(KillReply::Refuses);

    let (outcome, _) = stopped_after_exit(&fake, &daemon, ask_and_wait(Some(HOUR)));

    assert!(matches!(outcome, WaitOutcome::TreeMayRun));
    assert_eq!(
        daemon.calls(),
        [ProcessCall::SendKillTree],
        "the direct child was killed"
    );
    assert_failures(
        &fake.take_failures(),
        ID,
        &[format!(
            "terminating the process tree after the daemon ended failed: {}; its descendants may still be running{NOT_RESTARTED}",
            Chain(&tree_kill_refused())
        )],
    );
}

#[skuld::test]
fn ask_and_wait_asks_once_and_waits_for_the_timeout() {
    let fake = FakeGuard::new();
    let daemon = asked(AskReply::SentAndExits(0)).wait_tree(TreeWaitReply::Drained);

    let (outcome, told) = stopped_by(&fake, &daemon, ask_and_wait(Some(HOUR)));

    assert!(matches!(outcome, WaitOutcome::Stopping { .. }));
    assert_eq!(told, [WaitEnd::Stop]);
    let [ProcessCall::Ask, ProcessCall::WaitTree(left), ProcessCall::SendKillTree] = daemon.calls()[..] else {
        panic!("asked, waited, then ended: {:?}", daemon.calls());
    };
    assert_eq!(
        left, PROGRESS_PERIOD,
        "the first slice of a wait that is longer than a period"
    );
    assert_eq!(
        daemon.exit_code(),
        Some(0),
        "the daemon was ended, not left to exit by itself"
    );
    assert_eq!(fake.take_failures(), Vec::<String>::new());
    assert_eq!(fake.take_warnings(), Vec::<String>::new());
    assert_eq!(
        told_asking_within_the_hour(fake.take_infos(), "Ctrl+C"),
        [exited_line()]
    );
}

#[skuld::test]
fn a_timeout_of_none_is_waited_for_in_slices_that_never_run_out() {
    let fake = FakeGuard::new();
    let daemon = asked(AskReply::SentAndExits(0)).wait_tree(TreeWaitReply::Drained);

    stopped_by(&fake, &daemon, ask_and_wait(None));

    assert_eq!(
        daemon.calls(),
        [
            ProcessCall::Ask,
            ProcessCall::WaitTree(PROGRESS_PERIOD),
            ProcessCall::SendKillTree
        ]
    );
    let _ = fake.take_infos();
}

/// [`stopped_by`] with the progress reports recorded: how many waits for the tree had been made at each.
fn stopped_recording_progress(fake: &FakeGuard, daemon: &FakeProcess, plan: StopPlan) -> (WaitOutcome, Vec<usize>) {
    let (supervisor, waiter, child) = launched(fake, daemon);
    assert!(supervisor.bus.request_stop());
    let mut waits = Vec::new();
    let outcome = supervisor.wait_for_child_or_stop(waiter, &child, ID, plan, |event| {
        if event == StopEvent::Progress {
            waits.push(fake.calls(Op::WaitTree));
        }
    });
    assert_eq!(fake.waiter_end(0), WaiterEnd::Returned);
    (outcome, waits)
}

/// The slices each wait for the tree was cut into, in order.
fn slices_of(daemon: &FakeProcess) -> Vec<Duration> {
    daemon
        .calls()
        .into_iter()
        .filter_map(|call| match call {
            ProcessCall::WaitTree(slice) => Some(slice),
            _ => None,
        })
        .collect()
}

/// A short period and a deadline, over a daemon whose every slice expires with members left after `late` more than it
/// was asked for. What the answer, the slices and the reports hold, whatever the scheduler does.
fn wait_to_a_deadline(
    timeout: Duration,
    period: Duration,
    late: Duration,
) -> (Result<TreeDrain, cosca::error::Error>, Vec<Duration>, usize) {
    let fake = FakeGuard::new();
    let daemon = FakeProcess::new().wait_tree(TreeWaitReply::Expires(late));
    fake.will_spawn(daemon.clone());
    let child = spawn_child(&fake);
    let mut reports = 0;

    let deadline = Instant::now() + timeout;
    let answer = wait_for_tree(&*child, Some(deadline), period, &mut || reports += 1);

    (answer, slices_of(&daemon), reports)
}

/// No slice is longer than the period, nor than what was left to the deadline when it was asked. Every slice that
/// returns blocks for at least its length, so what was left when one was asked is at most the `timeout` less the
/// slices before it. A report follows each slice that left members but not the one after which the deadline had
/// passed, which is the last.
#[skuld::test]
fn no_slice_is_longer_than_the_period_or_than_what_was_left_to_the_deadline() {
    let period = Duration::from_millis(10);
    let timeout = Duration::from_millis(55);

    let (answer, slices, reports) = wait_to_a_deadline(timeout, period, Duration::ZERO);

    assert!(matches!(answer, Ok(TreeDrain::MembersRemain)));
    let mut before = Duration::ZERO;
    for slice in &slices {
        assert!(*slice <= period, "{slices:?}");
        assert!(*slice <= timeout - before, "{slices:?}");
        before += *slice;
    }
    assert_eq!(
        reports,
        slices.len() - 1,
        "a report after each slice but the last: {slices:?}"
    );
}

/// A wait ends once its deadline has passed, however long its slices took: a slice that woke late is not made up for by
/// another. Two slices that overshoot by 15 ms already pass a deadline of 40 ms, however much the scheduler adds.
#[skuld::test]
fn a_wait_ends_once_its_deadline_has_passed_whatever_its_slices_took() {
    let (answer, slices, reports) = wait_to_a_deadline(
        Duration::from_millis(40),
        Duration::from_millis(10),
        Duration::from_millis(15),
    );

    assert!(matches!(answer, Ok(TreeDrain::MembersRemain)));
    assert!(slices.len() <= 2, "{slices:?}");
    assert_eq!(reports, slices.len() - 1, "{slices:?}");
}

/// A tree that drains ends the wait at the slice that sees it: no report is made after it.
#[skuld::test]
fn the_stop_is_not_reported_after_the_slice_that_saw_the_tree_exit() {
    let fake = FakeGuard::new();
    let daemon = asked(AskReply::Sent)
        .wait_tree(TreeWaitReply::Remain)
        .wait_tree(TreeWaitReply::Drained);

    let (outcome, progress) = stopped_recording_progress(&fake, &daemon, ask_and_wait(Some(HOUR)));

    assert!(matches!(outcome, WaitOutcome::Stopping { .. }));
    assert_eq!(
        daemon.calls(),
        [
            ProcessCall::Ask,
            ProcessCall::WaitTree(PROGRESS_PERIOD),
            ProcessCall::WaitTree(PROGRESS_PERIOD),
            ProcessCall::SendKillTree
        ]
    );
    assert_eq!(progress, [1]);
    assert_eq!(fake.take_warnings(), Vec::<String>::new());
    assert_eq!(fake.take_failures(), Vec::<String>::new());
    let _ = fake.take_infos();
}

/// With no `timeout` the slices have no end of their own: members left is not a verdict, only the tree's exit, or a
/// failed wait, ends them.
#[skuld::test]
fn with_no_timeout_the_slices_go_on_until_the_tree_exits() {
    let fake = FakeGuard::new();
    let daemon = asked(AskReply::Sent)
        .wait_tree(TreeWaitReply::Remain)
        .wait_tree(TreeWaitReply::Remain)
        .wait_tree(TreeWaitReply::Remain)
        .wait_tree(TreeWaitReply::Drained);

    let (_, progress) = stopped_recording_progress(&fake, &daemon, ask_and_wait(None));

    let slices = vec![ProcessCall::WaitTree(PROGRESS_PERIOD); 4];
    let [ProcessCall::Ask, rest @ .., ProcessCall::SendKillTree] = &daemon.calls()[..] else {
        panic!("asked, waited, then ended: {:?}", daemon.calls());
    };
    assert_eq!(rest, &slices[..]);
    assert_eq!(progress, [1, 2, 3]);
    assert_eq!(fake.take_warnings(), Vec::<String>::new());
    let _ = fake.take_infos();
}

/// A `timeout` within one period is one slice, after which the wait has ended: nothing to report.
#[skuld::test]
fn a_timeout_within_one_period_is_one_slice_and_no_report() {
    let fake = FakeGuard::new();
    let daemon = asked(AskReply::Sent).wait_tree(TreeWaitReply::Expires(Duration::ZERO));

    let (_, progress) = stopped_recording_progress(&fake, &daemon, ask_and_wait(Some(BRIEF)));

    let [
        ProcessCall::Ask,
        ProcessCall::WaitTree(slice),
        ProcessCall::SendKillTree,
    ] = daemon.calls()[..]
    else {
        panic!("asked, waited once, then ended: {:?}", daemon.calls());
    };
    assert!(slice <= BRIEF, "{slice:?}");
    assert_eq!(progress, Vec::<usize>::new());
    assert_eq!(fake.take_warnings(), [did_not_exit_line(BRIEF)]);
    let _ = fake.take_infos();
}

/// The period is the supervisor's: a short one cuts a short `timeout` into slices, none longer than it, and the wait
/// ends with the timeout over once the deadline has passed. No count of slices is stated: it depends on the scheduler.
#[skuld::test]
fn a_short_period_cuts_a_short_timeout_into_slices_and_ends_it_at_its_deadline() {
    let fake = FakeGuard::new();
    let period = Duration::from_millis(5);
    let timeout = Duration::from_millis(30);
    let daemon = asked(AskReply::Sent).wait_tree(TreeWaitReply::Expires(Duration::ZERO));
    let (supervisor, waiter, child) = launched_with_period(&fake, &daemon, period);
    assert!(supervisor.bus.request_stop());
    let mut reports = 0;

    let outcome = supervisor.wait_for_child_or_stop(waiter, &child, ID, ask_and_wait(Some(timeout)), |event| {
        reports += usize::from(event == StopEvent::Progress)
    });

    assert!(matches!(outcome, WaitOutcome::Stopping { .. }));
    assert_eq!(fake.waiter_end(0), WaiterEnd::Returned);
    let slices = slices_of(&daemon);
    assert!(slices.iter().all(|slice| *slice <= period), "{slices:?}");
    assert_eq!(reports, slices.len() - 1, "{slices:?}");
    assert_eq!(fake.take_warnings(), [did_not_exit_line(timeout)]);
    assert_eq!(fake.take_failures(), Vec::<String>::new());
    let _ = fake.take_infos();
}

/// While the ask is answered "no console yet" the stop is asked again, at the pace of the re-asks, and each pass that
/// has not ended the asking reports the stop as going on: before any wait for the tree is made.
#[skuld::test]
fn the_reasks_report_the_stop_as_going_on_at_their_own_pace() {
    let fake = FakeGuard::new();
    let daemon = asked(AskReply::Unsupported)
        .ask(AskReply::Unsupported)
        .ask(AskReply::SentAndExits(0))
        .wait_tree(TreeWaitReply::Drained);

    let (outcome, progress) = stopped_recording_progress(&fake, &daemon, ask_and_wait(Some(HOUR)));

    assert!(matches!(outcome, WaitOutcome::Stopping { .. }));
    assert_eq!(progress, [0, 0], "one report after each answer that was asked again");
    let _ = (fake.take_warnings(), fake.take_infos());
}

/// A daemon that ignores the event is ended when the wait's bound passes, by the teardown that kills.
#[skuld::test]
fn a_daemon_that_ignores_the_ask_is_killed_when_the_wait_expires() {
    let fake = FakeGuard::new();
    let daemon = asked(AskReply::Sent).wait_tree(TreeWaitReply::Expires(Duration::ZERO));

    let (outcome, _) = stopped_by(&fake, &daemon, ask_and_wait(Some(BRIEF)));

    assert!(matches!(outcome, WaitOutcome::Stopping { .. }));
    assert!(daemon.killed(), "the daemon was left running after its timeout");
    let [ProcessCall::Ask, ProcessCall::WaitTree(_), ProcessCall::SendKillTree] = daemon.calls()[..] else {
        panic!("asked, waited, then ended: {:?}", daemon.calls());
    };
    assert_eq!(fake.take_failures(), Vec::<String>::new());
    assert_eq!(
        told_asking_within(BRIEF, fake.take_infos(), "Ctrl+C"),
        Vec::<String>::new()
    );
    assert_eq!(fake.take_warnings(), [did_not_exit_line(BRIEF)]);
}

/// One line for an asked stop, saying what happened: the ask, then how the wait ended.
#[skuld::test]
fn an_asked_stop_tells_the_event_it_sent_and_that_the_daemon_exited_within_the_wait() {
    let events = [
        (cosca::CtrlEvent::CtrlC, "Ctrl+C"),
        (cosca::CtrlEvent::CtrlBreak, "Ctrl+Break"),
    ];
    for (event, named) in events {
        let fake = FakeGuard::new();
        let daemon = asked(AskReply::Sent).wait_tree(TreeWaitReply::Drained);

        stopped_by(&fake, &daemon, ask_and_wait_with(event, Some(HOUR)));

        assert_eq!(
            told_asking_within_the_hour(fake.take_infos(), named),
            [exited_line()],
            "{named}"
        );
        assert_eq!(fake.take_warnings(), Vec::<String>::new(), "{named}");
        assert_eq!(fake.take_failures(), Vec::<String>::new(), "{named}");
    }
}

#[skuld::test]
fn an_asked_stop_with_no_timeout_says_it_waits_without_a_limit() {
    let fake = FakeGuard::new();
    let daemon = asked(AskReply::Sent).wait_tree(TreeWaitReply::Drained);

    stopped_by(&fake, &daemon, ask_and_wait(None));

    assert_eq!(fake.take_infos()[0], asking_without_a_limit_line("Ctrl+C"));
}

/// What the wait left is ended, and the log says the daemon did not exit by itself: one warning, no line that
/// says it did.
#[skuld::test]
fn an_asked_stop_that_ends_the_daemon_at_the_end_of_the_wait_says_so_in_one_warning() {
    let fake = FakeGuard::new();
    let daemon = asked(AskReply::Sent).wait_tree(TreeWaitReply::Expires(Duration::ZERO));

    stopped_by(&fake, &daemon, ask_and_wait(Some(BRIEF)));

    assert_eq!(
        told_asking_within(BRIEF, fake.take_infos(), "Ctrl+C"),
        Vec::<String>::new()
    );
    assert_eq!(fake.take_warnings(), [did_not_exit_line(BRIEF)]);
    assert_eq!(fake.take_failures(), Vec::<String>::new());
}

/// `AllMarkersClosed` is advisory (no proof that a process is gone) and is no `AllMembersExited`: no line says the daemon
/// exited, the exit is not taken as reported, and the line claims only the closing of the marker. The daemon is
/// ended by the teardown that kills (a Job Object never answers this; the arm is the honest one for a verdict that
/// is not the one asked for).
#[skuld::test]
fn a_wait_that_ends_on_a_closed_marker_does_not_say_the_daemon_exited() {
    let fake = FakeGuard::new();
    let daemon = asked(AskReply::Sent).wait_tree(TreeWaitReply::MarkersClosed);

    let (outcome, _) = stopped_by(&fake, &daemon, ask_and_wait(Some(HOUR)));

    assert!(matches!(outcome, WaitOutcome::Stopping { .. }));
    assert!(daemon.killed(), "the daemon was left running");
    assert_eq!(
        fake.take_warnings(),
        [format!(
            "goetia-shim[{ID}]: the wait for the daemon's process tree ended when the last copy of its marker was closed, which is no proof that every process in it exited; it is being ended"
        )]
    );
    assert_eq!(
        told_asking_within_the_hour(fake.take_infos(), "Ctrl+C"),
        Vec::<String>::new(),
        "no line says the daemon exited"
    );
    assert_eq!(fake.take_failures(), Vec::<String>::new());
}

/// Nothing was asked, so nothing is told: the stops that end the daemon at once, and a daemon that had exited.
#[skuld::test]
fn a_stop_that_asks_nothing_logs_no_information() {
    let fake = FakeGuard::new();
    let daemon = FakeProcess::new()
        .wait(WaitReply::Blocks)
        .send_kill_tree(KillReply::Ends);
    stopped_by(&fake, &daemon, StopPlan::AtOnce);
    assert_eq!(fake.take_infos(), Vec::<String>::new(), "at once");

    let fake = FakeGuard::new();
    let daemon = FakeProcess::new()
        .wait(WaitReply::Blocks)
        .send_kill_tree(KillReply::Ends);
    stopped_after_exit(&fake, &daemon, ask_and_wait(Some(HOUR)));
    assert_eq!(fake.take_infos(), Vec::<String>::new(), "a daemon that had exited");
}

/// A drained tree has no member left, the root included, so its exit report is certain to come, and
/// the teardown is chosen by it: the exited-tree teardown, whatever the order the threads ran in.
#[skuld::test]
fn a_drained_tree_gets_the_exited_tree_teardown() {
    let fake = FakeGuard::new();
    let daemon = FakeProcess::new()
        .wait(WaitReply::Blocks)
        .ask(AskReply::Sent)
        .wait_tree(TreeWaitReply::Drained)
        .send_kill_tree(KillReply::Refuses);

    let (outcome, _) = stopped_by(&fake, &daemon, ask_and_wait(Some(HOUR)));

    assert!(matches!(outcome, WaitOutcome::TreeMayRun));
    assert!(
        !daemon.calls().contains(&ProcessCall::Kill),
        "the direct child was killed"
    );
    assert_eq!(fake.take_failures().len(), 1);
    assert_eq!(
        told_asking_within_the_hour(fake.take_infos(), "Ctrl+C"),
        [exited_line()]
    );
}

/// A wait-only stop sends the program nothing and waits for its own exit, with no cap of goetia's; only then is
/// the tree torn down. The program here is ended by the test, after the supervisor has passed `Began`.
#[skuld::test]
fn wait_only_waits_for_the_programs_own_exit_and_then_tears_down() {
    let fake = FakeGuard::new();
    let daemon = FakeProcess::new()
        .wait(WaitReply::Blocks)
        .send_kill_tree(KillReply::Ends);
    let (supervisor, waiter, child) = launched(&fake, &daemon);
    let (stopping, told) = mpsc::channel();
    let progress = std::sync::atomic::AtomicUsize::new(0);

    let outcome = std::thread::scope(|scope| {
        let running = scope.spawn(|| {
            supervisor.wait_for_child_or_stop(waiter, &child, ID, StopPlan::WaitOnly, |event| match event {
                StopEvent::Began(end) => stopping.send(end).expect("the test is listening"),
                StopEvent::Progress => {
                    progress.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                }
            })
        });
        assert!(supervisor.bus.request_stop());
        assert_eq!(told.recv().expect("`Began` was told"), WaitEnd::Stop);
        daemon.end(0);
        running.join().expect("the supervisor did not panic")
    });

    assert_eq!(
        daemon.ended_at_teardown(),
        Some(true),
        "the program was ended before it exited by itself"
    );
    assert!(root_exited(&outcome));
    assert_eq!(daemon.exit_code(), Some(0));
    assert_eq!(
        daemon.calls(),
        [ProcessCall::SendKillTree],
        "the teardown came once, after the exit"
    );
    assert_eq!(fake.calls(Op::Ask), 0);
    assert_eq!(fake.calls(Op::WaitTree), 0);
    assert_eq!(
        progress.load(std::sync::atomic::Ordering::Relaxed),
        0,
        "a task's stop reports nothing: it waits for the program alone"
    );
    assert_eq!(fake.waiter_end(0), WaiterEnd::Returned);
    assert_eq!(fake.take_failures(), Vec::<String>::new());
    assert_eq!(
        fake.take_infos(),
        Vec::<String>::new(),
        "a wait-only stop tells nothing of an ask"
    );
}

/// A program that exits just after the stop is seen is not waited for beyond its report: it is torn down as after any
/// exit.
#[skuld::test]
fn wait_only_does_not_wait_for_a_program_that_exited_after_the_stop_was_seen() {
    let fake = FakeGuard::new();
    let daemon = FakeProcess::new()
        .wait(WaitReply::Blocks)
        .send_kill_tree(KillReply::Ends);

    let (outcome, told) = stopped_after_exit(&fake, &daemon, StopPlan::WaitOnly);

    assert!(matches!(outcome, WaitOutcome::Stopping { .. }));
    assert_eq!(told, [WaitEnd::Stop]);
    assert_eq!(daemon.calls(), [ProcessCall::SendKillTree]);
}

#[skuld::test]
fn at_once_asks_and_waits_for_nothing() {
    let fake = FakeGuard::new();
    let daemon = FakeProcess::running();

    let (outcome, _) = stopped_by(&fake, &daemon, StopPlan::AtOnce);

    assert!(
        matches!(outcome, WaitOutcome::Stopping(RootExit::NotSeen)),
        "the teardown ended it"
    );
    assert_eq!(daemon.calls(), [ProcessCall::SendKillTree]);
}

/// SCM can deliver a second `STOP` before `STOP_PENDING` is reported: it changes nothing, and the ask is
/// made once.
#[skuld::test]
fn a_second_stop_does_not_ask_again() {
    let fake = FakeGuard::new();
    let daemon = asked(AskReply::SentAndExits(0)).wait_tree(TreeWaitReply::Drained);
    let (supervisor, waiter, child) = launched(&fake, &daemon);
    assert!(supervisor.bus.request_stop());
    assert!(supervisor.bus.request_stop());

    let outcome = supervisor.wait_for_child_or_stop(waiter, &child, ID, ask_and_wait(Some(HOUR)), |_| {});

    assert!(matches!(outcome, WaitOutcome::Stopping { .. }));
    assert_eq!(fake.calls(Op::Ask), 1);
    assert_eq!(fake.waiter_end(0), WaiterEnd::Returned);
    assert_eq!(
        told_asking_within_the_hour(fake.take_infos(), "Ctrl+C"),
        [exited_line()],
        "the ask is told once, as it is made once"
    );
}

// The ask's answers ---------------------------------------------------------------------------------------------------

fn failure_line(text: String) -> [String; 1] {
    [text]
}

/// `Error::Io`: nothing was sent, and asking again cannot help: no wait, the teardown at once.
#[skuld::test]
fn an_ask_that_sent_nothing_is_logged_and_the_daemon_is_ended_at_once() {
    let fake = FakeGuard::new();
    let daemon = asked(AskReply::Io);

    let (outcome, _) = stopped_by(&fake, &daemon, ask_and_wait(Some(HOUR)));

    assert!(matches!(outcome, WaitOutcome::Stopping { .. }));
    assert_eq!(daemon.calls(), [ProcessCall::Ask, ProcessCall::SendKillTree]);
    assert_failures(
        &fake.take_failures(),
        ID,
        &failure_line(format!(
            "the stop could not ask the daemon to exit: {}; nothing was sent, so the daemon is ended without being asked",
            Chain(&ask_io())
        )),
    );
    assert_eq!(
        told_asking_within_the_hour(fake.take_infos(), "Ctrl+C"),
        Vec::<String>::new(),
        "the line comes before the ask, so an ask that sent nothing has it too"
    );
}

/// The answer is a warning, not a failure, and it says which event may have been sent.
#[skuld::test]
fn an_ask_that_may_have_been_sent_is_waited_for() {
    let answers: [(AskReply, fn() -> cosca::error::Error); 2] = [
        (AskReply::MayHaveStarted, ask_may_have_started),
        (AskReply::Unassessable, ask_unassessable),
    ];
    let events = [
        (cosca::CtrlEvent::CtrlC, "Ctrl+C"),
        (cosca::CtrlEvent::CtrlBreak, "Ctrl+Break"),
    ];
    for ((reply, error), (event, named)) in answers.into_iter().flat_map(|a| events.map(|e| (a, e))) {
        let fake = FakeGuard::new();
        let daemon = asked(reply).wait_tree(TreeWaitReply::Expires(Duration::ZERO));

        let (outcome, _) = stopped_by(&fake, &daemon, ask_and_wait_with(event, Some(BRIEF)));

        assert!(matches!(outcome, WaitOutcome::Stopping { .. }), "{reply:?}");
        let [ProcessCall::Ask, ProcessCall::WaitTree(_), ProcessCall::SendKillTree] = daemon.calls()[..] else {
            panic!("{reply:?}: asked, waited, then ended: {:?}", daemon.calls());
        };
        assert_eq!(fake.take_failures(), Vec::<String>::new(), "{reply:?}");
        assert_eq!(
            fake.take_warnings(),
            [may_have_sent_line(named, &error()), did_not_exit_line(BRIEF)],
            "{reply:?} {named}"
        );
        assert_eq!(
            told_asking_within(BRIEF, fake.take_infos(), named),
            Vec::<String>::new(),
            "{reply:?} {named}"
        );
    }
}

/// Right after a start the first ask is `Unsupported` (no console yet), and the second, a few
/// milliseconds later, is sent (measured).
#[skuld::test]
fn an_ask_that_is_unsupported_is_asked_again_until_it_is_sent() {
    let fake = FakeGuard::new();
    let daemon = asked(AskReply::Unsupported)
        .ask(AskReply::SentAndExits(0))
        .wait_tree(TreeWaitReply::Drained);

    let (outcome, _) = stopped_by(&fake, &daemon, ask_and_wait(Some(HOUR)));

    assert!(matches!(outcome, WaitOutcome::Stopping { .. }));
    let [
        ProcessCall::Ask,
        ProcessCall::Ask,
        ProcessCall::WaitTree(_),
        ProcessCall::SendKillTree,
    ] = daemon.calls()[..]
    else {
        panic!("asked twice, waited, then ended: {:?}", daemon.calls());
    };
    assert_eq!(fake.take_failures(), Vec::<String>::new());
    assert_eq!(fake.take_warnings(), [unsupported_warning()]);
    assert_eq!(
        told_asking_within_the_hour(fake.take_infos(), "Ctrl+C"),
        [exited_line()]
    );
}

fn unsupported_warning() -> String {
    format!(
        "goetia-shim[{ID}]: the daemon cannot be sent the exit event yet (it has no console, or never will): {}; asking again until it answers otherwise, the daemon exits or the timeout is over",
        Chain(&ask_unsupported())
    )
}

/// The daemon may or may not have been asked again before its exit is reported, so the answers after the first are
/// not counted.
#[skuld::test]
fn an_ask_that_is_unsupported_stops_when_the_daemon_exits() {
    let fake = FakeGuard::new();
    let daemon = FakeProcess::new()
        .wait(WaitReply::Blocks)
        .ask(AskReply::UnsupportedAndExits(3))
        .asking_always(AskReply::Unsupported)
        .send_kill_tree(KillReply::Ends);

    let (outcome, _) = stopped_by(&fake, &daemon, ask_and_wait(Some(HOUR)));

    assert!(matches!(outcome, WaitOutcome::Stopping { .. }));
    let calls = daemon.calls();
    assert_eq!(calls.first(), Some(&ProcessCall::Ask));
    assert_eq!(calls.last(), Some(&ProcessCall::SendKillTree));
    assert!(
        calls[1..calls.len() - 1].iter().all(|call| *call == ProcessCall::Ask),
        "the daemon had exited, yet it was waited for: {calls:?}"
    );
    assert_eq!(fake.take_failures(), Vec::<String>::new());
    assert_eq!(fake.take_warnings(), [unsupported_warning()]);
    assert_eq!(
        told_asking_within_the_hour(fake.take_infos(), "Ctrl+C"),
        [exited_unasked_line()]
    );
}

/// The supervisor reads the process itself, not only the waiter's report, which can lag the exit.
#[skuld::test]
fn a_daemon_that_has_exited_unreported_is_neither_asked_nor_waited_for() {
    let fake = FakeGuard::new();
    let daemon = FakeProcess::new()
        .wait(WaitReply::Blocks)
        .exited_unreported(3)
        .send_kill_tree(KillReply::Ends);

    let (outcome, _) = stopped_by(&fake, &daemon, ask_and_wait(Some(HOUR)));

    assert!(matches!(outcome, WaitOutcome::Stopping { .. }));
    assert_eq!(daemon.calls(), [ProcessCall::SendKillTree]);
    assert_eq!(fake.take_failures(), Vec::<String>::new());
    assert_eq!(fake.take_warnings(), Vec::<String>::new());
}

/// A daemon that exited after an `Unassessable` ask is still waited for (see `judge_ask`).
#[skuld::test]
fn an_ask_answered_unassessable_is_waited_for_though_the_daemon_has_exited() {
    let fake = FakeGuard::new();
    let daemon = FakeProcess::new()
        .wait(WaitReply::Blocks)
        .ask(AskReply::UnassessableAndExited(3))
        .wait_tree(TreeWaitReply::Drained)
        .send_kill_tree(KillReply::Ends);

    let (outcome, _) = stopped_by(&fake, &daemon, ask_and_wait(Some(HOUR)));

    assert!(matches!(outcome, WaitOutcome::Stopping { .. }));
    let [ProcessCall::Ask, ProcessCall::WaitTree(_), ProcessCall::SendKillTree] = daemon.calls()[..] else {
        panic!("asked, waited, then ended: {:?}", daemon.calls());
    };
    assert_eq!(daemon.exit_code(), Some(3));
    assert_eq!(fake.take_failures(), Vec::<String>::new());
    assert_eq!(
        fake.take_warnings(),
        [may_have_sent_line("Ctrl+C", &ask_unassessable())]
    );
    assert_eq!(
        told_asking_within_the_hour(fake.take_infos(), "Ctrl+C"),
        [exited_line()]
    );
}

/// The read of the process before an ask is made before every ask: a daemon that exited while it was being asked
/// again (and whose report lags) is not asked again, nor waited for. The fake has no reply for a second ask, so one
/// fails the test.
#[skuld::test]
fn a_daemon_that_exited_while_it_was_asked_again_is_not_asked_a_third_time() {
    let fake = FakeGuard::new();
    let daemon = FakeProcess::new()
        .wait(WaitReply::Blocks)
        .ask(AskReply::UnsupportedAndExitedUnreported(3))
        .send_kill_tree(KillReply::Ends);

    let (outcome, _) = stopped_by(&fake, &daemon, ask_and_wait(Some(HOUR)));

    assert!(matches!(outcome, WaitOutcome::Stopping { .. }));
    assert_eq!(daemon.calls(), [ProcessCall::Ask, ProcessCall::SendKillTree]);
    assert_eq!(fake.take_warnings(), [unsupported_warning()]);
    assert_eq!(
        told_asking_within_the_hour(fake.take_infos(), "Ctrl+C"),
        [exited_unasked_line()]
    );
    assert_eq!(fake.take_failures(), Vec::<String>::new());
}

/// An exit that cannot be read is told once for the stop, not once for every ask.
#[skuld::test]
fn an_exit_that_cannot_be_read_is_told_once_however_often_the_daemon_is_asked() {
    let fake = FakeGuard::new();
    let daemon = asked(AskReply::Unsupported)
        .ask(AskReply::Unsupported)
        .ask(AskReply::Sent)
        .try_wait_fails()
        .wait_tree(TreeWaitReply::Drained);

    stopped_by(&fake, &daemon, ask_and_wait(Some(HOUR)));

    assert_eq!(fake.calls(Op::Ask), 3);
    let unreadable = format!(
        "goetia-shim[{ID}]: could not read whether the daemon has exited: {}",
        Chain(&try_wait_failed())
    );
    assert_eq!(fake.take_warnings(), [unreadable, unsupported_warning()]);
    let _ = fake.take_infos();
}

/// A read of the exit that fails says nothing: the daemon is waited for as before, and the read's failure is
/// told beside the warning of the ask.
#[skuld::test]
fn an_exit_that_cannot_be_read_leaves_an_unassessable_ask_waited_for() {
    let fake = FakeGuard::new();
    let daemon = asked(AskReply::Unassessable)
        .try_wait_fails()
        .wait_tree(TreeWaitReply::Expires(Duration::ZERO));

    let (outcome, _) = stopped_by(&fake, &daemon, ask_and_wait(Some(BRIEF)));

    assert!(matches!(outcome, WaitOutcome::Stopping { .. }));
    let [ProcessCall::Ask, ProcessCall::WaitTree(_), ProcessCall::SendKillTree] = daemon.calls()[..] else {
        panic!("asked, waited, then ended: {:?}", daemon.calls());
    };
    assert_eq!(
        fake.take_warnings(),
        [
            format!(
                "goetia-shim[{ID}]: could not read whether the daemon has exited: {}",
                Chain(&try_wait_failed())
            ),
            may_have_sent_line("Ctrl+C", &ask_unassessable()),
            did_not_exit_line(BRIEF),
        ]
    );
    assert_eq!(
        told_asking_within(BRIEF, fake.take_infos(), "Ctrl+C"),
        Vec::<String>::new()
    );
}

/// The tree wait's deadline counts from when the stop was seen, not from when the wait begins: the time the re-asks
/// took is spent. One `Unsupported` answer is followed by the first pause, which never ends before it is over, so the
/// first slice, which the period does not cut here, is at most the `timeout` less that pause.
#[skuld::test]
fn the_tree_wait_counts_its_timeout_from_when_the_stop_was_seen() {
    let fake = FakeGuard::new();
    let timeout = Duration::from_millis(200);
    let daemon = asked(AskReply::Unsupported)
        .ask(AskReply::Sent)
        .wait_tree(TreeWaitReply::Expires(Duration::ZERO));
    let (supervisor, waiter, child) = launched_with_period(&fake, &daemon, HOUR);
    assert!(supervisor.bus.request_stop());

    supervisor.wait_for_child_or_stop(waiter, &child, ID, ask_and_wait(Some(timeout)), |_| {});

    assert_eq!(fake.waiter_end(0), WaiterEnd::Returned);
    let slices = slices_of(&daemon);
    let first = *slices.first().expect("the tree was waited for");
    assert!(
        first <= timeout - REASK_PACE_FIRST,
        "the wait did not count the pause before it: {first:?} of {timeout:?}"
    );
    let _ = (fake.take_warnings(), fake.take_infos(), fake.take_failures());
}

/// Every pass of `Unsupported` starts a proxy, so the pauses between passes grow: the first re-ask is fast, and
/// the pauses stop growing at the ceiling.
#[skuld::test]
fn the_pauses_between_reasks_grow_and_stop_growing_at_the_ceiling() {
    let mut pace = REASK_PACE_FIRST;
    let mut paces = vec![pace];
    for _ in 0..8 {
        pace = grown_pace(pace);
        paces.push(pace);
    }
    let ms: Vec<u128> = paces.iter().map(Duration::as_millis).collect();
    assert_eq!(ms, [25, 50, 100, 200, 400, 800, 1000, 1000, 1000]);
}

/// The loop takes the growing pauses: three `Unsupported` answers are followed by pauses of at least
/// 25, 50 and 100 ms. A lower bound only: a pause never ends before it is over, so a slow host cannot fail it.
#[skuld::test]
fn the_loop_pauses_longer_between_each_unsupported_answer() {
    let fake = FakeGuard::new();
    let daemon = asked(AskReply::Unsupported)
        .ask(AskReply::Unsupported)
        .ask(AskReply::Unsupported)
        .ask(AskReply::Sent)
        .wait_tree(TreeWaitReply::Drained);
    let started = Instant::now();

    let (outcome, _) = stopped_by(&fake, &daemon, ask_and_wait(Some(HOUR)));

    assert!(matches!(outcome, WaitOutcome::Stopping { .. }));
    assert_eq!(fake.calls(Op::Ask), 4);
    assert!(
        started.elapsed() >= Duration::from_millis(25 + 50 + 100),
        "the pauses did not grow: {:?}",
        started.elapsed()
    );
    let _ = fake.take_warnings();
    let _ = fake.take_infos();
}

/// A wait that fails while the daemon is asked again is not an exit: the asking ends, and since nothing is known
/// of the daemon the teardown is the one for a failed wait (the direct-child fallback is there), with nothing
/// waited for and no line that says it exited. The wait's failure is the one line the log has of it.
#[skuld::test]
fn a_wait_that_fails_while_the_daemon_is_asked_again_ends_the_asking_and_is_not_an_exit() {
    let fake = FakeGuard::new();
    let daemon = FakeProcess::new()
        .wait(WaitReply::Blocks)
        .ask(AskReply::UnsupportedAndWaitFails)
        .asking_always(AskReply::Unsupported)
        .send_kill_tree(KillReply::Refuses)
        .kill(KillReply::Ends);

    let (outcome, _) = stopped_by(&fake, &daemon, ask_and_wait(Some(HOUR)));

    assert!(
        matches!(outcome, WaitOutcome::TreeMayRun),
        "the teardown of a daemon that is not known to have exited falls back to the direct kill"
    );
    let calls = daemon.calls();
    assert_eq!(calls.first(), Some(&ProcessCall::Ask));
    assert_eq!(
        &calls[calls.len() - 2..],
        [ProcessCall::SendKillTree, ProcessCall::Kill]
    );
    assert!(
        calls[..calls.len() - 2].iter().all(|call| *call == ProcessCall::Ask),
        "the tree of a daemon whose wait failed was waited for: {calls:?}"
    );
    let failures = fake.take_failures();
    assert!(
        failures[0].ends_with(&format!("background wait on child: {}", Chain(&wait_failed()))),
        "{failures:?}"
    );
    assert!(
        failures.iter().all(|f| !f.contains("after the daemon ended")),
        "the failed wait was read as an exit, and the tree it left was ended as one: {failures:?}"
    );
    let _ = fake.take_warnings();
    assert_eq!(
        told_asking_within_the_hour(fake.take_infos(), "Ctrl+C"),
        Vec::<String>::new()
    );
}

/// A `timeout` already over ends the asking at its first check: no clock is read but the one the spec's
/// `timeout` is counted on.
#[skuld::test]
fn an_ask_that_is_unsupported_with_the_timeout_spent_is_not_asked_again() {
    let fake = FakeGuard::new();
    let daemon = asked(AskReply::Unsupported);

    let (outcome, _) = stopped_by(&fake, &daemon, ask_and_wait(Some(Duration::ZERO)));

    assert!(matches!(outcome, WaitOutcome::Stopping { .. }));
    assert_eq!(daemon.calls(), [ProcessCall::Ask, ProcessCall::SendKillTree]);
    assert_failures(
        &fake.take_failures(),
        ID,
        &failure_line(format!(
            "the daemon was never asked to exit: {}; its timeout is over, so it is ended without being asked",
            Chain(&ask_unsupported())
        )),
    );
    assert_eq!(fake.take_warnings(), Vec::<String>::new());
    assert_eq!(
        fake.take_infos(),
        [format!("{}0s to exit", asking_prefix("Ctrl+C"))],
        "a timeout that is spent leaves nothing to give"
    );
}

/// A `timeout` that runs out while the daemon is being asked again ends the asking, and the teardown follows. The
/// `timeout` is short enough to run out at any of the checks, so how many asks were made is not what is asserted:
/// the daemon is asked at least once, every ask precedes the teardown, and the tree is not waited for.
#[skuld::test]
fn a_timeout_that_runs_out_while_the_daemon_is_asked_again_is_followed_by_the_teardown() {
    let fake = FakeGuard::new();
    let daemon = FakeProcess::new()
        .wait(WaitReply::Blocks)
        .asking_always(AskReply::Unsupported)
        .send_kill_tree(KillReply::Ends);

    let (outcome, _) = stopped_by(&fake, &daemon, ask_and_wait(Some(Duration::from_millis(1))));

    assert!(matches!(outcome, WaitOutcome::Stopping { .. }));
    let calls = daemon.calls();
    assert_eq!(calls.first(), Some(&ProcessCall::Ask));
    assert_eq!(calls.last(), Some(&ProcessCall::SendKillTree));
    assert!(
        calls[..calls.len() - 1].iter().all(|call| *call == ProcessCall::Ask),
        "every ask precedes the teardown, and nothing is waited for: {calls:?}"
    );
    assert_failures(
        &fake.take_failures(),
        ID,
        &failure_line(format!(
            "the daemon was never asked to exit: {}; its timeout is over, so it is ended without being asked",
            Chain(&ask_unsupported())
        )),
    );
    let _ = fake.take_warnings();
    let (given, rest) = told_asking(fake.take_infos(), "Ctrl+C");
    assert!(given <= Duration::from_millis(1), "{given:?} of 1ms");
    assert_eq!(rest, Vec::<String>::new());
}

/// The wait that cannot be made is a failed wait of the tree: logged, and the daemon is ended now.
#[skuld::test]
fn a_failed_wait_for_the_tree_is_logged_and_the_daemon_is_ended() {
    let fake = FakeGuard::new();
    let daemon = asked(AskReply::Sent).wait_tree(TreeWaitReply::Fails);

    let (outcome, _) = stopped_by(&fake, &daemon, ask_and_wait(Some(HOUR)));

    assert!(matches!(outcome, WaitOutcome::Stopping { .. }));
    let [ProcessCall::Ask, ProcessCall::WaitTree(_), ProcessCall::SendKillTree] = daemon.calls()[..] else {
        panic!("asked, waited, then ended: {:?}", daemon.calls());
    };
    assert_failures(
        &fake.take_failures(),
        ID,
        &failure_line(format!(
            "waiting for the daemon's tree to exit failed: {}; the tree is ended now",
            Chain(&wait_tree_failed())
        )),
    );
    assert_eq!(
        told_asking_within_the_hour(fake.take_infos(), "Ctrl+C"),
        Vec::<String>::new()
    );
}

// The plans -----------------------------------------------------------------------------------------------------------

#[skuld::test]
fn a_service_asks_for_every_signal_but_kill_and_wm_close() {
    use crate::spec::StopSignal::{CtrlBreak, CtrlC, Interrupt, Kill, Quit, Terminate, WmClose};
    let timeout = Some(Duration::from_secs(5));
    use cosca::CtrlEvent;
    let events = [
        (Terminate, CtrlEvent::CtrlC),
        (Interrupt, CtrlEvent::CtrlC),
        (Quit, CtrlEvent::CtrlBreak),
        (CtrlC, CtrlEvent::CtrlC),
        (CtrlBreak, CtrlEvent::CtrlBreak),
    ];
    for (signal, event) in events {
        assert_eq!(
            StopPlan::of(&service_stopping_by(signal, timeout)),
            ask_and_wait_with(event, timeout),
            "{signal}"
        );
    }
    assert_eq!(
        StopPlan::of(&service_stopping_by(Terminate, None)),
        ask_and_wait(None),
        "`timeout: null` is a wait with no bound"
    );
    for signal in [Kill, WmClose] {
        assert_eq!(
            StopPlan::of(&service_stopping_by(signal, None)),
            StopPlan::AtOnce,
            "{signal}"
        );
    }
}

/// Ctrl+C for the signals Windows maps to it, Ctrl+Break for the two it maps to that, and nothing to send for
/// `kill` and `wm-close`.
#[skuld::test]
fn each_signal_is_the_console_event_it_is_on_windows() {
    use crate::spec::StopSignal::{CtrlBreak, CtrlC, Interrupt, Kill, Quit, Terminate, WmClose};
    use cosca::CtrlEvent::{CtrlBreak as Break, CtrlC as C};
    let events = [
        (Terminate, Some(C)),
        (Interrupt, Some(C)),
        (CtrlC, Some(C)),
        (Quit, Some(Break)),
        (CtrlBreak, Some(Break)),
        (Kill, None),
        (WmClose, None),
    ];
    for (signal, event) in events {
        assert_eq!(console_event(signal), event, "{signal}");
    }
}

/// A daemon whose stop asks is spawned with the cooperative stop set to the event of its signal and no process group of
/// its own; one whose stop does not ask is spawned with cosca's defaults.
#[skuld::test]
fn a_daemon_is_spawned_for_a_stop_that_asks_exactly_where_its_plan_asks() {
    use crate::spec::StopSignal::{CtrlBreak, CtrlC, Interrupt, Kill, Quit, Terminate, WmClose};
    use cosca::CtrlEvent::{CtrlBreak as Break, CtrlC as C};
    let signals = [
        (Terminate, Some(C)),
        (Interrupt, Some(C)),
        (CtrlC, Some(C)),
        (Quit, Some(Break)),
        (CtrlBreak, Some(Break)),
        (Kill, None),
        (WmClose, None),
    ];
    for (signal, event) in signals {
        let plan = StopPlan::of(&service_stopping_by(signal, Some(Duration::from_secs(5))));
        let setup = plan.spawn_setup();

        assert_eq!(setup.ask_with, event, "{signal}");
        assert_eq!(setup.process_group, event.map(|_| false), "{signal}");
        assert_eq!(plan.asks(), setup.ask_with.is_some(), "{signal}");
    }
    assert_eq!(
        StopPlan::of(&crate::testing::task(ID, vec!["C:/daemon.exe".to_string()], None)).spawn_setup(),
        SpawnSetup {
            ask_with: None,
            process_group: None
        },
        "a task is spawned with cosca's defaults"
    );
}

/// Task Scheduler's stop is the console close event, which reaches the program on its own: goetia sends nothing
/// and waits only for the program to exit.
#[skuld::test]
fn a_task_waits_only_for_the_programs_own_exit() {
    assert_eq!(
        StopPlan::of(&crate::testing::task(ID, vec!["C:/daemon.exe".to_string()], None)),
        StopPlan::WaitOnly
    );
}

/// Only a stop that asks needs a host that called `init_helpers`.
#[skuld::test]
fn only_an_ask_needs_the_proxy() {
    assert!(ask_and_wait(None).asks());
    assert!(!StopPlan::AtOnce.asks());
    assert!(!StopPlan::WaitOnly.asks());
}
