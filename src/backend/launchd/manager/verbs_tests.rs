//! A task's `start`, `stop` and `kill` over a scripted `launchctl` and real child processes: each child
//! stands for a run (the pid `print` shows), and the scripted `kill` ends it, so the watch that `stop`
//! waits on is a real kqueue's. Nothing here touches the system's own directories or services.

use std::io;
use std::os::unix::process::ExitStatusExt as _;
use std::process::ExitStatus;
use std::sync::Arc;
use std::time::Duration;

use super::super::manager_tests::{launchd_over, procinfo_not_naming};
use super::super::task::Shown;
use super::super::task::task_tests::{ID, Tree, marked, plists_of, service_spec, task_spec};
use super::super::watch::{test_hook, watch_tests::Held};
use super::super::write::write_tests::bootstrap;
use super::*;
use crate::backend::Backend as _;
use crate::backend::bounded::{self, Capture, Finished};
use crate::backend::client::fake::{Entry, Fake, exited, reply};
use crate::backend::launchd::generate::timer_label;
use crate::backend::standby;
use crate::manager::Step;
use crate::spec::{DaemonKind, DaemonSpec, StopAction, StopSignal, TaskFields, TaskSpec};
use cosca::identity::Resolved;

// Fixtures ============================================================================================================

const SCHEDULE: &str = "30 3 * * *";
/// A `print` of a task's job with no process.
const IDLE: &str = "\tstate = not running\n";
/// A `print` of a start launchd has accepted and not yet forked.
const QUEUED: &str = "\tstate = spawn scheduled\n";
/// A job being torn down.
const TEARING_DOWN: &str = "\tstate = SIGTERMed\n\tpid = 4242\n";

fn target() -> String {
    format!("system/{ID}")
}

fn running(pid: u32) -> String {
    format!("\tstate = running\n\tpid = {pid}\n")
}

fn print(body: &str) -> Entry {
    reply(&["print", &target()], exited(0, body, ""))
}

fn not_loaded() -> Entry {
    reply(&["print", &target()], exited(113, "", ""))
}

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

fn kickstart() -> Entry {
    reply(&["kickstart", &target()], exited(0, "", ""))
}

