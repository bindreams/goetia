//! A task's `stop` over a scripted `launchctl` and real child processes: each child stands for a run (the pid
//! `print` shows), and the scripted `kill` ends it, so the watch that `stop` waits on is a real kqueue's. Nothing
//! here touches the system's own directories or services.

use std::io;
use std::os::unix::process::ExitStatusExt as _;
use std::time::{Duration, Instant};

use super::super::manager_tests::{launchd_over, procinfo_not_naming};
use super::super::task::task_tests::{ID, Tree, task_spec};
use super::super::verbs::verbs_tests::*;
use super::super::watch::{test_hook, watch_tests::Held};
use super::*;
use crate::backend::Backend as _;
use crate::backend::bounded;
use crate::backend::client::fake::{Fake, exited, reply};
use crate::backend::standby;
use crate::manager::Step;
use crate::spec::{DaemonSpec, StopAction, StopSignal, TaskFields, TaskSpec};
use cosca::identity::Resolved;

// Fixtures ============================================================================================================

/// What ended `run`, the process a stop signalled through its hold: the signal, or none for an exit of its
/// own.
fn ended_by(run: &mut Held) -> Option<i32> {
    run.reap().signal()
}

/// A stop signals through the hold on the process, never `launchctl kill` by the label.
fn assert_no_label_signal(fake: &Fake) {
    assert!(
        calls_of(fake, "kill").is_empty(),
        "a signal goes through the process held, not by the label: {:?}",
        fake.calls()
    );
}

/// A task that stops as `action` says.
fn task_spec_stopping(action: StopAction) -> DaemonSpec {
    let DaemonSpec::Task(task) = task_spec(Some(SCHEDULE)) else {
        unreachable!("a task spec")
    };
    TaskSpec::new(TaskFields {
        stop_action: action,
        ..task.into_fields()
    })
    .map(DaemonSpec::from)
    .expect("a task with a stop action")
}

/// `stop` of a task over `fake`, drawing on a reservation made for [`Step::Stop`].
fn stopping(fake: &Fake, budget: Budget) -> Result<()> {
    let tree = Tree::new();
    let mgr = launchd_over(fake.clone());
    let mut held = reserved(&mgr, Step::Stop);
    mgr.stop(task_snapshot(&tree, false), budget, &mut held)
}

/// `stop` of a task that stops as `action` says, over `fake`.
fn stopping_as(fake: &Fake, budget: Budget, action: StopAction) -> Result<()> {
    let tree = Tree::new();
    let mgr = launchd_over(fake.clone());
    let mut held = reserved(&mgr, Step::Stop);
    mgr.stop(
        task_snapshot_of(&tree, false, &task_spec_stopping(action)),
        budget,
        &mut held,
    )
}

/// A stop action that sends SIGTERM and gives a run [`StopAction::MIN_TIMEOUT`] to answer it.
fn graced() -> StopAction {
    StopAction {
        send_signal: StopSignal::Terminate,
        timeout: Some(StopAction::MIN_TIMEOUT),
    }
}

/// SIGKILL goes out only once the `timeout` that began at SIGTERM is over, so a stop that ended a run that
/// ignores SIGTERM by SIGKILL took at least [`StopAction::MIN_TIMEOUT`], the `timeout` these tests give.
fn assert_grace_passed(stop_took: Duration) {
    assert!(
        stop_took >= StopAction::MIN_TIMEOUT,
        "SIGKILL was sent {stop_took:?} after the stop began, before the timeout was over"
    );
}

fn assert_wait_timeout(e: &Error) {
    assert!(
        matches!(e, Error::WaitTimeout { awaited: "stopped", .. }),
        "expected a wait timeout, got {e:?}"
    );
}

// stop ================================================================================================================

/// The pid `print` showed is watched, held, confirmed with launchd, signalled with SIGTERM through the hold,
/// and waited for: the stop returns once the run is gone. The first pass draws exactly the calls a stop
/// reserved, and no request: the signal is not one.
#[skuld::test]
fn stop_signals_sigterm_through_the_hold_and_waits_for_the_pid_it_saw() {
    let mut run = Held::new(0);
    let pid = run.pid();
    let fake = Fake::new([print(&running(pid)), print(&running(pid)), procinfo(pid), print(IDLE)]);
    let tree = Tree::new();
    let mgr = launchd_over(fake.clone());
    let mut held = reserved(&mgr, Step::Stop);

    mgr.stop(task_snapshot(&tree, false), Budget::Unbounded, &mut held)
        .expect("a stop");

    fake.assert_spent("read, confirm, and the read that finds no process");
    assert_no_label_signal(&fake);
    assert_eq!(ended_by(&mut run), Some(libc::SIGTERM), "the run was sent SIGTERM");
    assert_never_booted_out(&fake);
    assert_eq!(
        bounded::test_hook::left(&held),
        (super::super::STOP_REQUESTS, 0),
        "the first pass spends the stop's calls exactly, and makes no request"
    );
}

#[skuld::test]
fn stop_of_an_idle_task_is_ok() {
    for read in [print(IDLE), print("\tstate = exited\n"), not_loaded()] {
        let fake = Fake::new([read]);

        stopping(&fake, Budget::DEFAULT).expect("a stop of an idle task");

        fake.assert_spent("one read");
        assert_never_booted_out(&fake);
    }
}

/// No process and a state that is not idle, or a read that is not whole, do not establish that the job
/// stopped: `StopUnconfirmed`, saying what launchd showed. `xpcproxy` without a pid has never been seen
/// and is not taken for a start.
#[skuld::test]
fn stop_with_no_process_and_a_state_that_is_not_idle_is_unconfirmed() {
    for (what, read, shown) in unconfirmed_reads() {
        let fake = Fake::new([read]);

        let e = stopping(&fake, Budget::DEFAULT).expect_err(what);

        assert!(
            matches!(&e, Error::StopUnconfirmed { id, .. } if id == ID),
            "{what}: {e:?}"
        );
        assert_eq!(e.to_string(), format!("`{ID}` may not have stopped: {shown}"), "{what}");
        assert!(e.is_indeterminate(), "{what}");
        assert!(calls_of(&fake, "kill").is_empty(), "{what}");
    }
}

