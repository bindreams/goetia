//! A task's `start`, `stop` and `kill` over a scripted `launchctl` and real child processes: each child
//! stands for a run (the pid `print` shows), and the scripted `kill` ends it, so the watch that `stop`
//! waits on is a real kqueue's. Nothing here touches the system's own directories or services.

use std::os::unix::process::ExitStatusExt as _;
use std::process::ExitStatus;
use std::sync::Arc;
use std::time::Duration;

use super::super::manager_tests::launchd_over;
use super::super::task::task_tests::{ID, Tree, marked, plists_of, service_spec, task_spec};
use super::super::watch::{test_hook, watch_tests::Held};
use super::super::write::write_tests::bootstrap;
use super::*;
use crate::backend::Backend as _;
use crate::backend::bounded::{self, Capture, Finished};
use crate::backend::client::fake::{Entry, Fake, exited, reply};
use crate::backend::launchd::generate::timer_label;
use crate::spec::DaemonKind;

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

fn kickstart() -> Entry {
    reply(&["kickstart", &target()], exited(0, "", ""))
}

/// The snapshot `read` makes of an installed task, its two plists in the enabled or the staging directory.
fn task_snapshot(tree: &Tree, enabled: bool) -> Plist {
    let plists = plists_of(&task_spec(Some(SCHEDULE)));
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

/// No process and a state that is not idle, or a read that is not whole, do not establish that the job
/// stopped: `StopUnconfirmed`, saying what launchd showed. `xpcproxy` without a pid has never been seen
/// and is not taken for a start.
#[skuld::test]
fn stop_with_no_process_and_a_state_that_is_not_idle_is_unconfirmed() {
    let cut = Finished::Exited {
        status: ExitStatus::from_raw(0),
        capture: Capture {
            stdout: b"\tstate = not run".to_vec(),
            stderr: Vec::new(),
            complete: false,
        },
    };
    let failed = "reading its job from launchd failed, so whether a process remains is not established";
    let cases = [
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
    ];
    for (what, read, shown) in cases {
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
        sigterm_ending(first),
        print(&running(b)),
        print(&running(b)),
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
    let fake = Fake::new([print(&running(pid)), print(&running(pid)), sigterm()]);

    let e = stopping(&fake, Budget::Bounded(Duration::from_millis(100))).expect_err("a run that ignores SIGTERM");

    assert_wait_timeout(&e);
    fake.assert_spent("the signal was sent, and nothing after it");
}

/// `Immediate` waits for no exit: the signal is sent and the stop returns.
#[skuld::test]
fn stop_under_immediate_of_a_running_job_sends_and_returns_ok() {
    let run = Held::new(0);
    let pid = run.pid();
    let fake = Fake::new([print(&running(pid)), print(&running(pid)), sigterm()]);

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
        sigterm(),
    ]);

    stopping(&fake, Budget::Immediate).expect("a stop that waits for nothing after the signal");

    fake.assert_spent("the pid showed, then one SIGTERM, and nothing after it");
    let calls = fake.calls();
    let first_signal = calls.iter().position(|call| call[0] == "kill").unwrap();
    assert!(
        calls[..first_signal].iter().all(|call| call[0] == "print"),
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
        sigterm_ending(first),
        print(&running(b)),
        print(&running(b)),
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
    let fake = Fake::new([print(&running(pid)), print(&running(pid)), sigterm_ending(run)]);
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

/// SIGKILL by label, for a task as for a service; a job with no process is `NotRunning`.
#[skuld::test]
fn kill_of_a_task_is_the_sigkill_by_label_and_not_running_when_idle() {
    let tree = Tree::new();
    let sigkill = |code| reply(&["kill", "SIGKILL", &target()], exited(code, "", ""));
    let fake = Fake::new([sigkill(0), sigkill(3), sigkill(113)]);
    let mgr = launchd_over(fake.clone());
    let mut held = mgr.reserve(&[Step::Kill; 3], Budget::Unbounded).expect("reserve");

    mgr.kill(task_snapshot(&tree, false), &mut held).expect("a kill");
    for _ in 0..2 {
        let e = mgr
            .kill(task_snapshot(&tree, false), &mut held)
            .expect_err("an idle task has no process");
        assert!(matches!(&e, Error::NotRunning { id } if id == ID), "{e:?}");
    }

    fake.assert_spent("three kills, each by label");
}

/// `restart` is a stop and then the plain start: the start that follows a stop is the request alone.
#[skuld::test]
fn restart_of_a_task_is_stop_then_a_plain_start() {
    let tree = Tree::new();
    let snapshot = || task_snapshot(&tree, false);
    let job = snapshot().job().unwrap().location.path.clone();
    let fake = Fake::new([print(IDLE), not_loaded(), bootstrap(&job), kickstart()]);
    let mgr = launchd_over(fake.clone());
    let mut held = mgr
        .reserve(&[Step::Stop, Step::RequestStartAfterStop], Budget::DEFAULT)
        .expect("reserve");

    mgr.stop(snapshot(), Budget::DEFAULT, &mut held).expect("a stop");
    mgr.request_start_after_stop(snapshot(), &mut held).expect("a start");

    fake.assert_spent("the stop's read, then the load and the plain kick");
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
