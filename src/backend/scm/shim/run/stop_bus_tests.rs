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
    launched_for(fake, daemon, StopPlan::AtOnce)
}

/// [`launched`], for a stop that runs `plan`.
fn launched_for(fake: &FakeGuard, daemon: &FakeProcess, plan: StopPlan) -> (Arc<Supervisor>, Waiter, Arc<dyn Spawned>) {
    let supervisor = supervisor(fake);
    fake.will_make_thread();
    fake.will_spawn(daemon.clone());
    let waiter = supervisor.waiter(ID, plan).expect("make the waiter thread");
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
            let outcome = supervisor.wait_for_child_or_stop(waiter, &child, ID, |_| {});
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
            let outcome = supervisor.wait_for_child_or_stop(waiter, &child, ID, |_| {});
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
                supervisor.wait_for_child_or_stop(waiter, &child, ID, |_| {});
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
    let outcome = supervisor.wait_for_child_or_stop(waiter, &child, ID, |_| {});

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
    let waiter = supervisor.waiter(ID, StopPlan::AtOnce).expect("make the waiter thread");

    // Closing the only `Sender` is the whole mechanism: the thread's first act is `rx.recv()`,
    // which returns `Err` and returns.
    let Waiter { standby, thread, .. } = waiter;
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

    let outcome = supervisor.wait_for_child_or_stop(waiter, &child, ID, |_| {});

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

    let outcome = supervisor.wait_for_child_or_stop(waiter, &child, ID, |_| {});

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

    let outcome = supervisor.wait_for_child_or_stop(waiter, &child, ID, |_| {});

    assert!(
        matches!(outcome, WaitOutcome::Stopping),
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

    let outcome = supervisor.wait_for_child_or_stop(waiter, &child, ID, |_| {});

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

    let outcome = supervisor.wait_for_child_or_stop(waiter, &child, ID, |_| {});

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

    let outcome = supervisor.wait_for_child_or_stop(waiter, &child, ID, |_| {});

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

    let outcome = supervisor.wait_for_child_or_stop(waiter, &child, ID, |_| {});

    assert!(matches!(outcome, WaitOutcome::Stopping));
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

    let outcome = supervisor.wait_for_child_or_stop(waiter, &child, ID, |_| {});

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

    let outcome = supervisor.wait_for_child_or_stop(waiter, &child, ID, |_| {});

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

    let outcome = supervisor.wait_for_child_or_stop(waiter, &child, ID, |_| {});

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

/// The `timeout` the ask-and-wait tests state: far from any wait a test makes, so what is left of it is
/// read off the duration the tree wait was given.
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

const HOUR: Duration = Duration::from_secs(60 * 60);

/// One accepted stop over the `Fake`, in the order `service::launch` makes the pieces: the stop is accepted
/// before the wait, so it is seen at once. What `on_stopping` was told, and the outcome.
fn stopped_by(fake: &FakeGuard, daemon: &FakeProcess, plan: StopPlan) -> (WaitOutcome, Vec<WaitEnd>) {
    let (supervisor, waiter, child) = launched_for(fake, daemon, plan);
    assert!(supervisor.bus.request_stop());
    let mut told = Vec::new();
    let outcome = supervisor.wait_for_child_or_stop(waiter, &child, ID, |end| told.push(end));
    assert_eq!(fake.waiter_end(0), WaiterEnd::Returned);
    (outcome, told)
}

/// [`stopped_by`], where the daemon has exited, and its exit is reported, by the time the stop is seen
/// (`on_stopping` runs between the wait's end and the plan, and the daemon ends there).
fn stopped_after_exit(fake: &FakeGuard, daemon: &FakeProcess, plan: StopPlan) -> (WaitOutcome, Vec<WaitEnd>) {
    let (supervisor, waiter, child) = launched_for(fake, daemon, plan);
    assert!(supervisor.bus.request_stop());
    let mut told = Vec::new();
    let bus = Arc::clone(&supervisor.bus);
    let outcome = supervisor.wait_for_child_or_stop(waiter, &child, ID, |end| {
        told.push(end);
        daemon.end(0);
        bus.await_exit_report();
    });
    assert_eq!(fake.waiter_end(0), WaiterEnd::Returned);
    (outcome, told)
}

fn ask_and_wait(timeout: Option<Duration>) -> StopPlan {
    StopPlan::AskAndWait { timeout }
}

/// A daemon that runs until its tree is killed, and that the ask `reply`s to.
fn asked(reply: AskReply) -> FakeProcess {
    FakeProcess::new()
        .wait(WaitReply::Blocks)
        .ask(reply)
        .send_kill_tree(KillReply::Ends)
}

#[skuld::test]
fn on_stopping_is_told_a_stop_from_an_exit() {
    let fake = FakeGuard::new();
    let (_, told) = stopped_by(&fake, &FakeProcess::running(), StopPlan::AtOnce);
    assert_eq!(told, [WaitEnd::Stop]);

    let fake = FakeGuard::new();
    let daemon = FakeProcess::new().wait(WaitReply::Exits(0));
    let (supervisor, waiter, child) = launched_for(&fake, &daemon, ask_and_wait(Some(HOUR)));
    let mut told = Vec::new();
    let outcome = supervisor.wait_for_child_or_stop(waiter, &child, ID, |end| told.push(end));

    assert!(matches!(outcome, WaitOutcome::ChildExited));
    assert_eq!(told, [WaitEnd::Exit], "the program's own exit was told as a stop");
    assert_eq!(fake.waiter_end(0), WaiterEnd::Returned);
}

/// A stop and an exit at once: the stop is accepted, so it is what the callback is told, and what the
/// run reports.
#[skuld::test]
fn a_stop_that_finds_the_daemon_exited_is_still_told_as_a_stop() {
    let fake = FakeGuard::new();
    let daemon = FakeProcess::new()
        .wait(WaitReply::Blocks)
        .send_kill_tree(KillReply::Ends);

    let (outcome, told) = stopped_after_exit(&fake, &daemon, ask_and_wait(Some(HOUR)));

    assert!(matches!(outcome, WaitOutcome::Stopping));
    assert_eq!(told, [WaitEnd::Stop]);
}

/// The daemon is gone when the stop is seen: nothing is asked and nothing is waited for, and the tree
/// it left is ended as after any exit.
#[skuld::test]
fn a_daemon_that_exited_before_the_stop_was_seen_is_neither_asked_nor_waited_for() {
    let fake = FakeGuard::new();
    let daemon = FakeProcess::new()
        .wait(WaitReply::Blocks)
        .send_kill_tree(KillReply::Ends);

    let (outcome, _) = stopped_after_exit(&fake, &daemon, ask_and_wait(Some(HOUR)));

    assert!(matches!(outcome, WaitOutcome::Stopping));
    assert_eq!(daemon.calls(), [ProcessCall::SendKillTree]);
    assert_eq!(fake.take_failures(), Vec::<String>::new());
}

/// The teardown of a daemon that exited is the exited-tree one: no direct-child kill to fall back to, and
/// no claim that only the direct child was killed.
#[skuld::test]
fn a_daemon_that_exited_before_the_stop_was_seen_gets_the_exited_tree_teardown() {
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

    assert!(matches!(outcome, WaitOutcome::Stopping));
    assert_eq!(told, [WaitEnd::Stop]);
    let [ProcessCall::Ask, ProcessCall::WaitTree(left), ProcessCall::SendKillTree] = daemon.calls()[..] else {
        panic!("asked, waited, then ended: {:?}", daemon.calls());
    };
    assert!(
        left <= HOUR && left > HOUR - Duration::from_secs(60),
        "the wait was given {left:?} of {HOUR:?}, which counts from the moment the stop was seen"
    );
    assert_eq!(
        daemon.exit_code(),
        Some(0),
        "the daemon was ended, not left to exit by itself"
    );
    assert_eq!(fake.take_failures(), Vec::<String>::new());
    assert_eq!(fake.take_warnings(), Vec::<String>::new());
}

#[skuld::test]
fn a_timeout_of_none_is_waited_for_without_a_bound() {
    let fake = FakeGuard::new();
    let daemon = asked(AskReply::SentAndExits(0)).wait_tree(TreeWaitReply::Drained);

    stopped_by(&fake, &daemon, ask_and_wait(None));

    assert_eq!(
        daemon.calls(),
        [
            ProcessCall::Ask,
            ProcessCall::WaitTree(Duration::MAX),
            ProcessCall::SendKillTree
        ]
    );
}

/// A daemon that ignores the event is ended when the wait's bound passes, by the teardown that kills.
#[skuld::test]
fn a_daemon_that_ignores_the_ask_is_killed_when_the_wait_expires() {
    let fake = FakeGuard::new();
    let daemon = asked(AskReply::Sent).wait_tree(TreeWaitReply::Remain);

    let (outcome, _) = stopped_by(&fake, &daemon, ask_and_wait(Some(HOUR)));

    assert!(matches!(outcome, WaitOutcome::Stopping));
    assert!(daemon.killed(), "the daemon was left running after its timeout");
    let [ProcessCall::Ask, ProcessCall::WaitTree(_), ProcessCall::SendKillTree] = daemon.calls()[..] else {
        panic!("asked, waited, then ended: {:?}", daemon.calls());
    };
    assert_eq!(fake.take_failures(), Vec::<String>::new());
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
}

#[skuld::test]
fn at_once_asks_and_waits_for_nothing() {
    let fake = FakeGuard::new();
    let daemon = FakeProcess::running();

    let (outcome, _) = stopped_by(&fake, &daemon, StopPlan::AtOnce);

    assert!(matches!(outcome, WaitOutcome::Stopping));
    assert_eq!(daemon.calls(), [ProcessCall::SendKillTree]);
}

/// Wait-only sends nothing, and the daemon's own exit is what ends the wait: the test ends the daemon once
/// the supervisor is in it, and the exit code says nothing ended it before.
#[skuld::test]
fn wait_only_sends_nothing_and_waits_for_the_daemons_own_exit() {
    let fake = FakeGuard::new();
    let daemon = FakeProcess::new()
        .wait(WaitReply::Blocks)
        .send_kill_tree(KillReply::Ends);
    let (supervisor, waiter, child) = launched_for(&fake, &daemon, StopPlan::WaitOnly);
    assert!(supervisor.bus.request_stop());
    let (stopping_tx, stopping_rx) = mpsc::channel();
    let (tx, rx) = mpsc::channel();
    {
        let child = Arc::clone(&child);
        let supervisor = Arc::clone(&supervisor);
        std::thread::spawn(move || {
            let outcome = supervisor.wait_for_child_or_stop(waiter, &child, ID, |_| {
                let _ = stopping_tx.send(());
            });
            let _ = tx.send(outcome);
        });
    }

    stopping_rx
        .recv()
        .expect("the wait ended without telling `on_stopping`");
    daemon.end(0);
    let outcome = rx.recv().expect("the supervisor ended without an outcome");

    assert!(matches!(outcome, WaitOutcome::Stopping));
    assert_eq!(
        daemon.exit_code(),
        Some(0),
        "something ended the daemon before it exited by itself"
    );
    assert_eq!(
        daemon.calls(),
        [ProcessCall::SendKillTree],
        "only the teardown after the exit"
    );
    assert_eq!(fake.waiter_end(0), WaiterEnd::Returned);
}

/// SCM can deliver a second `STOP` before `STOP_PENDING` is reported: it changes nothing, and the ask is
/// made once.
#[skuld::test]
fn a_second_stop_does_not_ask_again() {
    let fake = FakeGuard::new();
    let daemon = asked(AskReply::SentAndExits(0)).wait_tree(TreeWaitReply::Drained);
    let (supervisor, waiter, child) = launched_for(&fake, &daemon, ask_and_wait(Some(HOUR)));
    assert!(supervisor.bus.request_stop());
    assert!(supervisor.bus.request_stop());

    let outcome = supervisor.wait_for_child_or_stop(waiter, &child, ID, |_| {});

    assert!(matches!(outcome, WaitOutcome::Stopping));
    assert_eq!(fake.calls(Op::Ask), 1);
    assert_eq!(fake.waiter_end(0), WaiterEnd::Returned);
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

    assert!(matches!(outcome, WaitOutcome::Stopping));
    assert_eq!(daemon.calls(), [ProcessCall::Ask, ProcessCall::SendKillTree]);
    assert_failures(
        &fake.take_failures(),
        ID,
        &failure_line(format!(
            "the stop could not ask the daemon to exit: {}; nothing was sent, so the daemon is ended without being asked",
            Chain(&ask_io())
        )),
    );
}

/// The proxy may have run: ending a daemon that may be shutting down is the worse mistake, so the wait
/// is made, and the answer is a warning, not a failure.
#[skuld::test]
fn an_ask_that_may_have_been_sent_is_waited_for() {
    let answers: [(AskReply, cosca::error::Error); 2] = [
        (AskReply::MayHaveStarted, ask_may_have_started()),
        (AskReply::Unassessable, ask_unassessable()),
    ];
    for (reply, error) in answers {
        let fake = FakeGuard::new();
        let daemon = asked(reply).wait_tree(TreeWaitReply::Remain);

        let (outcome, _) = stopped_by(&fake, &daemon, ask_and_wait(Some(HOUR)));

        assert!(matches!(outcome, WaitOutcome::Stopping), "{reply:?}");
        let [ProcessCall::Ask, ProcessCall::WaitTree(_), ProcessCall::SendKillTree] = daemon.calls()[..] else {
            panic!("{reply:?}: asked, waited, then ended: {:?}", daemon.calls());
        };
        assert_eq!(fake.take_failures(), Vec::<String>::new(), "{reply:?}");
        assert_eq!(
            fake.take_warnings(),
            [format!(
                "goetia-shim[{ID}]: the stop may have asked the daemon to exit: {}; waiting for it as if it had been asked",
                Chain(&error)
            )],
            "{reply:?}"
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

    assert!(matches!(outcome, WaitOutcome::Stopping));
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
}

fn unsupported_warning() -> String {
    format!(
        "goetia-shim[{ID}]: the daemon cannot be sent the exit event yet (it has no console, or never will): {}; asking again until it answers otherwise, the daemon exits or the timeout is over",
        Chain(&ask_unsupported())
    )
}

/// The daemon exits while it is asked again: the asking stops, nothing is waited for, and the tree it left is
/// ended as after any exit. The daemon may or may not have been asked again before its exit is
/// reported, so the answers after the first are not counted.
#[skuld::test]
fn an_ask_that_is_unsupported_stops_when_the_daemon_exits() {
    let fake = FakeGuard::new();
    let daemon = FakeProcess::new()
        .wait(WaitReply::Blocks)
        .ask(AskReply::UnsupportedAndExits(3))
        .asking_always(AskReply::Unsupported)
        .send_kill_tree(KillReply::Ends);

    let (outcome, _) = stopped_by(&fake, &daemon, ask_and_wait(Some(HOUR)));

    assert!(matches!(outcome, WaitOutcome::Stopping));
    let calls = daemon.calls();
    assert_eq!(calls.first(), Some(&ProcessCall::Ask));
    assert_eq!(calls.last(), Some(&ProcessCall::SendKillTree));
    assert!(
        calls[1..calls.len() - 1].iter().all(|call| *call == ProcessCall::Ask),
        "the daemon had exited, yet it was waited for: {calls:?}"
    );
    assert_eq!(fake.take_failures(), Vec::<String>::new());
    assert_eq!(fake.take_warnings(), [unsupported_warning()]);
}

/// cosca answers `Unassessable` for a root that has exited too, with nothing sent: the daemon is asked
/// about itself, and one that has exited is not waited for. The tree it left is ended as after any exit.
#[skuld::test]
fn an_ask_answered_unassessable_by_an_exited_daemon_is_not_waited_for() {
    let fake = FakeGuard::new();
    let daemon = FakeProcess::new()
        .wait(WaitReply::Blocks)
        .ask(AskReply::UnassessableAndExited(3))
        .send_kill_tree(KillReply::Refuses);

    let (outcome, _) = stopped_by(&fake, &daemon, ask_and_wait(Some(HOUR)));

    assert!(
        matches!(outcome, WaitOutcome::TreeMayRun),
        "the exited-tree teardown is the one that cannot fall back to a direct kill"
    );
    assert_eq!(daemon.calls(), [ProcessCall::Ask, ProcessCall::SendKillTree]);
    assert_eq!(daemon.exit_code(), Some(3));
    assert_eq!(fake.take_warnings(), Vec::<String>::new());
    assert_eq!(fake.take_failures().len(), 1, "only the tree that could not be ended");
}

/// A read of the exit that fails says nothing: the daemon is waited for as before, and the read's failure is
/// told beside the warning of the ask.
#[skuld::test]
fn an_exit_that_cannot_be_read_leaves_an_unassessable_ask_waited_for() {
    let fake = FakeGuard::new();
    let daemon = asked(AskReply::Unassessable)
        .try_wait_fails()
        .wait_tree(TreeWaitReply::Remain);

    let (outcome, _) = stopped_by(&fake, &daemon, ask_and_wait(Some(HOUR)));

    assert!(matches!(outcome, WaitOutcome::Stopping));
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
            format!(
                "goetia-shim[{ID}]: the stop may have asked the daemon to exit: {}; waiting for it as if it had been asked",
                Chain(&ask_unassessable())
            ),
        ]
    );
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
        .ask(AskReply::SentAndExits(0))
        .wait_tree(TreeWaitReply::Drained);
    let started = Instant::now();

    let (outcome, _) = stopped_by(&fake, &daemon, ask_and_wait(Some(HOUR)));

    assert!(matches!(outcome, WaitOutcome::Stopping));
    assert_eq!(fake.calls(Op::Ask), 4);
    assert!(
        started.elapsed() >= Duration::from_millis(25 + 50 + 100),
        "the pauses did not grow: {:?}",
        started.elapsed()
    );
    let _ = fake.take_warnings();
}

/// A `timeout` already over ends the asking at its first check: no clock is read but the one the spec's
/// `timeout` is counted on.
#[skuld::test]
fn an_ask_that_is_unsupported_with_the_timeout_spent_is_not_asked_again() {
    let fake = FakeGuard::new();
    let daemon = asked(AskReply::Unsupported);

    let (outcome, _) = stopped_by(&fake, &daemon, ask_and_wait(Some(Duration::ZERO)));

    assert!(matches!(outcome, WaitOutcome::Stopping));
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
}

/// The wait that cannot be made is a failed wait of the tree: logged, and the daemon is ended now.
#[skuld::test]
fn a_failed_wait_for_the_tree_is_logged_and_the_daemon_is_ended() {
    let fake = FakeGuard::new();
    let daemon = asked(AskReply::Sent).wait_tree(TreeWaitReply::Fails);

    let (outcome, _) = stopped_by(&fake, &daemon, ask_and_wait(Some(HOUR)));

    assert!(matches!(outcome, WaitOutcome::Stopping));
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
}

// The plans -----------------------------------------------------------------------------------------------------------

#[skuld::test]
fn a_service_asks_for_every_signal_but_kill_and_wm_close() {
    use crate::spec::StopSignal::{CtrlBreak, CtrlC, Interrupt, Kill, Quit, Terminate, WmClose};
    let timeout = Some(Duration::from_secs(5));
    for signal in [Terminate, Interrupt, Quit, CtrlC, CtrlBreak] {
        assert_eq!(
            StopPlan::of(&service_stopping_by(signal, timeout)),
            ask_and_wait(timeout),
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

/// Task mode keeps its stop as it is until part 2 gives it the wait-only plan.
#[skuld::test]
fn a_task_is_stopped_at_once() {
    assert_eq!(
        StopPlan::of(&crate::testing::task(ID, vec!["C:/daemon.exe".to_string()], None)),
        StopPlan::AtOnce
    );
}

#[skuld::test]
fn a_stop_hint_covers_the_timeout_and_the_teardown() {
    let plan = ask_and_wait(Some(Duration::from_secs(30)));

    assert_eq!(plan.wait_hint(WaitEnd::Stop), Duration::from_secs(40));
    assert_eq!(
        ask_and_wait(None).wait_hint(WaitEnd::Stop),
        Duration::MAX,
        "no timeout: as long as the field holds"
    );
    // The program's own exit is torn down, not asked, whatever the plan.
    assert_eq!(plan.wait_hint(WaitEnd::Exit), Duration::from_secs(10));
    assert_eq!(StopPlan::AtOnce.wait_hint(WaitEnd::Stop), Duration::from_secs(10));
    assert_eq!(StopPlan::WaitOnly.wait_hint(WaitEnd::Stop), Duration::from_secs(10));
}

/// Only a stop that asks needs a host that called `init_helpers`.
#[skuld::test]
fn only_an_ask_needs_the_proxy() {
    assert!(ask_and_wait(None).asks());
    assert!(!StopPlan::AtOnce.asks());
    assert!(!StopPlan::WaitOnly.asks());
}