/// A SIGTERM an earlier pass sent stands when a later pass cannot tell that no process is left.
#[skuld::test]
fn stop_unconfirmed_after_a_sigterm_leaves_the_sigterm() {
    let mut run = Held::new(0);
    let pid = run.pid();
    let fake = Fake::new([
        print(&running(pid)),
        print(&running(pid)),
        procinfo(pid),
        print("\tstate = someday-new-apple-state\n"),
    ]);

    let e = stopping(&fake, Budget::Unbounded).expect_err("the next pass cannot tell");

    assert!(matches!(e, Error::StopUnconfirmed { .. }), "{e:?}");
    fake.assert_spent("the signal went out, and the next read was not conclusive");
    assert_eq!(ended_by(&mut run), Some(libc::SIGTERM), "the SIGTERM stands");
}

/// `kickstart` returns before the fork, so a `stop` straight after a `start` finds a start launchd has
/// accepted and no pid. A signal then stops nothing (launchd answers "No process to signal" and the
/// start goes on), so the stop waits for the pid, and then ends the run.
#[skuld::test]
fn stop_of_a_task_whose_start_is_still_queued_waits_for_the_pid_and_ends_the_run() {
    let mut run = Held::new(0);
    let pid = run.pid();
    let fake = Fake::new([
        print(QUEUED),
        print(QUEUED),
        print(QUEUED),
        print(&running(pid)),
        print(&running(pid)),
        print(&running(pid)),
        procinfo(pid),
        print(IDLE),
    ]);

    stopping(&fake, Budget::Unbounded).expect("a stop of a queued start");

    fake.assert_spent("reads until the pid showed, then the signal and the read that finds none");
    assert_no_label_signal(&fake);
    assert_eq!(ended_by(&mut run), Some(libc::SIGTERM), "SIGTERM is sent");
    assert_never_booted_out(&fake);
}

/// A start that is accepted and then does not happen leaves `spawn scheduled` with no pid: the wait ends
/// on the state, and there is nothing to end.
#[skuld::test]
fn stop_of_a_queued_start_that_never_forks_is_ok_when_the_state_leaves_spawn_scheduled() {
    let fake = Fake::new([print(QUEUED), print(QUEUED), print(IDLE), print(IDLE)]);

    stopping(&fake, Budget::Unbounded).expect("a stop");

    fake.assert_spent("the wait ended on the state, and the next read found nothing");
    assert!(calls_of(&fake, "kill").is_empty());
}

/// A run that began while `stop` waited for the one before is a real run, and is ended too; the loop
/// ends only when `print` shows no process.
#[skuld::test]
fn stop_ends_a_run_that_begins_while_it_is_stopping() {
    let (mut first, mut second) = (Held::new(0), Held::new(0));
    let (a, b) = (first.pid(), second.pid());
    let fake = Fake::new([
        print(&running(a)),
        print(&running(a)),
        procinfo(a),
        print(&running(b)),
        print(&running(b)),
        procinfo(b),
        print(IDLE),
    ]);

    stopping(&fake, Budget::Unbounded).expect("a stop");

    fake.assert_spent("both runs were ended");
    assert_no_label_signal(&fake);
    assert_eq!(ended_by(&mut first), Some(libc::SIGTERM));
    assert_eq!(ended_by(&mut second), Some(libc::SIGTERM));
}

/// The pid that was read may be reused by the time it is watched: only a job that `print` still shows
/// with that pid is signalled.
#[skuld::test]
fn stop_with_a_pid_that_is_not_the_jobs_any_more_is_ok_and_sends_nothing() {
    let mut other = Held::new(0);
    let fake = Fake::new([print(&running(other.pid())), print(IDLE), print(IDLE)]);

    stopping(&fake, Budget::Unbounded).expect("a stop");

    fake.assert_spent("the confirming read showed another job, and the next pass found none");
    assert_no_label_signal(&fake);
    assert_eq!(
        other.finish(),
        Some(0),
        "nothing is sent to a process that is not the job's"
    );
}

/// `stop` waits for the run to be over and reads no status, so an exit the kernel reported without one
/// ends the wait like any other: the stop goes on to read again, and succeeds.
#[skuld::test]
fn stop_takes_an_exit_the_kernel_reported_without_a_status() {
    let _unreported = test_hook::statuses(0);
    let run = Held::new(0);
    let pid = run.pid();
    let fake = Fake::new([print(&running(pid)), print(&running(pid)), procinfo(pid), print(IDLE)]);

    stopping(&fake, Budget::Unbounded).expect("a stop");

    fake.assert_spent("the run ended without a status, and the next read found none");
}

/// A pid that has exited by the time it is watched cannot be watched: the next pass reads again.
#[skuld::test]
fn stop_with_a_pid_that_has_exited_reads_again() {
    let mut ended = Held::new(0);
    let pid = ended.pid();
    ended.finish();
    let fake = Fake::new([print(&running(pid)), print(IDLE)]);

    stopping(&fake, Budget::Unbounded).expect("a stop");

    fake.assert_spent("a gone pid is not confirmed, and the next read found none");
    assert!(calls_of(&fake, "kill").is_empty());
}

/// A run that ignores SIGTERM keeps the stop waiting until the budget ends: the budget the user gave is
/// the failure bound, and the signal stands.
#[skuld::test]
fn stop_under_a_bounded_budget_expires_into_wait_timeout_and_leaves_the_signal() {
    let mut run = Held::ignoring_sigterm();
    let pid = run.pid();
    // The last reading is the one a stop makes once its deadline is spent: the run is still there.
    let fake = Fake::new([
        print(&running(pid)),
        print(&running(pid)),
        procinfo(pid),
        print(&running(pid)),
        print(&running(pid)),
        procinfo(pid),
    ]);

    let e = stopping(&fake, Budget::Bounded(Duration::from_millis(100))).expect_err("a run that ignores SIGTERM");

    assert_wait_timeout(&e);
    fake.assert_spent("the signal was sent, and nothing after it but the read at the deadline");
    assert_eq!(
        run.finish(),
        Some(0),
        "the run was sent SIGTERM, which it ignored, and no more"
    );
}

