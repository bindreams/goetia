//! A task's `start`, `stop` and `kill` over a scripted `launchctl` and real child processes: each child
//! stands for a run (the pid `print` shows), and the scripted `kill` ends it, so the watch that `stop`
//! waits on is a real kqueue's. Nothing here touches the system's own directories or services.

use std::os::unix::process::ExitStatusExt as _;
use std::process::ExitStatus;
use std::sync::Arc;
use std::time::{Duration, Instant};

use cosca::identity::{ProcessId, Resolved};

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
use crate::spec::{DaemonKind, DaemonSpec, StopAction, StopSignal, TaskFields, TaskSpec};

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

/// The SIGTERM `stop` sends, by label.
fn sigterm() -> Entry {
    reply(&["kill", "SIGTERM", &target()], exited(0, "", ""))
}

/// A SIGTERM that ends `run`, as the real one would.
fn sigterm_ending(mut run: Held) -> Entry {
    sigterm().then(move || {
        run.finish();
    })
}

/// The SIGKILL a stop sends: at once for `send-signal: kill`, after the `timeout` for a run that ignores
/// SIGTERM.
fn sigkill() -> Entry {
    reply(&["kill", "SIGKILL", &target()], exited(0, "", ""))
}

/// A SIGKILL that ends `run`, as the real one would.
fn sigkill_ending(mut run: Held) -> Entry {
    sigkill().then(move || {
        run.finish();
    })
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
            "`{ID}` still has the run that was sent SIGTERM (pid 4242), and launchd starts no new run of a task \
             while one is in flight, so no start was issued; `goetia daemon start {ID}` starts one once it has \
             exited, and `goetia daemon restart {ID}` without `--timeout 0` waits for the exit"
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

/// The pid `print` showed is watched, confirmed, signalled with SIGTERM by label, and waited for: the
/// stop returns once the run is gone. The first pass draws exactly what a stop reserved.
#[skuld::test]
fn stop_signals_sigterm_and_waits_for_the_pid_it_saw() {
    let run = Held::new(0);
    let pid = run.pid();
    let fake = Fake::new([
        print(&running(pid)),
        print(&running(pid)),
        procinfo(pid),
        sigterm_ending(run),
        print(IDLE),
    ]);
    let tree = Tree::new();
    let mgr = launchd_over(fake.clone());
    let mut held = reserved(&mgr, Step::Stop);

    mgr.stop(task_snapshot(&tree, false), Budget::Unbounded, &mut held)
        .expect("a stop");

    fake.assert_spent("read, confirm, signal, and the read that finds no process");
    assert_eq!(calls_of(&fake, "kill"), [["kill", "SIGTERM", target().as_str()]]);
    assert_never_booted_out(&fake);
    assert_eq!(
        bounded::test_hook::left(&held),
        (0, 0),
        "the first pass spends the stop's reservation exactly"
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
    let run = Held::new(0);
    let pid = run.pid();
    let fake = Fake::new([
        print(&running(pid)),
        print(&running(pid)),
        procinfo(pid),
        sigterm_ending(run),
        print("\tstate = someday-new-apple-state\n"),
    ]);

    let e = stopping(&fake, Budget::Unbounded).expect_err("the next pass cannot tell");

    assert!(matches!(e, Error::StopUnconfirmed { .. }), "{e:?}");
    fake.assert_spent("the signal went out, and the next read was not conclusive");
    assert_eq!(calls_of(&fake, "kill").len(), 1);
}

/// `kickstart` returns before the fork, so a `stop` straight after a `start` finds a start launchd has
/// accepted and no pid. A signal then stops nothing (launchd answers "No process to signal" and the
/// start goes on), so the stop waits for the pid, and then ends the run.
#[skuld::test]
fn stop_of_a_task_whose_start_is_still_queued_waits_for_the_pid_and_ends_the_run() {
    let run = Held::new(0);
    let pid = run.pid();
    let fake = Fake::new([
        print(QUEUED),
        print(QUEUED),
        print(QUEUED),
        print(&running(pid)),
        print(&running(pid)),
        print(&running(pid)),
        procinfo(pid),
        sigterm_ending(run),
        print(IDLE),
    ]);

    stopping(&fake, Budget::Unbounded).expect("a stop of a queued start");

    fake.assert_spent("reads until the pid showed, then the signal and the read that finds none");
    assert_eq!(calls_of(&fake, "kill").len(), 1, "SIGTERM is sent once");
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

/// `kill` answers exit 3 for a job with no process: the run ended before the signal, which is the next
/// pass, and which finds none.
#[skuld::test]
fn stop_with_signal_no_process_is_ok() {
    let run = Held::new(0);
    let pid = run.pid();
    let fake = Fake::new([
        print(&running(pid)),
        print(&running(pid)),
        procinfo(pid),
        reply(
            &["kill", "SIGTERM", &target()],
            exited(3, "", "No process to signal.\n"),
        ),
        print(IDLE),
    ]);

    stopping(&fake, Budget::Unbounded).expect("a stop");

    fake.assert_spent("the signal found no process, and the next read none");
}

/// A run that began while `stop` waited for the one before is a real run, and is ended too; the loop
/// ends only when `print` shows no process.
#[skuld::test]
fn stop_ends_a_run_that_begins_while_it_is_stopping() {
    let (first, second) = (Held::new(0), Held::new(0));
    let (a, b) = (first.pid(), second.pid());
    let fake = Fake::new([
        print(&running(a)),
        print(&running(a)),
        procinfo(a),
        sigterm_ending(first),
        print(&running(b)),
        print(&running(b)),
        procinfo(b),
        sigterm_ending(second),
        print(IDLE),
    ]);

    stopping(&fake, Budget::Unbounded).expect("a stop");

    fake.assert_spent("both runs were ended");
    assert_eq!(calls_of(&fake, "kill").len(), 2);
}

/// The pid that was read may be reused by the time it is watched: only a job that `print` still shows
/// with that pid is signalled.
#[skuld::test]
fn stop_with_a_pid_that_is_not_the_jobs_any_more_is_ok_and_sends_nothing() {
    let other = Held::new(0);
    let fake = Fake::new([print(&running(other.pid())), print(IDLE), print(IDLE)]);

    stopping(&fake, Budget::Unbounded).expect("a stop");

    fake.assert_spent("the confirming read showed another job, and the next pass found none");
    assert!(
        calls_of(&fake, "kill").is_empty(),
        "nothing is sent to a job that is not the one watched"
    );
}

/// `stop` waits for the run to be over and reads no status, so an exit the kernel reported without one
/// ends the wait like any other: the stop goes on to read again, and succeeds.
#[skuld::test]
fn stop_takes_an_exit_the_kernel_reported_without_a_status() {
    let _unreported = test_hook::statuses(0);
    let run = Held::new(0);
    let pid = run.pid();
    let fake = Fake::new([
        print(&running(pid)),
        print(&running(pid)),
        procinfo(pid),
        sigterm_ending(run),
        print(IDLE),
    ]);

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
    let run = Held::new(0);
    let pid = run.pid();
    let fake = Fake::new([print(&running(pid)), print(&running(pid)), procinfo(pid), sigterm()]);

    let e = stopping(&fake, Budget::Bounded(Duration::from_millis(100))).expect_err("a run that ignores SIGTERM");

    assert_wait_timeout(&e);
    fake.assert_spent("the signal was sent, and nothing after it");
}

/// The wait that ends while a run still has its grace to answer SIGTERM says what that costs: nothing
/// sends SIGKILL after it, and `goetia daemon kill` does. A wait that ends with no grace pending has
/// nothing to say of it.
#[skuld::test]
fn a_wait_that_ends_inside_the_grace_names_the_limit_and_one_that_does_not_stays_plain() {
    // `sent` is the signal the fake answers, which does not end the run.
    let recovery = |action: StopAction, sent: Entry| {
        let run = Held::new(0);
        let pid = run.pid();
        let fake = Fake::new([print(&running(pid)), print(&running(pid)), procinfo(pid), sent]);
        match stopping_as(&fake, Budget::Bounded(Duration::from_millis(100)), action) {
            Err(Error::WaitTimeout { recovery, .. }) => recovery,
            other => panic!("expected a wait timeout, got {other:?}"),
        }
    };

    let in_grace = recovery(StopAction::default(), sigterm());
    assert!(
        in_grace.contains("SIGKILL") && in_grace.contains("goetia daemon kill"),
        "{in_grace}"
    );

    // `kill` sends SIGKILL at once: no grace is pending.
    let no_grace = recovery(
        StopAction {
            send_signal: StopSignal::Kill,
            timeout: None,
        },
        sigkill(),
    );
    assert!(!no_grace.contains("goetia daemon kill"), "{no_grace}");
}

/// A stop that ignores SIGTERM for as long as the `timeout` is escalated: SIGKILL is sent when it is over,
/// and the stop returns once the run is gone. The `timeout` is the grace the user configured, as it is
/// for systemd's `TimeoutStopSec=`. The SIGKILL goes to the process held, once launchd has confirmed it
/// again, and not to the label: the fake has no `kill SIGKILL` to answer, and the run still dies of the
/// signal.
#[skuld::test]
fn stop_sends_sigkill_to_a_run_that_ignores_sigterm_for_the_timeout() {
    let mut run = Held::new(0);
    let pid = run.pid();
    let fake = Fake::new([
        print(&running(pid)),
        print(&running(pid)),
        procinfo(pid),
        sigterm(),
        // The timeout is over: launchd is asked again whether the process held is the job's, and then it is
        // killed through the hold.
        print(&running(pid)),
        procinfo(pid),
        print(IDLE),
    ]);
    let action = StopAction {
        send_signal: StopSignal::Terminate,
        timeout: Some(StopAction::MIN_TIMEOUT),
    };
    let began = Instant::now();

    stopping_as(&fake, Budget::Unbounded, action).expect("a stop that ends the run");

    fake.assert_spent("SIGTERM, then the timeout, then the confirmation and the kill of the process held");
    assert!(
        began.elapsed() >= StopAction::MIN_TIMEOUT,
        "the run had its whole timeout"
    );
    assert_eq!(calls_of(&fake, "kill"), [["kill", "SIGTERM", target().as_str()]]);
    assert_eq!(run.finish(), None, "the run died of a signal, not of its own exit");
}

/// The process held is killed only if launchd still names it as the job's when the timeout is over: a
/// pid that launchd shows and does not name is a stranger's, or a run that is gone, and is left alone.
#[skuld::test]
fn a_process_launchd_no_longer_names_when_the_timeout_is_over_is_not_sent_sigkill() {
    let mut run = Held::new(0);
    let pid = run.pid();
    let fake = Fake::new([
        print(&running(pid)),
        print(&running(pid)),
        procinfo(pid),
        sigterm(),
        print(&running(pid)),
        procinfo_not_naming(pid),
        print(IDLE),
    ]);
    let action = StopAction {
        send_signal: StopSignal::Terminate,
        timeout: Some(StopAction::MIN_TIMEOUT),
    };
    let began = Instant::now();

    stopping_as(&fake, Budget::Unbounded, action).expect("a stop that finds no run to end");

    fake.assert_spent("SIGTERM, the timeout, the confirmation that refused, and the read that found none");
    assert_eq!(run.finish(), Some(0), "the process was not signalled");
    assert!(
        began.elapsed() >= StopAction::MIN_TIMEOUT + UNLOAD_POLL,
        "the refusal is paced, as the one before the SIGTERM is, while launchd's view catches up"
    );
}

/// SIGTERM goes by label, so a later pass can find the run that was sent it again: here launchd's lookup
/// did not name the process when the grace was over, and does by the next pass. That pass sends no second
/// SIGTERM, and the grace it finds is the one that began at the first: it is over, and the run is killed
/// through the hold at once.
#[skuld::test]
fn a_run_found_again_is_not_sent_a_second_sigterm_and_keeps_the_grace_that_began_at_the_first() {
    let mut run = Held::new(0);
    let pid = run.pid();
    let fake = Fake::new([
        print(&running(pid)),
        print(&running(pid)),
        procinfo(pid),
        sigterm(),
        // The grace is over, and launchd does not name the process held.
        print(&running(pid)),
        procinfo_not_naming(pid),
        // The next pass holds the same process: the grace is over, so it is killed through the hold.
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

    stopping_as(&fake, Budget::Unbounded, action).expect("a stop that ends the run");

    fake.assert_spent("SIGTERM once, the refused confirmation, then the kill of the same run");
    assert_eq!(calls_of(&fake, "kill"), [["kill", "SIGTERM", target().as_str()]]);
    assert_eq!(run.finish(), None, "the run died of a signal");
}

/// A pid that launchd shows and does not name is not signalled at all, SIGTERM included: it is read again.
#[skuld::test]
fn a_pid_launchd_shows_and_procinfo_does_not_name_is_sent_nothing() {
    let run = Held::new(0);
    let pid = run.pid();
    let fake = Fake::new([
        print(&running(pid)),
        print(&running(pid)),
        procinfo_not_naming(pid),
        print(IDLE),
    ]);

    stopping(&fake, Budget::Unbounded).expect("a stop");

    fake.assert_spent("the confirmation refused, and the next read found none");
    assert!(
        calls_of(&fake, "kill").is_empty(),
        "a process that is not the job's is not signalled"
    );
}

/// The wait for a queued start's pid is not spent from the caller's budget: a stop whose budget is the
/// `timeout` and a margin still gives the run that follows the whole `timeout` before the kill, however
/// long launchd's throttle held the start.
#[skuld::test]
fn a_queued_starts_wait_does_not_spend_the_grace_of_the_run_that_follows() {
    let mut run = Held::new(0);
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
        sigterm(),
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
    assert_eq!(calls_of(&fake, "kill"), [["kill", "SIGTERM", target().as_str()]]);
    assert_eq!(run.finish(), None, "the run died of a signal, not of its own exit");
}

/// A queued start's wait is not counted, and the stop is still held to its budget: it ends, and says it
/// waited for the budget and for the start together.
#[skuld::test]
fn a_stop_held_to_a_budget_ends_after_a_queued_start_and_reports_the_whole_wait() {
    let run = Held::new(0);
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
        sigterm(),
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

/// `send-signal: kill` sends SIGKILL at once, and no SIGTERM.
#[skuld::test]
fn stop_with_send_signal_kill_sends_sigkill_at_once() {
    let run = Held::new(0);
    let pid = run.pid();
    let fake = Fake::new([
        print(&running(pid)),
        print(&running(pid)),
        procinfo(pid),
        sigkill_ending(run),
        print(IDLE),
    ]);
    let action = StopAction {
        send_signal: StopSignal::Kill,
        timeout: None,
    };

    stopping_as(&fake, Budget::Unbounded, action).expect("a stop that ends the run");

    fake.assert_spent("read, confirm, SIGKILL, and the read that finds no process");
    assert_eq!(calls_of(&fake, "kill"), [["kill", "SIGKILL", target().as_str()]]);
}

/// A run that ends within the `timeout` is not sent SIGKILL.
#[skuld::test]
fn stop_of_a_run_that_answers_sigterm_in_time_sends_no_sigkill() {
    let run = Held::new(0);
    let pid = run.pid();
    let fake = Fake::new([
        print(&running(pid)),
        print(&running(pid)),
        procinfo(pid),
        sigterm_ending(run),
        print(IDLE),
    ]);
    let action = StopAction {
        send_signal: StopSignal::Terminate,
        timeout: Some(StopAction::MIN_TIMEOUT),
    };

    stopping_as(&fake, Budget::Unbounded, action).expect("a stop");

    fake.assert_spent("SIGTERM ended the run");
    assert_eq!(calls_of(&fake, "kill").len(), 1);
}

/// The SIGKILL is for the run whose `timeout` ran out. A run that began after that one was told to stop
/// is a new run: it is sent SIGTERM, and has a `timeout` of its own, whole, before it is killed.
#[skuld::test]
fn the_sigkill_is_for_the_run_whose_timeout_ran_out_and_not_for_the_next() {
    let first = Held::new(0);
    let mut second = Held::new(0);
    let (a, b) = (first.pid(), second.pid());
    let fake = Fake::new([
        print(&running(a)),
        print(&running(a)),
        procinfo(a),
        sigterm(),
        // `print` shows another run by the time the timeout is over: the first is not the job's any more,
        // and the process held is left alone.
        print(&running(b)),
        // The second run is told to stop, ignores it, and is killed when its own timeout is over.
        print(&running(b)),
        print(&running(b)),
        procinfo(b),
        sigterm(),
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

    fake.assert_spent("each run was told to stop once, and the second killed");
    assert!(
        began.elapsed() >= StopAction::MIN_TIMEOUT * 2,
        "each run had its whole timeout"
    );
    assert_eq!(calls_of(&fake, "kill").len(), 2);
    assert!(calls_of(&fake, "kill").iter().all(|call| call[1] == "SIGTERM"));
    assert_eq!(second.finish(), None, "the second run died of a signal");
    drop(first);
}

/// A run's SIGTERM and grace are kept for its identity, not its pid: the same process is the run that was
/// sent SIGTERM, and another process that was given its pid, or any other run, is a new one.
#[skuld::test]
fn a_runs_sigterm_is_kept_for_its_identity_and_a_reused_pid_with_a_new_identity_is_a_new_run() {
    let (a, b) = (Held::new(0), Held::new(0));
    let found = |pid| match ProcessId::of(pid) {
        Resolved::Found(id) => id,
        _ => panic!("a live process has an identity"),
    };
    let (run, other) = (found(a.pid()), found(b.pid()));
    // The same pid, another start: what a process that was given the number later is.
    let mut record = run.to_record().expect("a record of a live process");
    record.token = record.token.wrapping_add(1);
    let reused = ProcessId::try_from(&record).expect("an identity of the same pid and another start");
    assert_ne!(reused, run, "the same pid is not the same run");
    let end = Budget::Bounded(StopAction::MIN_TIMEOUT).start();
    let mut course = Course::new(None, Budget::Unbounded, Watch::new().expect("a kqueue"));
    assert_eq!(course.termed(run), None, "nothing was sent yet");

    course.termed = Some(Termed {
        run,
        grace_end: Some(end),
    });

    assert_eq!(
        course.termed(run),
        Some(Termed {
            run,
            grace_end: Some(end)
        }),
        "the run found again keeps the grace that began at its SIGTERM"
    );
    assert_eq!(course.termed(reused), None, "a reused pid is a run with its own grace");
    assert_eq!(course.termed(other), None);
}

/// `Immediate` waits for no exit: the signal is sent and the stop returns.
#[skuld::test]
fn stop_under_immediate_of_a_running_job_sends_and_returns_ok() {
    let run = Held::new(0);
    let pid = run.pid();
    let fake = Fake::new([print(&running(pid)), print(&running(pid)), procinfo(pid), sigterm()]);

    stopping(&fake, Budget::Immediate).expect("a stop that waits for nothing");

    fake.assert_spent("read, confirm, signal");
}

/// A queued start has no pid, and a signal sent then stops nothing: even under `Immediate` the stop waits
/// for the pid, and only then signals. It does not wait for the process.
#[skuld::test]
fn stop_under_immediate_of_a_queued_start_waits_for_the_pid_then_signals() {
    let run = Held::new(0);
    let pid = run.pid();
    let fake = Fake::new([
        print(QUEUED),
        print(QUEUED),
        print(&running(pid)),
        print(&running(pid)),
        print(&running(pid)),
        procinfo(pid),
        sigterm(),
    ]);

    stopping(&fake, Budget::Immediate).expect("a stop that waits for nothing after the signal");

    fake.assert_spent("the pid showed, then one SIGTERM, and nothing after it");
    let calls = fake.calls();
    let first_signal = calls.iter().position(|call| call[0] == "kill").unwrap();
    assert!(
        calls[..first_signal]
            .iter()
            .all(|call| call[0] == "print" || call[0] == "procinfo"),
        "no signal is sent before the pid shows: {calls:?}"
    );
    assert_eq!(calls_of(&fake, "kill").len(), 1);
}

/// A zero bound is a bound that has run out: the same wait for the pid and the same signal, and then the
/// wait for the process is an expiry.
#[skuld::test]
fn stop_under_a_zero_bound_of_a_queued_start_waits_for_the_pid_signals_and_expires() {
    let run = Held::new(0);
    let pid = run.pid();
    let fake = Fake::new([
        print(QUEUED),
        print(QUEUED),
        print(&running(pid)),
        print(&running(pid)),
        print(&running(pid)),
        procinfo(pid),
        sigterm(),
    ]);

    let e = stopping(&fake, Budget::Bounded(Duration::ZERO)).expect_err("a zero bound expires");

    assert_wait_timeout(&e);
    fake.assert_spent("the pid showed and the SIGTERM stands");
    assert_eq!(calls_of(&fake, "kill").len(), 1);
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
        sigterm_ending(first),
        print(&running(b)),
        print(&running(b)),
        procinfo(b),
        sigterm_ending(second),
        print(IDLE),
    ]);
    let tree = Tree::new();
    let mgr = launchd_over(fake.clone());
    let mut handed = reserved(&mgr, Step::Stop);

    mgr.stop(task_snapshot(&tree, false), Budget::Unbounded, &mut handed)
        .expect("a stop of two runs");

    assert_eq!(bounded::test_hook::left(&handed), (0, 0));
}

/// A pass after the first that cannot reserve returns that error, and the SIGTERM it follows stands.
#[skuld::test]
fn a_later_stop_pass_that_cannot_reserve_returns_the_reservation_error() {
    let run = Held::new(0);
    let pid = run.pid();
    let fake = Fake::new([
        print(&running(pid)),
        print(&running(pid)),
        procinfo(pid),
        sigterm_ending(run),
    ]);
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
        sigterm_ending(run),
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
