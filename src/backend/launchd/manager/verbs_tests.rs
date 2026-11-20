//! A task's `start` and `kill` over a scripted `launchctl` and real child processes: each child stands for
//! a run (the pid `print` shows). Nothing here touches the system's own directories or services. The fixtures are
//! shared with `stop`'s tests.

use std::os::unix::process::ExitStatusExt as _;
use std::process::ExitStatus;
use std::sync::Arc;

use super::super::manager_tests::launchd_over;
use super::super::task::Shown;
use super::super::task::task_tests::{ID, Tree, marked, plists_of, service_spec, task_spec};
use super::super::watch::watch_tests::Held;
use super::super::write::write_tests::bootstrap;
use super::*;
use crate::backend::Backend as _;
use crate::backend::bounded::{self, Capture, Finished};
use crate::backend::client::fake::{Entry, Fake, exited, reply};
use crate::backend::launchd::generate::timer_label;
use crate::manager::Step;
use crate::spec::{DaemonKind, DaemonSpec};

// Fixtures ============================================================================================================

pub(in crate::backend::launchd::manager) const SCHEDULE: &str = "30 3 * * *";
/// A `print` of a task's job with no process.
pub(in crate::backend::launchd::manager) const IDLE: &str = "\tstate = not running\n";
/// A `print` of a start launchd has accepted and not yet forked.
pub(in crate::backend::launchd::manager) const QUEUED: &str = "\tstate = spawn scheduled\n";
/// A job being torn down.
const TEARING_DOWN: &str = "\tstate = SIGTERMed\n\tpid = 4242\n";

pub(in crate::backend::launchd::manager) fn target() -> String {
    format!("system/{ID}")
}

pub(in crate::backend::launchd::manager) fn running(pid: u32) -> String {
    format!("\tstate = running\n\tpid = {pid}\n")
}

pub(in crate::backend::launchd::manager) fn print(body: &str) -> Entry {
    reply(&["print", &target()], exited(0, body, ""))
}

pub(in crate::backend::launchd::manager) fn not_loaded() -> Entry {
    reply(&["print", &target()], exited(113, "", ""))
}

pub(in crate::backend::launchd::manager) fn kickstart() -> Entry {
    reply(&["kickstart", &target()], exited(0, "", ""))
}

/// The snapshot `read` makes of an installed task, its two plists in the enabled or the staging directory.
pub(in crate::backend::launchd::manager) fn task_snapshot(tree: &Tree, enabled: bool) -> Plist {
    task_snapshot_of(tree, enabled, &task_spec(Some(SCHEDULE)))
}

pub(in crate::backend::launchd::manager) fn task_snapshot_of(tree: &Tree, enabled: bool, spec: &DaemonSpec) -> Plist {
    let plists = plists_of(spec);
    tree.put(enabled, ID, &plists.job);
    tree.put(
        enabled,
        &timer_label(ID),
        plists.timer.expect("a scheduled task has a timer"),
    );
    marked(tree.read())
}

pub(in crate::backend::launchd::manager) fn reserved(mgr: &Launchd, step: Step) -> Reservation {
    mgr.reserve(&[step], Budget::Unbounded).expect("reserve")
}

pub(in crate::backend::launchd::manager) fn calls_of(fake: &Fake, verb: &str) -> Vec<Vec<String>> {
    fake.calls().into_iter().filter(|call| call[0] == verb).collect()
}

pub(in crate::backend::launchd::manager) fn assert_never_booted_out(fake: &Fake) {
    assert!(
        calls_of(fake, "bootout").is_empty(),
        "a task is never booted out by `stop`"
    );
}

/// Every `print` with no pid that is neither idle nor a start launchd has accepted, as the read and as
/// the words `StopUnconfirmed` says of it.
pub(in crate::backend::launchd::manager) fn unconfirmed_reads() -> Vec<(&'static str, Entry, &'static str)> {
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

// kill ================================================================================================================

/// A `print` of a job whose process was killed and whose restart launchd has not yet made.
const KILLED: &str = "\tstate = spawn scheduled\n";

/// `launchctl procinfo` naming `pid` as a launch of the job.
pub(in crate::backend::launchd::manager) fn procinfo(pid: u32) -> Entry {
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