/// The wait that ends while a run still has its grace to answer SIGTERM says what that costs: nothing
/// sends SIGKILL after it, and `goetia daemon kill` does.
#[skuld::test]
fn a_wait_that_ends_inside_the_grace_names_the_limit() {
    // The run ignores SIGTERM, so the wait ends at the budget.
    let run = Held::ignoring_sigterm();
    let pid = run.pid();
    let fake = Fake::new([
        print(&running(pid)),
        print(&running(pid)),
        procinfo(pid),
        // The reading once the deadline is spent.
        print(&running(pid)),
        print(&running(pid)),
        procinfo(pid),
    ]);

    let recovery = match stopping_as(
        &fake,
        Budget::Bounded(Duration::from_millis(100)),
        StopAction::default(),
    ) {
        Err(Error::WaitTimeout { recovery, .. }) => recovery,
        other => panic!("expected a wait timeout, got {other:?}"),
    };

    assert!(
        recovery.contains("was sent SIGTERM and has the `stop-action` timeout to answer it")
            && recovery.contains("sends SIGKILL after that only while it waits")
            && recovery.contains(&format!("`goetia daemon kill {ID}` ends it")),
        "{recovery}"
    );
    assert!(
        !recovery.contains("did not answer it within") && !recovery.contains("was sent SIGKILL"),
        "the text of the other cases: {recovery}"
    );
}

/// A run that ignores SIGTERM for as long as the `timeout` is escalated: SIGKILL is sent when it is over,
/// and the stop returns once the run is gone. The `timeout` is the grace the user configured, as it is
/// for systemd's `TimeoutStopSec=`. Both signals go to the process held, the SIGKILL by the pass that
/// holds the run when the grace ends, once launchd has confirmed the process again, and neither to the label.
#[skuld::test]
fn stop_sends_sigkill_to_a_run_that_ignores_sigterm_for_the_timeout() {
    let mut run = Held::ignoring_sigterm();
    let pid = run.pid();
    let fake = Fake::new([
        print(&running(pid)),
        print(&running(pid)),
        procinfo(pid),
        // The timeout is over: the pass confirms the process again, and kills it. No read of its own.
        print(&running(pid)),
        procinfo(pid),
        print(IDLE),
    ]);

    let began = Instant::now();
    stopping_as(&fake, Budget::Unbounded, graced()).expect("a stop that ends the run");

    assert_grace_passed(began.elapsed());
    fake.assert_spent("SIGTERM, the timeout, then the confirmation and the kill of the process held");
    assert_no_label_signal(&fake);
    assert_eq!(ended_by(&mut run), Some(libc::SIGKILL), "the run was ended by SIGKILL");
}

/// The process held is killed only if launchd still names it as the job's when the timeout is over: a
/// pid that launchd shows and does not name is a stranger's, or a run that is gone, and is left alone.
#[skuld::test]
fn a_process_launchd_no_longer_names_when_the_timeout_is_over_is_not_sent_sigkill() {
    let mut run = Held::ignoring_sigterm();
    let pid = run.pid();
    let fake = Fake::new([
        print(&running(pid)),
        print(&running(pid)),
        procinfo(pid),
        print(&running(pid)),
        procinfo_not_naming(pid),
        print(IDLE),
    ]);

    stopping_as(&fake, Budget::Unbounded, graced()).expect("a stop that finds no run to end");

    fake.assert_spent("SIGTERM, the timeout, the confirmation that refused, and the read that found none");
    assert_eq!(
        run.finish(),
        Some(0),
        "SIGTERM was ignored, and nothing is sent to a process launchd does not name"
    );
}

/// The stop is always issued, whatever the budget: the first pass sends the run it finds its first signal,
/// and a bound that is spent (a zero bound, or the least bound a `restart` hands its stop leg once its budget
/// ran out) then ends the stop at its next pass, which reads once (`print`, and the confirmation of the pid it
/// shows) and finds the run still there. The run ignores SIGTERM, so it is there to be found, whatever the clock.
#[skuld::test]
fn a_stop_with_a_spent_budget_sends_its_first_signal_and_then_reports_the_timeout() {
    for budget in [Duration::ZERO, Duration::from_nanos(1)] {
        let mut run = Held::ignoring_sigterm();
        let pid = run.pid();
        let fake = Fake::new([
            print(&running(pid)),
            print(&running(pid)),
            procinfo(pid),
            // The read the next pass makes once, with its deadline spent.
            print(&running(pid)),
            print(&running(pid)),
            procinfo(pid),
        ]);

        let e = stopping_as(&fake, Budget::Bounded(budget), graced()).expect_err("a deadline that is spent");

        let Error::WaitTimeout { recovery, .. } = &e else {
            panic!("{budget:?}: expected a wait timeout, got {e:?}")
        };
        assert!(recovery.contains("was sent SIGTERM"), "{budget:?}: {recovery}");
        fake.assert_spent("the first signal, and the one reading after it");
        assert_eq!(run.finish(), Some(0), "{budget:?}: SIGTERM, ignored, and nothing more");
    }
}

/// A zero bound still waits for a queued start's pid, as every budget does, and signals the run it begins.
#[skuld::test]
fn a_stop_with_no_time_waits_for_a_queued_start_signals_it_and_reports_the_timeout() {
    let mut run = Held::ignoring_sigterm();
    let pid = run.pid();
    let fake = Fake::new([
        print(QUEUED),
        print(QUEUED),
        print(&running(pid)),
        print(&running(pid)),
        print(&running(pid)),
        procinfo(pid),
        print(&running(pid)),
        print(&running(pid)),
        procinfo(pid),
    ]);

    let e = stopping_as(&fake, Budget::Bounded(Duration::ZERO), graced()).expect_err("a deadline that is spent");

    let Error::WaitTimeout { recovery, .. } = &e else {
        panic!("expected a wait timeout, got {e:?}")
    };
    assert!(recovery.contains("was sent SIGTERM"), "{recovery}");
    fake.assert_spent("the queued start, the signal, and the one reading after it");
    assert_eq!(run.finish(), Some(0), "SIGTERM, ignored, and nothing more");
}

/// Once the first signal is sent, a spent deadline sends nothing more, and the pass that finds it spent reads
/// `print` once. A start launchd accepted is the timeout; no process and a state that is not idle (or a read
/// that was not whole) is the stop's `StopUnconfirmed`, as in any pass.
#[skuld::test]
fn a_stop_whose_deadline_is_spent_after_its_first_signal_reads_once_and_ends_as_the_read_says() {
    let failed = reply(
        &["print", &target()],
        exited(5, "", "Could not print: 5: Input/output error\n"),
    );
    let reads = [
        ("a queued start", print(QUEUED), true),
        (
            "a state that says nothing",
            print("\tstate = someday-new-apple-state\n"),
            false,
        ),
        ("a read that failed", failed, false),
    ];
    for (what, read, times_out) in reads {
        let mut run = Held::ignoring_sigterm();
        let pid = run.pid();
        let fake = Fake::new([print(&running(pid)), print(&running(pid)), procinfo(pid), read]);

        let e = stopping_as(&fake, Budget::Bounded(Duration::ZERO), graced()).expect_err(what);

        if times_out {
            assert_wait_timeout(&e);
        } else {
            assert!(matches!(&e, Error::StopUnconfirmed { .. }), "{what}: {e:?}");
        }
        fake.assert_spent(what);
        assert_eq!(run.finish(), Some(0), "{what}: SIGTERM, ignored, and nothing more");
    }
}