/// The snapshot `read` makes of an installed task, its two plists in the enabled or the staging directory.
fn task_snapshot(tree: &Tree, enabled: bool) -> Plist {
    task_snapshot_of(tree, enabled, &task_spec(Some(SCHEDULE)))
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

fn task_snapshot_of(tree: &Tree, enabled: bool, spec: &DaemonSpec) -> Plist {
    let plists = plists_of(spec);
    tree.put(enabled, ID, &plists.job);
    tree.put(
        enabled,
        &timer_label(ID),
        plists.timer.expect("a scheduled task has a timer"),
    );
    marked(tree.read())
}

fn reserved(mgr: &Launchd, step: Step) -> Reservation {
    mgr.reserve(&[step], Budget::Unbounded).expect("reserve")
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

fn calls_of(fake: &Fake, verb: &str) -> Vec<Vec<String>> {
    fake.calls().into_iter().filter(|call| call[0] == verb).collect()
}

fn assert_never_booted_out(fake: &Fake) {
    assert!(
        calls_of(fake, "bootout").is_empty(),
        "a task is never booted out by `stop`"
    );
}

fn assert_wait_timeout(e: &Error) {
    assert!(
        matches!(e, Error::WaitTimeout { awaited: "stopped", .. }),
        "expected a wait timeout, got {e:?}"
    );
}

// start ===============================================================================================================

/// The job is loaded where it is, and kicked with the plain form: no `-p`, no state read before it, and
/// no budget, whatever the caller's.
#[skuld::test]
fn start_of_a_task_loads_the_job_and_plain_kickstarts_without_a_state_read_or_a_budget() {
    for budget in [Budget::DEFAULT, Budget::Immediate, Budget::Unbounded] {
        let tree = Tree::new();
        let snapshot = task_snapshot(&tree, false);
        let job = snapshot.job().unwrap().location.path.clone();
        let fake = Fake::new([not_loaded(), bootstrap(&job), kickstart()]);
        let mgr = launchd_over(fake.clone());

        mgr.start(snapshot, budget, &mut reserved(&mgr, Step::Start))
            .expect("a start");

        fake.assert_spent("load, then a plain kickstart");
        assert_eq!(
            fake.calls().last().unwrap(),
            &["kickstart".to_string(), target()],
            "{budget:?}"
        );
    }
}

/// A job still tearing down answers for the label: it is waited out before the job is loaded.
#[skuld::test]
fn start_of_a_task_with_its_job_tearing_down_waits_it_out() {
    let tree = Tree::new();
    let snapshot = task_snapshot(&tree, false);
    let job = snapshot.job().unwrap().location.path.clone();
    let fake = Fake::new([
        print(TEARING_DOWN),
        print(TEARING_DOWN),
        not_loaded(),
        bootstrap(&job),
        kickstart(),
    ]);
    let mgr = launchd_over(fake.clone());

    mgr.start(snapshot, Budget::DEFAULT, &mut reserved(&mgr, Step::Start))
        .expect("a start");

    fake.assert_spent("the wait for the old job to go, the load, and the kick");
}

/// A job launchd holds is not loaded again, and a kick of a run in flight leaves it alone.
#[skuld::test]
fn start_of_a_loaded_task_only_kicks_it() {
    let tree = Tree::new();
    let fake = Fake::new([print(IDLE), kickstart()]);
    let mgr = launchd_over(fake.clone());

    mgr.start(
        task_snapshot(&tree, false),
        Budget::DEFAULT,
        &mut reserved(&mgr, Step::Start),
    )
    .expect("a start");

    fake.assert_spent("a read, then the kick");
}

/// Where the job's plist is decides only what `bootstrap` is given.
#[skuld::test]
fn start_is_the_same_for_a_staged_and_an_enabled_task() {
    for enabled in [false, true] {
        let tree = Tree::new();
        let snapshot = task_snapshot(&tree, enabled);
        let job = snapshot.job().unwrap().location.path.clone();
        assert!(job.starts_with(tree.dir(enabled)), "the job is where the test put it");
        let fake = Fake::new([not_loaded(), bootstrap(&job), kickstart()]);
        let mgr = launchd_over(fake.clone());

        mgr.start(snapshot, Budget::DEFAULT, &mut reserved(&mgr, Step::Start))
            .expect("a start");

        fake.assert_spent(if enabled { "an enabled task" } else { "a staged task" });
    }
}

/// A start requested right after a stop nobody waited for (`restart --timeout 0`) over a job that still
/// has the run that was sent SIGTERM: `kickstart` would start nothing and answer success (measured), so
/// it is refused, naming the pid, and nothing is sent: one read, no `kickstart`, no `bootstrap`.
#[skuld::test]
fn a_start_after_an_unwaited_stop_is_refused_while_the_job_still_has_its_run() {
    let tree = Tree::new();
    let fake = Fake::new([print(&running(4242))]);
    let mgr = launchd_over(fake.clone());

    let e = mgr
        .request_start_after_stop(
            task_snapshot(&tree, false),
            &mut reserved(&mgr, Step::RequestStartAfterStop),
        )
        .expect_err("a start over a run still in flight is refused");

    assert!(matches!(e, Error::StillRunning { pid: 4242, .. }), "{e:?}");
    assert_eq!(
        e.to_string(),
        format!(
            "`{ID}` still has the run that was asked to stop (pid 4242), and the service manager starts no new run \
             of a task while one is in flight, so no start was issued; `goetia daemon start {ID}` starts one once \
             it has exited, and `goetia daemon restart {ID}` without `--timeout 0` waits for the exit"
        )
    );
    assert!(!e.is_indeterminate(), "nothing was sent");
    fake.assert_spent("the one read");
    assert!(calls_of(&fake, "kickstart").is_empty() && calls_of(&fake, "bootstrap").is_empty());
}

/// Whatever shows a process is the run: a job whose `print` reads `SIGTERMed` with a pid is refused too.
#[skuld::test]
fn a_start_after_an_unwaited_stop_is_refused_for_every_state_that_shows_a_pid() {
    for body in [TEARING_DOWN.to_string(), running(7)] {
        let tree = Tree::new();
        let fake = Fake::new([print(&body)]);
        let mgr = launchd_over(fake.clone());

        let e = mgr
            .request_start_after_stop(
                task_snapshot(&tree, true),
                &mut reserved(&mgr, Step::RequestStartAfterStop),
            )
            .expect_err("refused");

        assert!(matches!(e, Error::StillRunning { .. }), "{body}: {e:?}");
        fake.assert_spent("the one read");
    }
}

/// With no process left it is the task's `start` as before, its one read first: the longest path (the
/// job not loaded: the check, the load's read and `bootstrap`, the plain `kickstart`) stays inside what a
/// start reserves, which the reservation asserts.
#[skuld::test]
fn a_start_after_an_unwaited_stop_over_an_idle_job_is_a_start() {
    for idle in [IDLE, QUEUED] {
        let tree = Tree::new();
        let snapshot = task_snapshot(&tree, false);
        let job = snapshot.job().unwrap().location.path.clone();
        let fake = Fake::new([print(idle), not_loaded(), bootstrap(&job), kickstart()]);
        let mgr = launchd_over(fake.clone());

        mgr.request_start_after_stop(snapshot, &mut reserved(&mgr, Step::RequestStartAfterStop))
            .expect("a start over a job with no process");

        fake.assert_spent("the read, the load, the plain kickstart");
    }
}

/// A read that failed or was not whole concludes nothing, and a `kickstart` over a run that is still
/// there starts nothing and answers success (measured): so no start is sent, and the error is `stop`'s
/// failed-read one, in the words `stop` uses, indeterminate.
#[skuld::test]
fn a_start_after_an_unwaited_stop_is_not_sent_when_the_read_failed() {
    let cut = Finished::Exited {
        status: ExitStatus::from_raw(0),
        capture: Capture {
            stdout: b"\tstate = not run".to_vec(),
            stderr: Vec::new(),
            complete: false,
        },
    };
    let failed = reply(
        &["print", &target()],
        exited(5, "", "Could not print: 5: Input/output error\n"),
    );
    for (what, read) in [
        ("a print that failed", failed),
        ("a print that was not whole", reply(&["print"], cut)),
    ] {
        let tree = Tree::new();
        let fake = Fake::new([read]);
        let mgr = launchd_over(fake.clone());

        let e = mgr
            .request_start_after_stop(
                task_snapshot(&tree, false),
                &mut reserved(&mgr, Step::RequestStartAfterStop),
            )
            .expect_err("a start is not sent over a read that failed");

        assert!(matches!(e, Error::StopUnconfirmed { .. }), "{what}: {e:?}");
        assert_eq!(
            e.to_string(),
            format!(
                "`{ID}` may not have stopped: {}",
                Shown::Failed.describe(State::Unknown)
            ),
            "{what}"
        );
        assert!(e.is_indeterminate(), "{what}: the exit code is 4");
        fake.assert_spent("the one read");
        assert!(
            calls_of(&fake, "kickstart").is_empty() && calls_of(&fake, "bootstrap").is_empty(),
            "{what}: nothing is sent"
        );
    }
}

/// A `print` with no pid and a state that is neither idle nor a start launchd has accepted concludes
/// nothing, as it does for `stop`: no start is sent, and the error is `stop`'s, with what launchd showed.
#[skuld::test]
fn a_start_after_an_unwaited_stop_is_not_sent_over_any_read_stop_calls_unconfirmed() {
    for (what, read, shown) in unconfirmed_reads() {
        let tree = Tree::new();
        let fake = Fake::new([read]);
        let mgr = launchd_over(fake.clone());

        let e = mgr
            .request_start_after_stop(
                task_snapshot(&tree, false),
                &mut reserved(&mgr, Step::RequestStartAfterStop),
            )
            .expect_err(what);

        assert!(
            matches!(&e, Error::StopUnconfirmed { id, .. } if id == ID),
            "{what}: {e:?}"
        );
        assert_eq!(e.to_string(), format!("`{ID}` may not have stopped: {shown}"), "{what}");
        assert!(e.is_indeterminate(), "{what}: the exit code is 4");
        fake.assert_spent("the one read");
        assert!(
            calls_of(&fake, "kickstart").is_empty() && calls_of(&fake, "bootstrap").is_empty(),
            "{what}: nothing is sent"
        );
    }
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

/// Every `print` with no pid that is neither idle nor a start launchd has accepted, as the read and as
/// the words `StopUnconfirmed` says of it.
fn unconfirmed_reads() -> Vec<(&'static str, Entry, &'static str)> {
    let cut = Finished::Exited {
        status: ExitStatus::from_raw(0),
        capture: Capture {
            stdout: b"\tstate = not run".to_vec(),
            stderr: Vec::new(),
            complete: false,
        },
    };
    let failed = "reading its job from launchd failed, so whether a process remains is not established";
    vec![
        (
            "a state goetia does not know",
            print("\tstate = someday-new-apple-state\n"),
            "launchd shows its job in state `someday-new-apple-state` with no process, a state goetia does not know",
        ),
        (
            "xpcproxy without a pid",
            print("\tstate = xpcproxy\n"),
            "launchd shows its job in state `xpcproxy` with no process, a state goetia does not know",
        ),
        (
            "a state goetia knows, with no pid line",
            print("\tstate = running\n"),
            "launchd shows its job in state `running` with no process",
        ),
        (
            "no state line",
            print("system/x = {\n\tactive count = 1\n}\n"),
            "launchd shows its job with no process and no state",
        ),
        (
            "a print that failed",
            reply(
                &["print", &target()],
                exited(5, "", "Could not print: 5: Input/output error\n"),
            ),
            failed,
        ),
        ("a print that was not whole", reply(&["print", &target()], cut), failed),
    ]
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
    // The last read is the one a stop makes once its deadline is spent: the run is still there.
    let fake = Fake::new([
        print(&running(pid)),
        print(&running(pid)),
        procinfo(pid),
        print(&running(pid)),
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
        // The read once the deadline is spent.
        print(&running(pid)),
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
        recovery.contains("SIGKILL") && recovery.contains("goetia daemon kill"),
        "{recovery}"
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

    stopping_as(&fake, Budget::Unbounded, graced()).expect("a stop that ends the run");

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
/// ran out) then ends the stop at its next pass, which reads once and finds the run still there.
#[skuld::test]
fn a_stop_with_a_spent_budget_sends_its_first_signal_and_then_reports_the_timeout() {
    for budget in [Duration::ZERO, Duration::from_nanos(1)] {
        let mut run = Held::new(0);
        let pid = run.pid();
        let fake = Fake::new([
            print(&running(pid)),
            print(&running(pid)),
            procinfo(pid),
            // The read the next pass makes once, with its deadline spent.
            print(&running(pid)),
        ]);

        let e = stopping(&fake, Budget::Bounded(budget)).expect_err("a deadline that is spent");

        assert_wait_timeout(&e);
        fake.assert_spent("the first signal, and the one read after it");
        assert_eq!(ended_by(&mut run), Some(libc::SIGTERM), "{budget:?}: SIGTERM went out");
    }
}

/// A zero bound still waits for a queued start's pid, as every budget does, and signals the run it begins.
#[skuld::test]
fn a_stop_with_no_time_waits_for_a_queued_start_signals_it_and_reports_the_timeout() {
    let mut run = Held::new(0);
    let pid = run.pid();
    let fake = Fake::new([
        print(QUEUED),
        print(QUEUED),
        print(&running(pid)),
        print(&running(pid)),
        print(&running(pid)),
        procinfo(pid),
        print(&running(pid)),
    ]);

    let e = stopping(&fake, Budget::Bounded(Duration::ZERO)).expect_err("a deadline that is spent");

    assert_wait_timeout(&e);
    fake.assert_spent("the queued start, the signal, and the one read after it");
    assert_eq!(ended_by(&mut run), Some(libc::SIGTERM), "SIGTERM went out");
}

/// Once the first signal is sent, a spent deadline sends nothing more, and the pass that finds it spent reads
/// `print` once. A start launchd accepted is the timeout; no process and a state that is not idle (or a read
/// that was not whole) is the stop's `StopUnconfirmed`, as in any pass.
#[skuld::test]
fn a_stop_whose_deadline_is_spent_after_its_first_signal_reads_once_and_ends_as_the_read_says() {
    let failed = reply(
        &["print", &target()],
        exited(5, "", "Could not print: 5: Input/output error
"),
    );
    let reads = [
        ("a queued start", print(QUEUED), true),
        (
            "a state that says nothing",
            print("	state = someday-new-apple-state
"),
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
    ]);
    let recovery = timeout_of(&fake, graced());
    assert!(recovery.contains("was sent SIGTERM"), "{recovery}");
    run.finish();

    // A run sent SIGKILL, gone; another run is up.
    let (mut killed, mut next) = (Held::ignoring_sigterm(), Held::ignoring_sigterm());
    let (a, b) = (killed.pid(), next.pid());
    let fake = Fake::new([print(&running(a)), print(&running(a)), procinfo(a), print(&running(b))]);
    let recovery = timeout_of(&fake, kill);
    assert!(recovery.contains("was not signalled"), "{recovery}");
    assert!(!recovery.contains("was sent SIGKILL"), "{recovery}");
    assert_eq!(ended_by(&mut killed), Some(libc::SIGKILL));
    assert_eq!(next.finish(), Some(0), "the run that was up was sent nothing");

    // A run sent SIGTERM, gone; a start is queued.
    let mut run = Held::new(0);
    let pid = run.pid();
    let fake = Fake::new([
        print(&running(pid)),
        print(&running(pid)),
        procinfo(pid),
        print(QUEUED),
    ]);
    let recovery = timeout_of(&fake, graced());
    assert!(recovery.contains("was not signalled"), "{recovery}");
    assert_eq!(ended_by(&mut run), Some(libc::SIGTERM));
}

/// The deadline is decided once more right before every send after the first. Here it passes during the read
/// that shows the next run, after the pass began with time left: that run is not sent its first signal, and
/// the stop reports the timeout. (The read is slow by the fake's own sleep; the deadline is far enough that
/// only that sleep spends it.)
#[skuld::test]
fn a_deadline_that_passes_before_a_later_runs_first_signal_sends_nothing_to_it() {
    let mut first = Held::new(0);
    let mut second = Held::ignoring_sigterm();
    let (a, b) = (first.pid(), second.pid());
    let slow = Duration::from_millis(1500);
    let fake = Fake::new([
        print(&running(a)),
        print(&running(a)),
        procinfo(a),
        // The next pass begins with time left; this read, which shows the next run, spends it.
        print(&running(b)).then(move || std::thread::sleep(slow)),
        print(&running(b)),
        procinfo(b),
        // The pass after it finds the deadline spent, and reads once.
        print(&running(b)),
    ]);

    let e = stopping(&fake, Budget::Bounded(Duration::from_millis(1000))).expect_err("a deadline that passed");

    assert_wait_timeout(&e);
    fake.assert_spent("the first run's signal, the next run held and not signalled, and the one read");
    assert_eq!(ended_by(&mut first), Some(libc::SIGTERM));
    assert_eq!(second.finish(), Some(0), "the next run was sent nothing");
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

    stopping_as(&fake, Budget::Unbounded, graced()).expect("a stop that ends the run");

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
    assert_eq!(course(None, false).escalation(mine), None, "SIGTERM and no grace to follow it");
    for killed in [false, true] {
        assert_eq!(
            course(Some(over), killed).escalation(other),
            Some(Escalation::Unsent),
            "another run was sent nothing, whatever this one was"
        );
    }
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

    assert_eq!(course(graced()).due(mine), Some(Signal::Term), "its first");
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
        Some(Signal::Term),
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

/// The wait for a queued start's pid is not spent from the caller's budget: a stop whose budget is the
/// `timeout` and a margin still gives the run that follows the whole `timeout` before the kill, however
/// long launchd's throttle held the start.
#[skuld::test]
fn a_queued_starts_wait_does_not_spend_the_grace_of_the_run_that_follows() {
    let mut run = Held::ignoring_sigterm();
    let pid = run.pid();
    let throttled = Duration::from_millis(1600);
    let fake = Fake::new([
        print(QUEUED),
        print(QUEUED),
        print(QUEUED).then(move || std::thread::sleep(throttled)),
        print(&running(pid)),
        print(&running(pid)),
        print(&running(pid)),
        procinfo(pid),
        print(&running(pid)),
        procinfo(pid),
        print(IDLE),
    ]);
    let action = StopAction {
        send_signal: StopSignal::Terminate,
        timeout: Some(StopAction::MIN_TIMEOUT),
    };
    // Less than the throttle plus the `timeout`, as the default of a stop has the `timeout` and a margin.
    let budget = Budget::Bounded(Duration::from_millis(2500));

    stopping_as(&fake, budget, action).expect("the run is killed once its timeout is over");

    fake.assert_spent("the queued start, SIGTERM, the timeout, and the kill");
    assert_no_label_signal(&fake);
    assert_eq!(ended_by(&mut run), Some(libc::SIGKILL), "the run was ended by SIGKILL");
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
        // The read once the deadline is spent.
        print(&running(pid)),
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

    stopping_as(&fake, Budget::Unbounded, action).expect("a stop that ends the second run");

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
        (Signal::Term.sending(), "sending SIGTERM"),
        (Signal::Kill.sending(), "sending SIGKILL"),
        ("holding the process", "holding the process"),
    ] {
        let e = stop_failed(ID, doing, io::Error::other("refused"));

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

// kill ================================================================================================================

/// A `print` of a job whose process was killed and whose restart launchd has not yet made.
const KILLED: &str = "\tstate = spawn scheduled\n";

/// `launchctl procinfo` naming `pid` as a launch of the job.
fn procinfo(pid: u32) -> Entry {
    let block = format!("program path = /bin/sh\n\nsystem/{ID} = {{\n\tstate = running\n\tpid = {pid}\n}}\n");
    reply(&["procinfo", &pid.to_string()], exited(0, &block, ""))
}

/// The kill of a task's run, whose pid `run` is: the read, the confirmation (a read, and `procinfo`
/// naming the process held), and the record's wait, whose first read still shows the pid.
fn showing(run: &Held, then: Entry) -> [Entry; 5] {
    let pid = run.pid();
    [
        print(&running(pid)),
        print(&running(pid)),
        procinfo(pid),
        print(&running(pid)),
        then,
    ]
}

/// SIGKILL to the run through a hold, and the wait until launchd stops showing its pid, for a task as for
/// a service: no `launchctl kill` is sent. A job with no process, and no start launchd has accepted, is
/// `NotRunning`, after the read that found no pid and the one that asks what that means.
#[skuld::test]
fn kill_of_a_task_ends_its_run_and_not_running_when_idle() {
    let tree = Tree::new();
    let mut run = Held::new(0);
    let fake = Fake::new(showing(&run, print(KILLED)).into_iter().chain([
        print(IDLE),
        print(IDLE),
        not_loaded(),
        not_loaded(),
    ]));
    let mgr = launchd_over(fake.clone());
    let mut held = mgr.reserve(&[Step::Kill; 3], Budget::Unbounded).expect("reserve");

    mgr.kill(task_snapshot(&tree, false), &mut held).expect("a kill");
    for _ in 0..2 {
        let e = mgr
            .kill(task_snapshot(&tree, false), &mut held)
            .expect_err("an idle task has no process");
        assert!(matches!(&e, Error::NotRunning { id } if id == ID), "{e:?}");
    }

    fake.assert_spent("a kill that read the run and waited for it to go, and two that found none");
    assert!(
        calls_of(&fake, "kill").is_empty(),
        "the signal is not a launchctl request"
    );
    assert_eq!(run.finish(), None, "the run was signalled to its end");
}

/// `kickstart` returns before the fork, so a `kill` straight after a `start` finds a start launchd has
/// accepted and no process: nothing can be signalled, and the run goes ahead. `status` calls that moment
/// `Running`, so the kill waits for the pid, as `stop` does, and then kills it. The longest path draws
/// exactly what a kill reserved.
#[skuld::test]
fn kill_of_a_task_whose_start_is_still_queued_waits_for_the_pid_and_kills_it() {
    let tree = Tree::new();
    let mut run = Held::new(0);
    let pid = run.pid();
    let fake = Fake::new(
        [print(QUEUED), print(QUEUED), print(QUEUED), print(&running(pid))]
            .into_iter()
            .chain(showing(&run, print(KILLED))),
    );
    let mgr = launchd_over(fake.clone());
    let mut held = reserved(&mgr, Step::Kill);

    mgr.kill(task_snapshot(&tree, false), &mut held)
        .expect("a kill of a queued start");

    fake.assert_spent("the first look, the read, the wait for the pid, and the kill of the run that forked");
    assert!(calls_of(&fake, "kill").is_empty());
    assert_never_booted_out(&fake);
    assert_eq!(
        bounded::test_hook::left(&held),
        (0, 0),
        "the longest path spends the kill's reservation exactly"
    );
    assert_eq!(run.finish(), None);
}

/// A start that forks between the first look and the read shows its pid: it is killed, with no wait for
/// the fork.
#[skuld::test]
fn kill_of_a_start_that_forked_after_the_first_look_kills_the_run() {
    let tree = Tree::new();
    let mut run = Held::new(0);
    let pid = run.pid();
    let fake = Fake::new(
        [print(IDLE), print(&running(pid))]
            .into_iter()
            .chain(showing(&run, print(KILLED))),
    );
    let mgr = launchd_over(fake.clone());
    let mut held = reserved(&mgr, Step::Kill);

    mgr.kill(task_snapshot(&tree, false), &mut held)
        .expect("a kill of the run that forked");

    fake.assert_spent("the first look, the read that shows the pid, and the kill");
    assert_never_booted_out(&fake);
    assert_eq!(run.finish(), None);
}

/// A start that is accepted and then does not happen leaves `spawn scheduled` with no pid: the wait
/// ends on the state, and the kill that follows finds no process.
#[skuld::test]
fn kill_of_a_queued_start_that_never_forks_is_not_running() {
    let tree = Tree::new();
    let fake = Fake::new([print(QUEUED), print(QUEUED), print(QUEUED), print(IDLE), print(IDLE)]);
    let mgr = launchd_over(fake.clone());
    let mut held = reserved(&mgr, Step::Kill);

    let e = mgr
        .kill(task_snapshot(&tree, false), &mut held)
        .expect_err("there is nothing to kill");

    assert!(matches!(&e, Error::NotRunning { id } if id == ID), "{e:?}");
    fake.assert_spent("the first look, the read, the wait that ended on the state, and the second look");
}

/// A first look that finds no pid, followed by a read that cannot say no run is left (a read that failed
/// or was not whole, or a no-pid state that is neither idle nor queued) is `StopUnconfirmed`, as for
/// `stop`: not a determinate `NotRunning`, and nothing is killed.
#[skuld::test]
fn kill_of_a_task_whose_read_after_the_first_look_is_unconfirmed_says_so() {
    for (what, read, shown) in unconfirmed_reads() {
        let tree = Tree::new();
        let fake = Fake::new([print(IDLE), read]);
        let mgr = launchd_over(fake.clone());
        let mut held = reserved(&mgr, Step::Kill);

        let e = mgr.kill(task_snapshot(&tree, false), &mut held).expect_err(what);

        assert!(
            matches!(&e, Error::StopUnconfirmed { id, .. } if id == ID),
            "{what}: {e:?}"
        );
        assert_eq!(e.to_string(), format!("`{ID}` may not have stopped: {shown}"), "{what}");
        assert!(e.is_indeterminate(), "{what}: the exit code is 4");
        fake.assert_spent("the first look and the read");
    }
}

/// `restart` is a stop and then the plain start: the start that follows a stop is the request alone.
#[skuld::test]
fn restart_of_a_task_is_stop_then_a_plain_start() {
    let tree = Tree::new();
    let snapshot = || task_snapshot(&tree, false);
    let job = snapshot().job().unwrap().location.path.clone();
    let fake = Fake::new([print(IDLE), print(IDLE), not_loaded(), bootstrap(&job), kickstart()]);
    let mgr = launchd_over(fake.clone());
    let mut held = mgr
        .reserve(&[Step::Stop, Step::RequestStartAfterStop], Budget::DEFAULT)
        .expect("reserve");

    mgr.stop(snapshot(), Budget::DEFAULT, &mut held).expect("a stop");
    mgr.request_start_after_stop(snapshot(), &mut held).expect("a start");

    fake.assert_spent("the stop's read, the start's read of the job, then the load and the plain kick");
    assert_never_booted_out(&fake);
}

// The kind of an id ===================================================================================================

/// A job whose metadata will not decode is a task if a goetia timer sits beside it, and a service if not.
#[skuld::test]
fn the_kind_of_an_undecodable_blob_is_task_iff_a_timer_is_present() {
    let tree = Tree::new();
    let with_timer = task_snapshot(&tree, false);
    let damaged = |plist: Plist, timer: bool| Plist {
        blob: Err(Arc::new(Error::blob("the metadata will not decode"))),
        timer: plist.timer.filter(|_| timer),
        ..plist
    };

    assert_eq!(with_timer.kind(), DaemonKind::Task);
    assert_eq!(
        damaged(task_snapshot(&Tree::new(), false), true).kind(),
        DaemonKind::Task
    );
    assert_eq!(
        damaged(task_snapshot(&Tree::new(), false), false).kind(),
        DaemonKind::Service
    );
}

/// A service whose metadata is damaged must keep working as one: its `stop` boots it out, as before.
#[skuld::test]
fn stop_of_a_service_with_an_undecodable_blob_and_no_timer_boots_it_out() {
    let tree = Tree::new();
    tree.put(false, ID, plists_of(&service_spec()).job);
    let service = marked(tree.read());
    let snapshot = Plist {
        blob: Err(Arc::new(Error::blob("the metadata will not decode"))),
        ..service
    };
    let fake = Fake::new([reply(&["bootout", &target()], exited(0, "", "")), not_loaded()]);
    let mgr = launchd_over(fake.clone());

    mgr.stop(snapshot, Budget::DEFAULT, &mut reserved(&mgr, Step::Stop))
        .expect("a stop of a damaged service");

    fake.assert_spent("the bootout and the wait for the job to go");
}