/// A run that ended inside the budget is not a timeout, whenever the next pass starts: the next pass is after
/// the deadline, and its one read shows the job idle. Nothing is sent, and nothing is read twice.
#[skuld::test]
fn a_run_that_ended_inside_the_budget_is_not_a_timeout_though_the_next_pass_starts_after_it() {
    for read in [print(IDLE), print("\tstate = exited\n"), not_loaded()] {
        let mut run = Held::new(0);
        let pid = run.pid();
        let fake = Fake::new([print(&running(pid)), print(&running(pid)), procinfo(pid), read]);

        stopping(&fake, Budget::Bounded(Duration::ZERO)).expect("the job is done");

        fake.assert_spent("the signal, and the one read, which shows no process");
        assert_eq!(ended_by(&mut run), Some(libc::SIGTERM), "SIGTERM went out");
    }
}

/// What the timeout says is what this stop did to the run that is up: SIGTERM for the run it sent SIGTERM;
/// for a run it never signalled, that it was not signalled, whatever it sent the run before (SIGKILL
/// included) and whether the run is shown or a start is queued.
#[skuld::test]
fn a_timeout_says_what_the_stop_did_to_the_run_that_is_up() {
    let kill = StopAction {
        send_signal: StopSignal::Kill,
        timeout: None,
    };
    let spent = Budget::Bounded(Duration::ZERO);
    let timeout_of = |fake: &Fake, action| match stopping_as(fake, spent, action) {
        Err(Error::WaitTimeout { recovery, .. }) => recovery,
        other => panic!("expected a wait timeout, got {other:?}"),
    };

    // A run sent SIGTERM, and still there.
    let mut run = Held::ignoring_sigterm();
    let pid = run.pid();
    let fake = Fake::new([
        print(&running(pid)),
        print(&running(pid)),
        procinfo(pid),
        print(&running(pid)),
        print(&running(pid)),
        procinfo(pid),
    ]);
    let recovery = timeout_of(&fake, graced());
    assert!(recovery.contains("was sent SIGTERM"), "{recovery}");
    run.finish();

    // A run sent SIGKILL, gone; another run is up.
    let (mut killed, mut next) = (Held::ignoring_sigterm(), Held::new(0));
    let (a, b) = (killed.pid(), next.pid());
    let fake = Fake::new([
        print(&running(a)),
        print(&running(a)),
        procinfo(a),
        print(&running(b)),
        print(&running(b)),
        procinfo(b),
    ]);
    let recovery = timeout_of(&fake, kill);
    assert!(recovery.contains("was not signalled"), "{recovery}");
    assert!(!recovery.contains("was sent SIGKILL"), "{recovery}");
    assert_eq!(ended_by(&mut killed), Some(libc::SIGKILL));
    assert_eq!(next.finish(), Some(0), "the run that was up was sent nothing");

    // A run sent SIGTERM, gone; a start is queued.
    let mut run = Held::new(0);
    let pid = run.pid();
    let fake = Fake::new([print(&running(pid)), print(&running(pid)), procinfo(pid), print(QUEUED)]);
    let recovery = timeout_of(&fake, graced());
    assert!(recovery.contains("was not signalled"), "{recovery}");
    assert_eq!(ended_by(&mut run), Some(libc::SIGTERM));
}

/// The deadline is decided once more right before every send after the first: the send of a spent one is
/// refused, and the run is sent nothing. The course is spent by construction, a zero bound with a run already
/// signalled, so no clock decides.
#[skuld::test]
fn a_send_after_the_first_is_refused_once_the_deadline_is_spent() {
    let first = Held::new(0);
    let mut second = Held::new(0);
    let identity = |run: &Held| match ProcessId::of(run.pid()) {
        Resolved::Found(identity) => identity,
        _ => panic!("the child is running"),
    };
    let signalled = identity(&first);
    let held = crate::backend::kill::process::open(second.pid())
        .expect("open the child")
        .expect("the child is running");
    let mut course = Course::new(None, Budget::Bounded(Duration::ZERO), Watch::new().expect("a watch"));
    course.sent(signalled, Signal::Terminate);
    assert!(course.spent(), "a zero bound is spent once something was sent");

    let sent = send(&held, &mut course, ID, Signal::Terminate).expect("a refusal is not a failure");

    assert!(!sent, "a spent deadline sends nothing");
    assert_eq!(
        course.termed(identity(&second)),
        None,
        "nothing was recorded for the run"
    );
    assert_eq!(second.finish(), Some(0), "the run was sent nothing");
}

/// What the timeout says of a pid launchd shows that is no run of the job's this stop can reach: that launchd
/// still showed a process ID, that the process had ended or been replaced, and to stop again; no word of a
/// signal or of `goetia daemon kill`, which would not help.
fn assert_uncleared(e: &Error) {
    let Error::WaitTimeout { recovery, .. } = e else {
        panic!("expected a wait timeout, got {e:?}")
    };
    assert!(
        recovery.contains("still showed a process ID") && recovery.contains("daemon stop"),
        "{recovery}"
    );
    assert!(
        !recovery.contains("not signalled") && !recovery.contains("daemon kill"),
        "nothing was left to signal, and `kill` does not help: {recovery}"
    );
}

/// The wording for a pid launchd shows and this stop cannot reach, as someone who has not read the code reads it.
#[skuld::test]
fn the_uncleared_timeout_says_what_launchd_showed_and_to_stop_again() {
    let budget = Budget::Bounded(Duration::from_secs(3));
    let Error::WaitTimeout { recovery: plain, .. } = budget::timed_out(ID, "stopped", budget) else {
        panic!("a wait timeout")
    };

    let Error::WaitTimeout { recovery, .. } = timed_out_stopping(ID, budget, Escalation::Uncleared) else {
        panic!("a wait timeout")
    };

    assert_eq!(
        recovery,
        format!(
            "{plain} Launchd still showed a process ID for the job when the deadline passed, but that process \
             had already ended or belonged to a run that replaced it; run `goetia daemon stop {ID}` again."
        )
    );
}

/// The first pass always runs, and from the second on every pass reads the deadline first, whether anything was
/// sent or not: a stop that keeps finding a pid launchd shows and does not name (a stranger given the run's
/// number) ends at its deadline, with the timeout of a pid it cannot reach. It is not read
/// again and again for ever.
#[skuld::test]
fn a_stop_that_keeps_finding_a_pid_launchd_does_not_name_ends_at_its_deadline() {
    let mut other = Held::new(0);
    let pid = other.pid();
    let fake = Fake::new([
        print(&running(pid)),
        print(&running(pid)),
        procinfo_not_naming(pid),
        // The pass after it finds the deadline spent, and reads once: the pid is still shown, and still not named.
        print(&running(pid)),
        print(&running(pid)),
        procinfo_not_naming(pid),
    ]);

    let e = stopping(&fake, Budget::Bounded(Duration::ZERO)).expect_err("a pid that is never cleared");

    assert_uncleared(&e);
    fake.assert_spent("the first pass, and the one reading the second makes");
    assert_no_label_signal(&fake);
    assert_eq!(
        other.finish(),
        Some(0),
        "a process that is not the job's is sent nothing"
    );
}

/// The same outcome for a pid the OS reports gone: a zombie, which cannot be watched and whose number cannot be
/// reused while it is unreaped, so no other process can take its place in the script. Launchd has not cleared
/// it by the deadline: not `Ok`, since a start made now starts nothing.
#[skuld::test]
fn a_pid_shown_at_the_deadline_that_has_exited_is_a_run_that_ended_and_is_not_cleared() {
    let mut ended = Held::new(0);
    let pid = ended.pid();
    ended.exit_unreaped();
    let fake = Fake::new([print(&running(pid)), print(&running(pid))]);

    let e = stopping(&fake, Budget::Bounded(Duration::ZERO)).expect_err("launchd still shows the run");

    assert_uncleared(&e);
    fake.assert_spent("the first pass, which could not watch the pid, and the one reading at the deadline");
}

/// `Immediate` waits for no exit, but not past what launchd shows: a pass that finds launchd showing a pid that
/// has ended (gone, or one it does not name) is followed by another reading, however many readings show it, and
/// the stop returns when one shows no process.
#[skuld::test]
fn stop_under_immediate_reads_on_while_launchd_shows_a_pid_it_cannot_reach_and_returns_when_it_shows_none() {
    let mut zombie = Held::new(0);
    let gone = zombie.pid();
    zombie.exit_unreaped();
    let mut stranger = Held::new(0);
    let other = stranger.pid();
    for (what, fake) in [
        (
            "a pid that is gone",
            Fake::new([
                print(&running(gone)),
                print(&running(gone)),
                print(&running(gone)),
                print(IDLE),
            ]),
        ),
        (
            "a pid launchd does not name",
            Fake::new([
                print(&running(other)),
                print(&running(other)),
                procinfo_not_naming(other),
                print(&running(other)),
                print(&running(other)),
                procinfo_not_naming(other),
                print(&running(other)),
                print(&running(other)),
                procinfo_not_naming(other),
                print(IDLE),
            ]),
        ),
    ] {
        stopping(&fake, Budget::Immediate).expect("a stop that waits for no exit");

        fake.assert_spent(what);
        assert_no_label_signal(&fake);
    }
    assert_eq!(
        stranger.finish(),
        Some(0),
        "a process that is not the job's is sent nothing"
    );
}

/// The second reading is a pass like any other: a new live run of the job is held, and sent SIGTERM; the stop
/// returns, as it does for a run it finds first.
#[skuld::test]
fn stop_under_immediate_whose_second_reading_shows_a_new_run_sends_it_sigterm() {
    let mut zombie = Held::new(0);
    let gone = zombie.pid();
    zombie.exit_unreaped();
    let mut next = Held::new(0);
    let pid = next.pid();
    let fake = Fake::new([
        print(&running(gone)),
        print(&running(pid)),
        print(&running(pid)),
        procinfo(pid),
    ]);

    stopping(&fake, Budget::Immediate).expect("a stop that waits for nothing after the signal");

    fake.assert_spent("the pid that is gone, then the new run, and the signal to it");
    assert_no_label_signal(&fake);
    assert_eq!(ended_by(&mut next), Some(libc::SIGTERM), "the new run was sent SIGTERM");
}

/// What `print` shows is what keeps the loop going, whether it changes or not: another pid launchd shows and
/// does not reach is read again, as is the same pid, and only an idle job ends it.
#[skuld::test]
fn stop_under_immediate_reads_again_while_the_pid_launchd_shows_changes() {
    let (mut first, mut second) = (Held::new(0), Held::new(0));
    let (a, b) = (first.pid(), second.pid());
    first.exit_unreaped();
    second.exit_unreaped();
    for (what, fake) in [
        (
            "another pid, then the same, then no process",
            Fake::new([print(&running(a)), print(&running(b)), print(&running(b)), print(IDLE)]),
        ),
        (
            "another pid, then no process",
            Fake::new([print(&running(a)), print(&running(b)), print(IDLE)]),
        ),
    ] {
        stopping(&fake, Budget::Immediate).expect(what);

        fake.assert_spent(what);
    }
}

/// A start that forks between two readings of the same pid is waited for under `Immediate` too, and the pid
/// shown after it is read like any other.
#[skuld::test]
fn stop_under_immediate_reads_a_pid_again_after_a_queued_start_forked_between_the_readings() {
    let mut zombie = Held::new(0);
    let a = zombie.pid();
    zombie.exit_unreaped();
    let fake = Fake::new([
        print(&running(a)),
        print(QUEUED),
        print(QUEUED),
        print(&running(a)),
        print(&running(a)),
        print(IDLE),
    ]);

    stopping(&fake, Budget::Immediate).expect("a stop that waits for nothing");

    fake.assert_spent("the pid, the queued start and its fork, the pid again, and the reading after it");
}

/// A later pass can find the run that was sent SIGTERM again: here launchd's lookup did not name the process
/// when the grace was over, and does by the next pass. The grace is the one that began at the first SIGTERM,
/// and it is over: the run is killed through the hold at once, not sent a second SIGTERM or given another
/// grace. The signals sent say so.
#[skuld::test]
fn a_run_found_again_keeps_the_grace_that_began_at_its_sigterm() {
    let mut run = Held::ignoring_sigterm();
    let pid = run.pid();
    let fake = Fake::new([
        print(&running(pid)),
        print(&running(pid)),
        procinfo(pid),
        // The grace is over, and launchd does not name the process held.
        print(&running(pid)),
        procinfo_not_naming(pid),
        // The next pass holds the same process: the grace is over, so it is killed through the hold.
        print(&running(pid)),
        print(&running(pid)),
        procinfo(pid),
        print(IDLE),
    ]);

    let began = Instant::now();
    stopping_as(&fake, Budget::Unbounded, graced()).expect("a stop that ends the run");

    assert_grace_passed(began.elapsed());
    fake.assert_spent("SIGTERM once, the refused confirmation, then the kill of the same run");
    assert_eq!(ended_by(&mut run), Some(libc::SIGKILL), "the run was ended by SIGKILL");
}

/// What the timeout of a stop whose deadline is spent says of the run that is up: what the stop did to that run
/// and no other. The grace is a stated deadline here, so no clock decides.
#[skuld::test]
fn a_run_is_said_to_have_been_sent_what_this_stop_sent_it() {
    let (mine, other) = (Held::new(0), Held::new(0));
    let identity = |run: &Held| match ProcessId::of(run.pid()) {
        Resolved::Found(identity) => identity,
        _ => panic!("the child is running"),
    };
    let (mine, other) = (identity(&mine), identity(&other));
    let (over, pending) = (Budget::Immediate.start(), Budget::Unbounded.start());
    let course = |grace_end, killed| {
        let mut course = Course::new(Some(graced()), Budget::Unbounded, Watch::new().expect("a watch"));
        course.termed = Some(Termed {
            run: mine,
            grace_end,
            killed,
        });
        course
    };

    assert_eq!(course(Some(pending), false).escalation(mine), Some(Escalation::Pending));
    assert_eq!(course(Some(over), false).escalation(mine), Some(Escalation::Spent));
    assert_eq!(course(Some(over), true).escalation(mine), Some(Escalation::Sent));
    assert_eq!(
        course(None, false).escalation(mine),
        None,
        "SIGTERM and no grace to follow it"
    );
    for killed in [false, true] {
        assert_eq!(
            course(Some(over), killed).escalation(other),
            Some(Escalation::Unsent),
            "another run was sent nothing, whatever this one was"
        );
    }
}

/// What the timeout says of a run the stop did not signal, word for word: one line, and its command is `goetia
/// daemon kill <id>`.
#[skuld::test]
fn the_timeout_of_a_run_that_was_not_signalled_says_so_in_one_sentence_and_names_the_command() {
    let budget = Budget::Bounded(Duration::from_secs(3));
    let Error::WaitTimeout { recovery: plain, .. } = budget::timed_out(ID, "stopped", budget) else {
        panic!("a wait timeout")
    };

    let Error::WaitTimeout { recovery, .. } = timed_out_stopping(ID, budget, Escalation::Unsent) else {
        panic!("a wait timeout")
    };

    assert_eq!(
        recovery,
        format!(
            "{plain} The run was not signalled before the deadline; nothing will signal it: \
             `goetia daemon kill {ID}` ends it."
        )
    );
}

/// What a run is due, by what it has been sent and whether its grace is over: SIGTERM, or SIGKILL for
/// `send-signal: kill`, first; then SIGKILL only once its grace is over, never twice, and never to another
/// run on the strength of this one's grace. The grace is a stated deadline here, so no clock decides.
#[skuld::test]
fn a_run_is_due_sigkill_once_its_grace_is_over_and_not_before() {
    let (mine, other) = (Held::new(0), Held::new(0));
    let identity = |run: &Held| match ProcessId::of(run.pid()) {
        Resolved::Found(identity) => identity,
        _ => panic!("the child is running"),
    };
    let (mine, other) = (identity(&mine), identity(&other));
    let course = |action| Course::new(Some(action), Budget::Unbounded, Watch::new().expect("a watch"));
    let kill = StopAction {
        send_signal: StopSignal::Kill,
        timeout: None,
    };
    let (over, pending) = (Budget::Immediate.start(), Budget::Unbounded.start());
    let termed = |grace_end, killed| {
        let mut course = course(graced());
        course.termed = Some(Termed {
            run: mine,
            grace_end: Some(grace_end),
            killed,
        });
        course
    };

    assert_eq!(course(graced()).due(mine), Some(Signal::Terminate), "its first");
    assert_eq!(course(kill).due(mine), Some(Signal::Kill), "its first, for `kill`");
    assert_eq!(termed(pending, false).due(mine), None, "a grace that is not over");
    assert_eq!(
        termed(over, false).due(mine),
        Some(Signal::Kill),
        "a grace that is over"
    );
    assert_eq!(termed(over, true).due(mine), None, "killed once");
    assert_eq!(
        termed(over, false).due(other),
        Some(Signal::Terminate),
        "another run has a grace of its own"
    );
}

/// A pid that launchd shows and does not name is not signalled at all, SIGTERM included: it is read again.
#[skuld::test]
fn a_pid_launchd_shows_and_procinfo_does_not_name_is_sent_nothing() {
    let mut run = Held::new(0);
    let pid = run.pid();
    let fake = Fake::new([
        print(&running(pid)),
        print(&running(pid)),
        procinfo_not_naming(pid),
        print(IDLE),
    ]);

    stopping(&fake, Budget::Unbounded).expect("a stop");

    fake.assert_spent("the confirmation refused, and the next read found none");
    assert_no_label_signal(&fake);
    assert_eq!(
        run.finish(),
        Some(0),
        "a process that is not the job's is not signalled"
    );
}

/// A queued start's wait is not counted, and the stop is still held to its budget: it ends, and says it
/// waited for the budget and for the start together.
#[skuld::test]
fn a_stop_held_to_a_budget_ends_after_a_queued_start_and_reports_the_whole_wait() {
    let run = Held::ignoring_sigterm();
    let pid = run.pid();
    let throttled = Duration::from_millis(500);
    let budget = Duration::from_millis(100);
    let fake = Fake::new([
        print(QUEUED),
        print(QUEUED),
        print(QUEUED).then(move || std::thread::sleep(throttled)),
        print(&running(pid)),
        print(&running(pid)),
        print(&running(pid)),
        procinfo(pid),
        // The reading once the deadline is spent.
        print(&running(pid)),
        print(&running(pid)),
        procinfo(pid),
    ]);

    let e = stopping(&fake, Budget::Bounded(budget)).expect_err("a run that ignores SIGTERM");

    match e {
        Error::WaitTimeout {
            awaited: "stopped",
            waited,
            ..
        } => assert!(
            waited >= throttled + budget,
            "the wait that was made is reported: {waited:?}"
        ),
        other => panic!("expected a wait timeout, got {other:?}"),
    }
    fake.assert_spent("the queued start and the SIGTERM");
}

/// `send-signal: kill` sends SIGKILL at once, through the hold, and no SIGTERM.
#[skuld::test]
fn stop_with_send_signal_kill_sends_sigkill_at_once() {
    let mut run = Held::ignoring_sigterm();
    let pid = run.pid();
    let fake = Fake::new([print(&running(pid)), print(&running(pid)), procinfo(pid), print(IDLE)]);
    let action = StopAction {
        send_signal: StopSignal::Kill,
        timeout: None,
    };

    stopping_as(&fake, Budget::Unbounded, action).expect("a stop that ends the run");

    fake.assert_spent("read, confirm, and the read that finds no process");
    assert_no_label_signal(&fake);
    assert_eq!(ended_by(&mut run), Some(libc::SIGKILL), "the run was ended by SIGKILL");
}

/// A run that ends within the `timeout` is not sent SIGKILL.
#[skuld::test]
fn stop_of_a_run_that_answers_sigterm_in_time_sends_no_sigkill() {
    let mut run = Held::new(0);
    let pid = run.pid();
    let fake = Fake::new([print(&running(pid)), print(&running(pid)), procinfo(pid), print(IDLE)]);
    let action = StopAction {
        send_signal: StopSignal::Terminate,
        timeout: Some(StopAction::MIN_TIMEOUT),
    };

    stopping_as(&fake, Budget::Unbounded, action).expect("a stop");

    fake.assert_spent("SIGTERM ended the run");
    assert_eq!(
        ended_by(&mut run),
        Some(libc::SIGTERM),
        "the run answered SIGTERM, and was sent no more"
    );
}

/// The SIGKILL is for the run whose `timeout` ran out. A run that began after that one was told to stop
/// is a new run: it is sent SIGTERM, and has a `timeout` of its own, whole, before it is killed.
#[skuld::test]
fn the_sigkill_is_for_the_run_whose_timeout_ran_out_and_not_for_the_next() {
    let mut first = Held::ignoring_sigterm();
    let mut second = Held::ignoring_sigterm();
    let (a, b) = (first.pid(), second.pid());
    let fake = Fake::new([
        print(&running(a)),
        print(&running(a)),
        procinfo(a),
        // `print` shows another run by the time the timeout is over: the first is not the job's any more,
        // and the process held is left alone.
        print(&running(b)),
        // The second is told to stop, and ignores it.
        print(&running(b)),
        print(&running(b)),
        procinfo(b),
        // Its own timeout is over: it is killed.
        print(&running(b)),
        procinfo(b),
        print(IDLE),
    ]);
    let action = StopAction {
        send_signal: StopSignal::Terminate,
        timeout: Some(StopAction::MIN_TIMEOUT),
    };

    let began = Instant::now();
    stopping_as(&fake, Budget::Unbounded, action).expect("a stop that ends the second run");

    assert_grace_passed(began.elapsed());
    fake.assert_spent("each run was told to stop once, and the second killed");
    assert_no_label_signal(&fake);
    assert_eq!(
        first.finish(),
        Some(0),
        "the first run was sent SIGTERM, which it ignored, and no more"
    );
    assert_eq!(
        ended_by(&mut second),
        Some(libc::SIGKILL),
        "the second run was ended by SIGKILL"
    );
}

/// `Immediate` waits for no exit: the signal is sent and the stop returns.
#[skuld::test]
fn stop_under_immediate_of_a_running_job_sends_and_returns_ok() {
    let mut run = Held::new(0);
    let pid = run.pid();
    let fake = Fake::new([print(&running(pid)), print(&running(pid)), procinfo(pid)]);

    stopping(&fake, Budget::Immediate).expect("a stop that waits for nothing");

    fake.assert_spent("read, confirm");
    assert_eq!(ended_by(&mut run), Some(libc::SIGTERM), "the signal went out");
}

/// A queued start has no pid, and a signal sent then stops nothing: even under `Immediate` the stop waits
/// for the pid, and only then signals. It does not wait for the process.
#[skuld::test]
fn stop_under_immediate_of_a_queued_start_waits_for_the_pid_then_signals() {
    let mut run = Held::new(0);
    let pid = run.pid();
    let fake = Fake::new([
        print(QUEUED),
        print(QUEUED),
        print(&running(pid)),
        print(&running(pid)),
        print(&running(pid)),
        procinfo(pid),
    ]);

    stopping(&fake, Budget::Immediate).expect("a stop that waits for nothing after the signal");

    fake.assert_spent("the pid showed, and the process was signalled");
    assert_eq!(
        ended_by(&mut run),
        Some(libc::SIGTERM),
        "the signal went out once the pid showed"
    );
}

/// The first pass uses the reservation it was handed, and a later pass makes its own: the first spends
/// the stop's count exactly, so a second pass that drew on it would trip the reservation's assertion.
#[skuld::test]
fn a_later_stop_pass_makes_its_own_reservation() {
    let (first, second) = (Held::new(0), Held::new(0));
    let (a, b) = (first.pid(), second.pid());
    let fake = Fake::new([
        print(&running(a)),
        print(&running(a)),
        procinfo(a),
        print(&running(b)),
        print(&running(b)),
        procinfo(b),
        print(IDLE),
    ]);
    let tree = Tree::new();
    let mgr = launchd_over(fake.clone());
    let mut handed = reserved(&mgr, Step::Stop);

    mgr.stop(task_snapshot(&tree, false), Budget::Unbounded, &mut handed)
        .expect("a stop of two runs");

    assert_eq!(bounded::test_hook::left(&handed), (super::super::STOP_REQUESTS, 0));
}

/// A pass after the first that cannot reserve returns that error, and the SIGTERM it follows stands.
#[skuld::test]
fn a_later_stop_pass_that_cannot_reserve_returns_the_reservation_error() {
    let run = Held::new(0);
    let pid = run.pid();
    let fake = Fake::new([print(&running(pid)), print(&running(pid)), procinfo(pid)]);
    let tree = Tree::new();
    let mgr = launchd_over(fake.clone());
    let mut handed = reserved(&mgr, Step::Stop);
    let _short = bounded::test_hook::temp_files(0);

    let e = mgr
        .stop(task_snapshot(&tree, false), Budget::Unbounded, &mut handed)
        .expect_err("the second pass cannot reserve");

    assert!(
        matches!(&e, Error::Reservation { id: Some(id), .. } if id == ID),
        "{e:?}"
    );
    fake.assert_spent("the signal went out before the reservation failed");
}

/// A later pass of a task's `stop` makes no request, so the reservation it makes holds none: it is made
/// with no thread to give, as it is for a pass that draws no reaper.
#[skuld::test]
fn a_later_stop_pass_reserves_no_reaper() {
    let (first, second) = (Held::new(0), Held::new(0));
    let (a, b) = (first.pid(), second.pid());
    let fake = Fake::new([
        print(&running(a)),
        print(&running(a)),
        procinfo(a),
        print(&running(b)),
        print(&running(b)),
        procinfo(b),
        print(IDLE),
    ]);
    let tree = Tree::new();
    let mgr = launchd_over(fake.clone());
    let mut handed = reserved(&mgr, Step::Stop);
    let _no_thread = standby::test_hook::threads(0);

    mgr.stop(task_snapshot(&tree, false), Budget::Unbounded, &mut handed)
        .expect("a stop of two runs, the second pass of which reserves no reaper");

    fake.assert_spent("both runs were ended");
}

/// One pass of a task's `stop` draws what a pass reserves, and nothing is left of it: a pass that holds a
/// process (the read, and the confirmation) and a pass that waits for a queued start (the read, and the wait).
#[skuld::test]
fn a_pass_of_a_tasks_stop_spends_what_a_pass_reserves() {
    let run = Held::new(0);
    let tree = Tree::new();
    let id = task_snapshot(&tree, false).id.clone();
    for (what, script, budget) in [
        (
            "a process held",
            vec![
                print(&running(run.pid())),
                print(&running(run.pid())),
                procinfo(run.pid()),
            ],
            Budget::Immediate,
        ),
        (
            "a queued start waited for",
            vec![print(QUEUED), print(QUEUED), print(QUEUED), print(&running(4242))],
            Budget::Unbounded,
        ),
    ] {
        let fake = Fake::new(script);
        let mgr = launchd_over(fake.clone());
        let mut own =
            super::super::own_reservation(&id, super::super::TASK_STOP_REQUESTS, super::super::TASK_STOP_CALLS)
                .expect("a pass's reservation");
        let mut course = Course::new(Some(graced()), budget, Watch::new().expect("a watch"));

        mgr.stop_pass(&id, &mut course, &mut own).expect("a pass");

        fake.assert_spent(what);
        assert_eq!(bounded::test_hook::left(&own), (0, 0), "{what}");
    }
}

/// A signal or a hold that failed is the stop's failure, in the stop's words and not `kill`'s.
#[skuld::test]
fn a_failed_send_or_hold_is_the_stops_failure() {
    for (doing, said) in [
        (format!("sending {}", Signal::Terminate.name()), "sending SIGTERM"),
        (format!("sending {}", Signal::Kill.name()), "sending SIGKILL"),
        ("holding the process".to_string(), "holding the process"),
    ] {
        let e = stop_failed(ID, &doing, io::Error::other("refused"));

        assert!(
            matches!(&e, Error::Os { what, .. } if *what == format!("stop `{ID}`: {said}")),
            "{e:?}"
        );
    }
}

/// A task is never booted out by `stop`, in any path: the label stays loaded, and a start loads nothing.
#[skuld::test]
fn stop_never_boots_a_task_out() {
    let run = Held::new(0);
    let pid = run.pid();
    let fake = Fake::new([
        print(QUEUED),
        print(&running(pid)),
        print(&running(pid)),
        print(&running(pid)),
        procinfo(pid),
        print(IDLE),
    ]);

    stopping(&fake, Budget::DEFAULT).expect("a stop");

    assert_never_booted_out(&fake);
}

/// A watch that cannot be made fails the stop before any request: no signal goes out that nothing could
/// follow.
#[skuld::test]
fn a_watch_that_cannot_be_made_is_an_error_before_any_request() {
    let fake = Fake::new([]);
    let _none = test_hook::queues(0);

    let e = stopping(&fake, Budget::DEFAULT).expect_err("no kqueue");

    assert!(e.to_string().contains("kqueue"), "{e}");
    assert_eq!(fake.calls(), Vec::<Vec<String>>::new(), "nothing may be sent");
}

/// What a launchd task's stop that ran out of budget says of the signals it sent, by what it had sent: a wait
/// that ended while the run still had its grace, or after the grace and before goetia could send SIGKILL, says
/// nothing will end the run and what does; one that ended after SIGKILL went out says it was sent, and that the
/// run is still there.
#[skuld::test]
fn a_stop_that_ran_out_says_which_signal_it_sent_and_what_ends_the_run() {
    let say = |escalation| match timed_out_stopping("frpc", Budget::Bounded(Duration::from_secs(3)), escalation) {
        Error::WaitTimeout {
            id,
            awaited: "stopped",
            waited,
            recovery,
        } => {
            assert_eq!((id.as_str(), waited), ("frpc", Duration::from_secs(3)));
            recovery
        }
        other => panic!("expected Error::WaitTimeout, got {other:?}"),
    };

    let pending = say(Escalation::Pending);
    assert!(
        pending.contains("SIGTERM") && pending.contains("sends SIGKILL after that only while it waits"),
        "{pending}"
    );
    assert!(pending.contains("`goetia daemon kill frpc` ends it"), "{pending}");

    let spent = say(Escalation::Spent);
    assert!(
        spent.contains("did not answer it") && spent.contains("before goetia sent SIGKILL"),
        "{spent}"
    );
    assert!(spent.contains("`goetia daemon kill frpc` ends it"), "{spent}");

    let sent = say(Escalation::Sent);
    assert!(
        sent.contains("was sent SIGKILL") && sent.contains("has not exited"),
        "{sent}"
    );
    assert!(
        !sent.contains("goetia daemon kill"),
        "nothing is left for `kill` to send: {sent}"
    );
}
