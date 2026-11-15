//! Tasks on a real launchd: two plists (the job `<id>.plist` and the timer `<id>@timer.plist`) that
//! install, arm, fire, run, stop and uninstall as one daemon, and the refusals of a schedule launchd
//! cannot hold.
//!
//! A task that has to be caught in flight is [`Held`]: it announces itself and waits on a FIFO, so a
//! test waits on the program, never on a time. A wait on the timer is a wait on an external event with
//! no cap: the suite's watchdog is its bound. Every id is guarded (`Guard`, which uninstalls and boots
//! both labels out) before it is created. A test takes `UNIT_DIR_EXCLUSIVE` where it reads the plist
//! directories as a whole (`list`, the conformance run).

use std::fs;
use std::io::{BufRead as _, BufReader};
use std::os::unix::fs::PermissionsExt as _;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, SystemTime};

use goetia::Error;
use goetia::backend::launchd::manager::Launchd;
use goetia::cli::ExitCode;
use goetia::decide::Outcome;
use goetia::manager::{Budget, Exit, Installed, ServiceManager, State};
use goetia::spec::{DaemonKind, Id};
use goetia::testing::task;

use crate::launchd::{
    Guard, UNIT_DIR_EXCLUSIVE, base_spec, enabled_path, is_loaded, print_field, print_job, staging_path,
    timer_enabled_path, timer_staging_path,
};
use crate::support::cron_oracle::assert_next_run_strict;
use crate::support::tasks::{
    Held, Lines, assert_same_text, id_of, run_task, sh, status_with_result, task_status, with_logs,
};
use crate::support::{self, Group, cmd, kill, launchd_group, may_account_for};

// Fixtures ============================================================================================================

fn sleeper() -> Vec<String> {
    ["/bin/sleep", "300"].map(String::from).to_vec()
}

/// The label of the timer of the task `id`.
fn timer_of(id: &str) -> String {
    format!("{id}@timer")
}

/// How many times launchd has started the job `label`.
fn runs_of(label: &str) -> u64 {
    let printed = print_job(label).stdout;
    print_field(&printed, "runs = ")
        .and_then(|runs| runs.parse().ok())
        .unwrap_or_else(|| panic!("`launchctl print` of {label} shows no run count:\n{printed}"))
}

/// The pid `launchctl print` shows for the job `label`, if it has a process.
fn pid_of(label: &str) -> Option<u32> {
    print_field(&print_job(label).stdout, "pid = ").and_then(|pid| pid.parse().ok())
}

/// `run` of the task `id` left no hold behind: `launchctl debug` refuses (exit 37) while one is pending,
/// and accepts now. That arms one, which the guard's boot-out discards.
#[track_caller]
fn assert_no_hold_pending(id: &str) {
    let armed = cmd::run("launchctl", &["debug", &format!("system/{id}"), "--start-suspended"]);
    assert!(armed.ok(), "a hold was left pending: {armed}");
}

/// Stops the task on drop, so a thread waiting for a held run can end when its test panics.
struct StopOnDrop(Id);

impl Drop for StopOnDrop {
    fn drop(&mut self) {
        drop(Launchd::new().stop(&self.0, Budget::Unbounded));
    }
}

/// Kills and reaps a child process on drop.
struct ChildGuard(Child);

impl Drop for ChildGuard {
    fn drop(&mut self) {
        drop(self.0.kill());
        drop(self.0.wait());
    }
}

// Installing ==========================================================================================================

/// An installed task is two plists in staging, loaded as neither, idle and not armed; `list` has it once.
#[skuld::test(labels = [UNIT_DIR_EXCLUSIVE], serial = UNIT_DIR_EXCLUSIVE)]
fn a_task_installs_disarmed_and_idle(#[fixture(launchd_group)] _group: &Group) {
    let id = support::random_test_id();
    let task_id = id_of(&id);
    let mgr = Launchd::new();
    let _guard = Guard::install(&mgr, &task(&id, sleeper(), Some("0 3 * * *")));

    assert!(staging_path(&id).is_file() && timer_staging_path(&id).is_file());
    assert!(!enabled_path(&id).exists() && !timer_enabled_path(&id).exists());
    assert!(!is_loaded(&id) && !is_loaded(&timer_of(&id)));
    let status = task_status(&mgr, &task_id);
    assert_eq!(status.common.state, State::Idle, "{status:?}");
    assert!(!status.common.enabled, "{status:?}");
    assert_eq!(status.next_run, None, "{status:?}");
    assert_eq!(status.last_result, None, "{status:?}");

    let listed = mgr.list().expect("list");
    let accounting: Vec<&Installed> = listed
        .iter()
        .filter(|entry| may_account_for(std::slice::from_ref(*entry), &id))
        .collect();
    assert!(
        matches!(accounting.as_slice(), [Installed::Ours { .. }]),
        "the task is listed once, and as ours: {accounting:?}"
    );
}

/// `install` over a task with a changed command, schedule or both, while its job is loaded and idle (it
/// has run): it goes through, the task stays armed, and a later `run` is the new command's.
#[skuld::test]
fn editing_a_task_after_it_ran_works(#[fixture(launchd_group)] _group: &Group) {
    for (what, version, schedule) in [
        ("command", 2, "0 3 * * *"),
        ("schedule", 1, "45 4 * * *"),
        ("command and schedule", 2, "45 4 * * *"),
    ] {
        let id = support::random_test_id();
        let task_id = id_of(&id);
        let dir = tempfile::tempdir().expect("tempdir");
        let log = dir.path().join("task.log");
        let versioned = |version: u32, schedule: &str| {
            with_logs(task(&id, sh(&format!("echo version-{version}")), Some(schedule)), &log)
        };
        let mgr = Launchd::new();
        let _guard = Guard::install(&mgr, &versioned(1, "0 3 * * *"));
        mgr.enable(&task_id).expect("enable");
        let (exit, out) = run_task(&mgr, &task_id);
        assert_eq!((exit, out.as_slice()), (Exit::Code(0), b"version-1\n".as_slice()));
        assert!(is_loaded(&id), "{what}: the job is loaded after its run");

        let before = SystemTime::now();
        let outcome = mgr
            .install(&versioned(version, schedule), false)
            .unwrap_or_else(|e| panic!("{what}: install: {}", e.report()));
        let status = task_status(&mgr, &task_id);
        let after = SystemTime::now();

        assert!(matches!(outcome, Outcome::Update { .. }), "{what}: {outcome:?}");
        assert!(status.common.enabled, "{what}: still armed: {status:?}");
        assert_next_run_strict(&status, schedule, &task_id, before, after);
        assert!(
            is_loaded(&id) && is_loaded(&timer_of(&id)),
            "{what}: both are loaded again"
        );
        assert!(
            matches!(mgr.install(&versioned(version, schedule), false), Ok(Outcome::UpToDate)),
            "{what}: the plists on disk are the new spec's"
        );
        let (exit, out) = run_task(&mgr, &task_id);
        assert_eq!(exit, Exit::Code(0), "{what}");
        assert_eq!(String::from_utf8_lossy(&out), format!("version-{version}\n"), "{what}");
        assert_no_hold_pending(&id);
    }
}

/// launchd cannot replace a plist under a run in flight, so an edit that changes the job is refused as a
/// loaded job of kind task, and the run goes on.
#[skuld::test]
fn an_edit_over_a_running_task_is_loaded_job_of_kind_task(#[fixture(launchd_group)] _group: &Group) {
    let id = support::random_test_id();
    let task_id = id_of(&id);
    let mut held = Held::new();
    let mgr = Launchd::new();
    let _guard = Guard::install(&mgr, &task(&id, held.command.clone(), None));
    mgr.start(&task_id, Budget::DEFAULT).expect("start");
    held.wait_started();
    let pid = task_status(&mgr, &task_id).common.pid;

    let e = mgr
        .install(&task(&id, sh("exit 0"), None), false)
        .expect_err("an edit over a run in flight is refused");

    assert!(
        matches!(
            e.cause(),
            Error::LoadedJob {
                kind: DaemonKind::Task,
                ..
            }
        ),
        "{e:?}"
    );
    assert!(e.cause().to_string().contains("has a run in flight"), "{e}");
    let status = task_status(&mgr, &task_id);
    assert_eq!((status.common.state, status.common.pid), (State::Running, pid));
    held.release();
    assert_eq!(status_with_result(&mgr, &task_id).last_result, Some(Exit::Code(0)));
}

/// A service made a task and a task made a service leaves no job in `LaunchDaemons` and no timer
/// loaded: a retype drops arming and boot enrolment in both directions.
#[skuld::test]
fn retyping_a_service_to_a_task_and_back(#[fixture(launchd_group)] _group: &Group) {
    let id = support::random_test_id();
    let task_id = id_of(&id);
    let mgr = Launchd::new();
    let service = base_spec(&id, sleeper());
    let _guard = Guard::install(&mgr, &service);
    mgr.enable(&task_id).expect("enable the service");
    assert!(enabled_path(&id).is_file());

    let outcome = mgr
        .install(&task(&id, sleeper(), Some("0 3 * * *")), false)
        .expect("retype to a task");

    assert!(matches!(outcome, Outcome::Update { .. }), "{outcome:?}");
    assert!(!enabled_path(&id).exists(), "no job is left in LaunchDaemons");
    assert!(staging_path(&id).is_file() && timer_staging_path(&id).is_file());
    assert!(!is_loaded(&id) && !is_loaded(&timer_of(&id)));
    let status = task_status(&mgr, &task_id);
    assert!(!status.common.enabled, "{status:?}");

    mgr.enable(&task_id).expect("enable the task");
    assert!(is_loaded(&id) && is_loaded(&timer_of(&id)), "the task is armed");

    let outcome = mgr.install(&service, false).expect("retype to a service");

    assert!(matches!(outcome, Outcome::Update { .. }), "{outcome:?}");
    assert!(!enabled_path(&id).exists(), "no job is left in LaunchDaemons");
    assert!(!timer_enabled_path(&id).exists() && !timer_staging_path(&id).exists());
    assert!(staging_path(&id).is_file());
    assert!(!is_loaded(&timer_of(&id)), "no timer is loaded");
    assert!(!is_loaded(&id), "no job is loaded");
    let status = mgr.status(&task_id).expect("status");
    assert!(!status.common().enabled, "{status:?}");
}

/// A task with its schedule dropped has no timer, on disk or in launchd; its job is not unloaded, but is
/// no longer enabled.
#[skuld::test]
fn dropping_the_schedule_unloads_and_removes_the_timer(#[fixture(launchd_group)] _group: &Group) {
    let id = support::random_test_id();
    let task_id = id_of(&id);
    let mgr = Launchd::new();
    let _guard = Guard::install(&mgr, &task(&id, sleeper(), Some("0 3 * * *")));
    mgr.enable(&task_id).expect("enable");
    assert!(is_loaded(&timer_of(&id)), "the timer is armed");

    let outcome = mgr
        .install(&task(&id, sleeper(), None), false)
        .expect("install without a schedule");

    assert!(matches!(outcome, Outcome::Update { .. }), "{outcome:?}");
    assert!(!timer_enabled_path(&id).exists() && !timer_staging_path(&id).exists());
    assert!(!is_loaded(&timer_of(&id)), "the timer is not loaded");
    assert!(is_loaded(&id), "the job is not unloaded");
    assert!(
        staging_path(&id).is_file() && !enabled_path(&id).exists(),
        "nothing is left to arm"
    );
    let status = task_status(&mgr, &task_id);
    assert_eq!(status.next_run, None, "{status:?}");
    assert!(!status.common.enabled, "{status:?}");
}

// Arming and the timer ================================================================================================

/// `enable` moves both plists to `LaunchDaemons` and loads both, and `next_run` is the next minute the
/// schedule names (the oracle's, in the machine's zone); `disable` moves both back, unloads the timer and
/// leaves the job loaded. Each shape of schedule: both day fields (OR), steps, Sunday as 0 and 7, lists,
/// a day some months lack, a whole hour of minutes, a month range of every day.
#[skuld::test]
fn enable_arms_and_disable_disarms(#[fixture(launchd_group)] _group: &Group) {
    let mgr = Launchd::new();
    for text in [
        "*/7 * * * *",
        "0 3 * * *",
        "30 4 * * 0",
        "30 4 * * 7",
        "15 2,14,22 * * *",
        "0 0 13 * 5",
        "0 0 1,15 * *",
        "0 0 31 * *",
        "*/20 */6 * * 1-5",
        "* 12 * * *",
        "0 3 * 3-5 *",
    ] {
        let id = support::random_test_id();
        let task_id = id_of(&id);
        let _guard = Guard::install(&mgr, &task(&id, sleeper(), Some(text)));

        let before = SystemTime::now();
        mgr.enable(&task_id)
            .unwrap_or_else(|e| panic!("`{text}`: enable: {}", e.report()));
        let status = task_status(&mgr, &task_id);
        let after = SystemTime::now();

        assert!(
            enabled_path(&id).is_file() && timer_enabled_path(&id).is_file(),
            "`{text}`"
        );
        assert!(
            !staging_path(&id).exists() && !timer_staging_path(&id).exists(),
            "`{text}`"
        );
        assert!(is_loaded(&id) && is_loaded(&timer_of(&id)), "`{text}`: both are loaded");
        assert!(status.common.enabled, "`{text}`: {status:?}");
        assert_next_run_strict(&status, text, &task_id, before, after);

        mgr.disable(&task_id).expect("disable");

        assert!(
            staging_path(&id).is_file() && timer_staging_path(&id).is_file(),
            "`{text}`"
        );
        assert!(
            !enabled_path(&id).exists() && !timer_enabled_path(&id).exists(),
            "`{text}`"
        );
        assert!(!is_loaded(&timer_of(&id)), "`{text}`: the timer is unloaded");
        assert!(is_loaded(&id), "`{text}`: the job is still loaded");
        let status = task_status(&mgr, &task_id);
        assert!(!status.common.enabled, "`{text}`: {status:?}");
        assert_eq!(status.next_run, None, "`{text}`: {status:?}");
    }
}

/// `disable` stops the schedule, not the run in flight.
#[skuld::test]
fn disable_leaves_a_run_in_flight_running(#[fixture(launchd_group)] _group: &Group) {
    let id = support::random_test_id();
    let task_id = id_of(&id);
    let mut held = Held::new();
    let mgr = Launchd::new();
    let _guard = Guard::install(&mgr, &task(&id, held.command.clone(), Some("0 3 * * *")));
    mgr.enable(&task_id).expect("enable");
    mgr.start(&task_id, Budget::DEFAULT).expect("start");
    held.wait_started();

    mgr.disable(&task_id).expect("disable");

    let status = task_status(&mgr, &task_id);
    assert_eq!(status.common.state, State::Running, "{status:?}");
    assert!(!status.common.enabled, "{status:?}");
    assert!(!is_loaded(&timer_of(&id)));
    held.release();
    assert_eq!(status_with_result(&mgr, &task_id).last_result, Some(Exit::Code(0)));
}

/// The timer starts the task by itself: the test starts nothing, and blocks on the program's own
/// announcement.
#[skuld::test]
fn the_timer_fires_by_itself(#[fixture(launchd_group)] _group: &Group) {
    let id = support::random_test_id();
    let task_id = id_of(&id);
    let mut held = Held::new();
    let mgr = Launchd::new();
    let _guard = Guard::install(&mgr, &task(&id, held.command.clone(), Some("* * * * *")));
    mgr.enable(&task_id).expect("enable");

    held.wait_started();

    let running = task_status(&mgr, &task_id);
    assert_eq!(running.common.state, State::Running, "{running:?}");
    held.release();
    let finished = status_with_result(&mgr, &task_id);
    assert_eq!(finished.last_result, Some(Exit::Code(0)), "{finished:?}");
    assert!(finished.common.enabled, "the schedule stays armed: {finished:?}");
}

/// A fire while a run is in flight starts nothing: the timer's `kickstart` finds the job running. The
/// test waits for a fire (the timer's run count) and for the timer's own run to be over.
#[skuld::test]
fn a_fire_during_a_run_starts_nothing(#[fixture(launchd_group)] _group: &Group) {
    let id = support::random_test_id();
    let task_id = id_of(&id);
    let timer = timer_of(&id);
    let mut held = Held::new();
    let mgr = Launchd::new();
    let _guard = Guard::install(&mgr, &task(&id, held.command.clone(), Some("* * * * *")));
    mgr.enable(&task_id).expect("enable");
    mgr.start(&task_id, Budget::DEFAULT).expect("start");
    held.wait_started();
    let pid = task_status(&mgr, &task_id).common.pid;
    let fires = runs_of(&timer);

    kill::until(|| {
        let printed = print_job(&timer).stdout;
        runs_of(&timer) > fires
            && print_field(&printed, "pid = ").is_none()
            && print_field(&printed, "last exit code = ").is_some_and(|code| !code.contains("never exited"))
    });

    let status = task_status(&mgr, &task_id);
    assert_eq!(
        (status.common.state, status.common.pid),
        (State::Running, pid),
        "{status:?}"
    );
    assert_eq!(runs_of(&id), 1, "the fire started no second run");
    held.release();
    assert_eq!(status_with_result(&mgr, &task_id).last_result, Some(Exit::Code(0)));
}

/// `stop` ends the run in flight and leaves the schedule armed: the next fire starts a fresh run.
#[skuld::test]
fn the_timer_fire_after_a_stop_starts_a_fresh_run(#[fixture(launchd_group)] _group: &Group) {
    let id = support::random_test_id();
    let task_id = id_of(&id);
    let mut held = Held::new();
    let mgr = Launchd::new();
    let _guard = Guard::install(&mgr, &task(&id, held.command.clone(), Some("* * * * *")));
    mgr.enable(&task_id).expect("enable");
    mgr.start(&task_id, Budget::DEFAULT).expect("start");
    held.wait_started();
    mgr.stop(&task_id, Budget::DEFAULT).expect("stop");

    // The stop's outcome is the run's result, not the task's state: a fire may start a run the moment
    // after `stop` returns. `stop` returned with no run left, so every run numbered up to now is dead
    // (one that `stop` ended too, because a fire began it meanwhile, has an announcement waiting) and
    // a run numbered after is a fire's, begun after the stop.
    let stopped = task_status(&mgr, &task_id);
    assert_eq!(stopped.last_result, Some(Exit::Signal(15)), "{stopped:?}");
    assert!(stopped.common.enabled, "stop leaves the schedule armed: {stopped:?}");
    let before = held.begun();
    while held.wait_started() <= before {}

    held.release();
}

// start, stop, restart, uninstall =====================================================================================

#[skuld::test]
fn start_returns_before_the_run_ends(#[fixture(launchd_group)] _group: &Group) {
    let id = support::random_test_id();
    let task_id = id_of(&id);
    let mut held = Held::new();
    let mgr = Launchd::new();
    let _guard = Guard::install(&mgr, &task(&id, held.command.clone(), None));

    mgr.start(&task_id, Budget::DEFAULT).expect("start");

    let status = task_status(&mgr, &task_id);
    assert_eq!(status.common.state, State::Running, "{status:?}");
    held.wait_started();
    held.release();
    assert_eq!(status_with_result(&mgr, &task_id).last_result, Some(Exit::Code(0)));
}

#[skuld::test]
fn start_of_a_running_task_starts_nothing_new(#[fixture(launchd_group)] _group: &Group) {
    let id = support::random_test_id();
    let task_id = id_of(&id);
    let mut held = Held::new();
    let mgr = Launchd::new();
    let _guard = Guard::install(&mgr, &task(&id, held.command.clone(), None));
    mgr.start(&task_id, Budget::DEFAULT).expect("start");
    held.wait_started();
    let pid = task_status(&mgr, &task_id).common.pid;

    mgr.start(&task_id, Budget::DEFAULT).expect("start again");

    assert_eq!(runs_of(&id), 1, "a second run began");
    let status = task_status(&mgr, &task_id);
    assert_eq!((status.common.state, status.common.pid), (State::Running, pid));
    held.release();
}

/// `start` loads the job from where its plist is, without enabling it.
#[skuld::test]
fn start_of_a_staged_task_loads_it_from_staging(#[fixture(launchd_group)] _group: &Group) {
    let id = support::random_test_id();
    let task_id = id_of(&id);
    let mut held = Held::new();
    let mgr = Launchd::new();
    let _guard = Guard::install(&mgr, &task(&id, held.command.clone(), Some("0 3 * * *")));
    assert!(!is_loaded(&id), "the job is not loaded before the start");

    mgr.start(&task_id, Budget::DEFAULT).expect("start");

    held.wait_started();
    assert!(is_loaded(&id));
    assert!(!is_loaded(&timer_of(&id)), "start does not arm the schedule");
    assert!(staging_path(&id).is_file() && !enabled_path(&id).exists());
    held.release();
}

/// `launchctl kickstart` returns while the start is only queued (`spawn scheduled`): a `run` straight
/// after a `start` finds a task in flight, not an idle one and not one whose state is unknown.
#[skuld::test]
fn run_straight_after_start_answers_already_running(#[fixture(launchd_group)] _group: &Group) {
    let id = support::random_test_id();
    let task_id = id_of(&id);
    let mut held = Held::new();
    let mgr = Launchd::new();
    let _guard = Guard::install(&mgr, &task(&id, held.command.clone(), None));
    mgr.start(&task_id, Budget::DEFAULT).expect("start");
    let mut out = Vec::new();

    let e = mgr
        .run(&task_id, &mut out)
        .expect_err("a task that was just started is in flight");

    assert!(matches!(e, Error::AlreadyRunning { .. }), "{e:?}");
    assert!(out.is_empty());
    held.wait_started();
    held.release();
    assert_eq!(status_with_result(&mgr, &task_id).last_result, Some(Exit::Code(0)));
    assert_eq!(runs_of(&id), 1, "the refused run started nothing");
    assert_no_hold_pending(&id);
}

/// A `stop` of a task whose start is queued waits for the run to begin and ends it: the run ended by
/// SIGTERM is the job's last result. (The shared conformance test's `start` then `stop` is this case.)
#[skuld::test]
fn stop_straight_after_start_ends_the_run(#[fixture(launchd_group)] _group: &Group) {
    let id = support::random_test_id();
    let task_id = id_of(&id);
    let held = Held::new();
    let mgr = Launchd::new();
    let _guard = Guard::install(&mgr, &task(&id, held.command.clone(), None));
    mgr.start(&task_id, Budget::DEFAULT).expect("start");

    mgr.stop(&task_id, Budget::DEFAULT).expect("stop");

    let status = task_status(&mgr, &task_id);
    assert_eq!(status.common.state, State::Idle, "{status:?}");
    assert_eq!(
        status.last_result,
        Some(Exit::Signal(15)),
        "a run began and the stop ended it: {status:?}"
    );
}

/// Under no budget a `stop` still waits for a queued start to begin, signals the run and returns `Ok`
/// without waiting for it to end: the run does end.
#[skuld::test]
fn stop_with_no_budget_straight_after_start_ends_the_run(#[fixture(launchd_group)] _group: &Group) {
    let id = support::random_test_id();
    let task_id = id_of(&id);
    let held = Held::new();
    let mgr = Launchd::new();
    let _guard = Guard::install(&mgr, &task(&id, held.command.clone(), None));
    mgr.start(&task_id, Budget::DEFAULT).expect("start");

    mgr.stop(&task_id, Budget::Immediate).expect("stop");

    kill::until(|| task_status(&mgr, &task_id).common.state == State::Idle);
    let status = task_status(&mgr, &task_id);
    assert_eq!(status.last_result, Some(Exit::Signal(15)), "{status:?}");
}

#[skuld::test]
fn stop_ends_the_run_and_reports_sigterm_in_last_result(#[fixture(launchd_group)] _group: &Group) {
    let id = support::random_test_id();
    let task_id = id_of(&id);
    let mut held = Held::new();
    let mgr = Launchd::new();
    let _guard = Guard::install(&mgr, &task(&id, held.command.clone(), Some("0 3 * * *")));
    mgr.enable(&task_id).expect("enable");
    mgr.start(&task_id, Budget::DEFAULT).expect("start");
    held.wait_started();

    mgr.stop(&task_id, Budget::DEFAULT).expect("stop");

    let status = task_status(&mgr, &task_id);
    assert_eq!(status.common.state, State::Idle, "{status:?}");
    assert_eq!(status.last_result, Some(Exit::Signal(15)), "{status:?}");
    assert!(status.common.enabled, "stop leaves the schedule armed: {status:?}");
    assert!(is_loaded(&id), "a task is never booted out by stop");
    mgr.stop(&task_id, Budget::DEFAULT).expect("stop of an idle task is Ok");
}

/// A run that ignores SIGTERM outlives a bounded `stop`, which says so; `kill` ends it.
#[skuld::test]
fn stop_of_a_task_that_ignores_sigterm_times_out_and_kill_ends_it(#[fixture(launchd_group)] _group: &Group) {
    let id = support::random_test_id();
    let task_id = id_of(&id);
    let mut held = Held::ignoring_sigterm();
    let mgr = Launchd::new();
    let _guard = Guard::install(&mgr, &task(&id, held.command.clone(), None));
    mgr.start(&task_id, Budget::DEFAULT).expect("start");
    held.wait_started();

    let e = mgr
        .stop(&task_id, Budget::Bounded(Duration::from_millis(500)))
        .expect_err("a run that ignores SIGTERM outlives the budget");

    assert!(
        matches!(e.cause(), Error::WaitTimeout { awaited: "stopped", .. }),
        "{e:?}"
    );
    let status = task_status(&mgr, &task_id);
    assert_eq!(status.common.state, State::Running, "{status:?}");
    mgr.kill(&task_id).expect("kill");
    kill::until(|| task_status(&mgr, &task_id).common.state == State::Idle);
    let status = task_status(&mgr, &task_id);
    assert_eq!(status.last_result, Some(Exit::Signal(9)), "{status:?}");
}

/// `restart` of a task in flight ends that run (SIGTERM, waited for) and starts a new one. Not the
/// unwaited form (`--timeout 0`): its stop does not wait for the run to end, and a start requested while
/// the run is still ending finds the job running, which `kickstart` leaves alone; whether that is lost
/// is not measured, and no test can place the start after the run's end.
#[skuld::test]
fn restart_of_a_running_task_ends_it_and_starts_a_new_run(#[fixture(launchd_group)] _group: &Group) {
    let id = support::random_test_id();
    let task_id = id_of(&id);
    let mut held = Held::new();
    let mgr = Launchd::new();
    let _guard = Guard::install(&mgr, &task(&id, held.command.clone(), None));
    mgr.start(&task_id, Budget::DEFAULT).expect("start");
    held.wait_started();
    let first = task_status(&mgr, &task_id)
        .common
        .pid
        .expect("a running task has a pid");
    let args = goetia::cli::restart::Args {
        ids: vec![id.clone()],
        wait: goetia::cli::wait::WaitArgs {
            timeout: None,
            no_timeout: false,
        },
    };
    let get_manager = || -> goetia::Result<Box<dyn ServiceManager>> { Ok(Box::new(Launchd::new())) };
    let (mut out, mut err) = (Vec::new(), Vec::new());

    // Truthful, not a stub: the group's elevation check is this test's own precondition.
    let code = goetia::cli::restart::run(&args, &get_manager, &|| true, &mut out, &mut err);

    assert_eq!(
        code,
        ExitCode::Success,
        "stdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&out),
        String::from_utf8_lossy(&err)
    );
    held.wait_started();
    let status = task_status(&mgr, &task_id);
    assert_eq!(status.common.state, State::Running, "{status:?}");
    assert_ne!(
        status.common.pid,
        Some(first),
        "the first run was ended and a new one began"
    );
    held.release();
    assert_eq!(status_with_result(&mgr, &task_id).last_result, Some(Exit::Code(0)));
}

/// `uninstall` of both plists: no file in either directory, neither label loaded.
#[skuld::test]
fn uninstall_removes_both_plists_and_boots_both_out(#[fixture(launchd_group)] _group: &Group) {
    let id = support::random_test_id();
    let task_id = id_of(&id);
    let mgr = Launchd::new();
    let _guard = Guard::install(&mgr, &task(&id, sleeper(), Some("0 3 * * *")));
    mgr.enable(&task_id).expect("enable");
    assert!(is_loaded(&id) && is_loaded(&timer_of(&id)));

    mgr.uninstall(&task_id).expect("uninstall");

    for path in [
        staging_path(&id),
        timer_staging_path(&id),
        enabled_path(&id),
        timer_enabled_path(&id),
    ] {
        assert!(!path.exists(), "{} is left", path.display());
    }
    assert!(!is_loaded(&id) && !is_loaded(&timer_of(&id)));
    let e = mgr.status(&task_id).expect_err("nothing is installed");
    assert!(matches!(e, Error::NotInstalled { .. }), "{e:?}");
}

/// `uninstall` of an armed task with a run in flight ends the run first.
#[skuld::test]
fn uninstall_of_a_running_armed_task_ends_it(#[fixture(launchd_group)] _group: &Group) {
    let id = support::random_test_id();
    let task_id = id_of(&id);
    let mut held = Held::new();
    let mgr = Launchd::new();
    let _guard = Guard::install(&mgr, &task(&id, held.command.clone(), Some("0 3 * * *")));
    mgr.enable(&task_id).expect("enable");
    mgr.start(&task_id, Budget::DEFAULT).expect("start");
    held.wait_started();
    let pid = task_status(&mgr, &task_id)
        .common
        .pid
        .expect("a running task has a pid");

    mgr.uninstall(&task_id).expect("uninstall");

    // SAFETY: signal 0 only checks that the process exists.
    let alive = unsafe { libc::kill(libc::pid_t::try_from(pid).expect("a pid"), 0) } == 0;
    assert!(!alive, "the run's process {pid} is gone");
    assert!(!is_loaded(&id) && !is_loaded(&timer_of(&id)));
    assert!(!enabled_path(&id).exists() && !timer_enabled_path(&id).exists());
}

// run =================================================================================================================

/// What a run shows and how it ends, through the library and through `goetia daemon run`: stdout and
/// stderr lines in order from the `logs` file, never the marker, and the program's own exit code.
#[skuld::test]
fn run_streams_the_logs_file_and_returns_the_exit_code(#[fixture(launchd_group)] _group: &Group) {
    let id = support::random_test_id();
    let task_id = id_of(&id);
    let dir = tempfile::tempdir().expect("tempdir");
    let log = dir.path().join("task.log");
    let mgr = Launchd::new();
    let _guard = Guard::install(
        &mgr,
        &with_logs(task(&id, sh("echo out-line; echo err-line >&2; exit 3"), None), &log),
    );

    let (exit, out) = run_task(&mgr, &task_id);

    assert_eq!(exit, Exit::Code(3));
    assert_eq!(String::from_utf8_lossy(&out), "out-line\nerr-line\n");
    let status = task_status(&mgr, &task_id);
    assert_eq!(status.common.state, State::Idle, "{status:?}");
    assert_eq!(status.last_result, Some(Exit::Code(3)), "{status:?}");
    let file = fs::read_to_string(&log).expect("read the log");
    assert!(file.contains("goetia-run "), "the file holds the run's marker: {file}");

    let args = goetia::cli::run::Args { id: id.clone() };
    let get_manager = || -> goetia::Result<Box<dyn ServiceManager>> { Ok(Box::new(Launchd::new())) };
    let (mut out, mut err) = (Vec::new(), Vec::new());
    // Truthful, not a stub: the group's elevation check is this test's own precondition.
    let code = goetia::cli::run::run(&args, &get_manager, &|| true, &mut out, &mut err);
    assert_eq!(code, ExitCode::Run(3), "stderr:\n{}", String::from_utf8_lossy(&err));
    assert_eq!(String::from_utf8_lossy(&out), "out-line\nerr-line\n");
    assert_no_hold_pending(&id);
}

/// A task with no `logs` has no output launchd keeps: one note says so, and `run` still returns the exit.
#[skuld::test]
fn run_of_a_task_without_logs_notes_why_and_returns_the_exit(#[fixture(launchd_group)] _group: &Group) {
    let id = support::random_test_id();
    let task_id = id_of(&id);
    let mgr = Launchd::new();
    let _guard = Guard::install(&mgr, &task(&id, sh("echo not-shown; exit 4"), None));

    let (exit, out) = run_task(&mgr, &task_id);

    assert_eq!(exit, Exit::Code(4));
    assert_eq!(
        String::from_utf8_lossy(&out),
        format!("note: `{id}` has no `logs` file, so its output is not shown; waiting for the run to end\n")
    );
    assert_no_hold_pending(&id);
}

/// A run ended by a signal returns it. `stop` ends this one while the test reads its output.
#[skuld::test]
fn run_of_a_signalled_task_returns_signal_15(#[fixture(launchd_group)] _group: &Group) {
    let id = support::random_test_id();
    let task_id = id_of(&id);
    let dir = tempfile::tempdir().expect("tempdir");
    let log = dir.path().join("task.log");
    let held = Held::new();
    let mgr = Launchd::new();
    let _guard = Guard::install(&mgr, &with_logs(task(&id, held.command.clone(), None), &log));
    let (lines, shown) = mpsc::channel();

    let exit = std::thread::scope(|scope| {
        let _stop = StopOnDrop(task_id.clone());
        let (mgr, task_id) = (&mgr, &task_id);
        let run = scope.spawn(move || mgr.run(task_id, &mut Lines(lines)));
        let mut seen = Vec::new();
        while !seen.windows(6).any(|window| window == b"ready\n") {
            seen.extend(shown.recv().expect("the run ended before the task said ready"));
        }
        mgr.stop(task_id, Budget::DEFAULT).expect("stop");
        run.join().expect("the run's thread")
    });

    assert_eq!(exit.expect("run"), Exit::Signal(15));
    assert_eq!(task_status(&mgr, &task_id).common.state, State::Idle);
    assert_no_hold_pending(&id);
}

/// `run` of a task in flight is refused and starts nothing: the run in flight is the only one.
#[skuld::test]
fn run_refuses_a_run_in_flight(#[fixture(launchd_group)] _group: &Group) {
    let id = support::random_test_id();
    let task_id = id_of(&id);
    let mut held = Held::new();
    let mgr = Launchd::new();
    let _guard = Guard::install(&mgr, &task(&id, held.command.clone(), None));
    mgr.start(&task_id, Budget::DEFAULT).expect("start");
    held.wait_started();
    let pid = task_status(&mgr, &task_id).common.pid;
    let mut out = Vec::new();

    let e = mgr.run(&task_id, &mut out).expect_err("a task in flight is not run");

    assert!(matches!(e, Error::AlreadyRunning { .. }), "{e:?}");
    assert!(out.is_empty());
    assert_eq!(runs_of(&id), 1, "a second run began");
    assert_eq!(task_status(&mgr, &task_id).common.pid, pid);
    held.release();
    assert_eq!(status_with_result(&mgr, &task_id).last_result, Some(Exit::Code(0)));
    assert_no_hold_pending(&id);
}

/// A run starts right behind another that has just returned and shows none of its lines: run 1 prints
/// one line and exits, run 2 prints another, and neither shows the other's, twenty times.
#[skuld::test]
fn a_second_run_shows_none_of_the_first_runs_lines(#[fixture(launchd_group)] _group: &Group) {
    let id = support::random_test_id();
    let task_id = id_of(&id);
    let dir = tempfile::tempdir().expect("tempdir");
    let (flag, log) = (dir.path().join("flag"), dir.path().join("task.log"));
    let script = format!(
        "if [ -e \"{flag}\" ]; then rm \"{flag}\"; echo second-run; else : > \"{flag}\"; echo first-run; fi",
        flag = flag.display()
    );
    let mgr = Launchd::new();
    let _guard = Guard::install(&mgr, &with_logs(task(&id, sh(&script), None), &log));

    for n in 1..=20 {
        let (exit, out) = run_task(&mgr, &task_id);
        assert_eq!(exit, Exit::Code(0));
        assert_same_text(n, &out, b"first-run\n");

        let (exit, out) = run_task(&mgr, &task_id);
        assert_eq!(exit, Exit::Code(0));
        assert_same_text(n, &out, b"second-run\n");
    }
    assert_no_hold_pending(&id);
}

/// Panics unless `out` is one run's two lines, `begin <n>` and `end <n>` for the same `n`, after the
/// note that a scheduled run was waited for, if it has one.
#[track_caller]
fn assert_one_runs_lines(shown: usize, out: &[u8], note: &str) {
    let text = String::from_utf8_lossy(out).into_owned();
    let body = text.strip_prefix(&format!("{note}\n")).unwrap_or(text.as_str());
    let lines: Vec<&str> = body.lines().collect();
    let [begin, end] = lines.as_slice() else {
        panic!("run {shown}: not the two lines of one run: {text:?}");
    };
    assert!(
        matches!(
            (begin.strip_prefix("begin "), end.strip_prefix("end ")),
            (Some(a), Some(b)) if a == b
        ),
        "run {shown}: the lines are not one run's: {text:?}"
    );
}

/// `run`s back to back while the timer fires every minute: a scheduled run that begins right after
/// ours ends appends to the same file while `run` reads it, and none of its lines is shown. The loop
/// ends once the timer has fired twice, so a minute boundary is inside it. A `run` that meets a scheduled
/// run in flight is refused, and tries again; one that meets it as it starts waits for it, and says so.
#[skuld::test]
fn a_scheduled_run_beginning_right_after_ours_is_not_shown(#[fixture(launchd_group)] _group: &Group) {
    let id = support::random_test_id();
    let task_id = id_of(&id);
    let timer = timer_of(&id);
    let dir = tempfile::tempdir().expect("tempdir");
    let (counter, log) = (dir.path().join("count"), dir.path().join("task.log"));
    let script = format!(
        "n=$(cat \"{c}\" 2>/dev/null || echo 0); n=$((n + 1)); echo $n > \"{c}\"; echo \"begin $n\"; echo \"end $n\"",
        c = counter.display()
    );
    let mgr = Launchd::new();
    let _guard = Guard::install(&mgr, &with_logs(task(&id, sh(&script), Some("* * * * *")), &log));
    mgr.enable(&task_id).expect("enable");
    let note =
        format!("note: a scheduled run of `{id}` is in progress; waiting for it to end before starting this run");
    let fires = runs_of(&timer);
    let mut shown = 0;

    while runs_of(&timer) < fires + 2 {
        let mut out = Vec::new();
        match mgr.run(&task_id, &mut out) {
            Ok(exit) => {
                assert_eq!(exit, Exit::Code(0));
                shown += 1;
                assert_one_runs_lines(shown, &out, &note);
            }
            Err(Error::AlreadyRunning { .. }) => {}
            Err(e) => panic!("run: {}", e.report()),
        }
    }
    assert_no_hold_pending(&id);
}

/// A scheduled run begins between `run`'s read of the task's state and its hold on the next launch, so
/// that `kickstart -p` names the run in flight. That window is a few milliseconds, so the test opens it:
/// `goetia daemon run` is a child process whose `launchctl` is first on its `PATH` a script that, at the
/// first `launchctl debug` of the run, starts the job as the timer does (`launchctl kickstart`) and waits
/// for it to be in flight. The slipped-in run is a held one: the child says it waits for it, the test
/// releases it, and `run` then runs the task itself, shows only its own output, and holds nothing back
/// for the next launch.
#[skuld::test]
fn a_scheduled_run_in_flight_is_waited_for_by_run(#[fixture(launchd_group)] _group: &Group) {
    let id = support::random_test_id();
    let task_id = id_of(&id);
    let dir = tempfile::tempdir().expect("tempdir");
    let log = dir.path().join("task.log");
    let bin = tempfile::tempdir().expect("tempdir");
    let mut held = Held::new();
    let mgr = Launchd::new();
    let _guard = Guard::install(
        &mgr,
        &with_logs(task(&id, held.command.clone(), Some("0 3 * * *")), &log),
    );
    mgr.enable(&task_id).expect("enable");
    let wrapper = bin.path().join("launchctl");
    let slipped = bin.path().join("slipped");
    fs::write(
        &wrapper,
        format!(
            "#!/bin/sh\n\
             if [ \"$1\" = debug ] && [ ! -e \"{slipped}\" ]; then\n\
             : > \"{slipped}\"\n\
             /bin/launchctl kickstart \"system/{id}\" || exit 1\n\
             until /bin/launchctl print \"system/{id}\" | grep -Eq '^[[:space:]]*pid = [0-9]+'; do :; done\n\
             fi\n\
             exec /bin/launchctl \"$@\"\n",
            slipped = slipped.display()
        ),
    )
    .expect("write the launchctl wrapper");
    fs::set_permissions(&wrapper, fs::Permissions::from_mode(0o755)).expect("chmod the wrapper");
    let path = format!(
        "{}:{}",
        bin.path().display(),
        std::env::var("PATH").unwrap_or_else(|_| "/usr/bin:/bin:/usr/sbin:/sbin".to_string())
    );
    let mut child = ChildGuard(
        Command::new(env!("CARGO_BIN_EXE_goetia"))
            .args(["daemon", "run", id.as_str()])
            .env("PATH", path)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .spawn()
            .expect("spawn goetia daemon run"),
    );
    let stdout = child.0.stdout.take().expect("goetia's stdout");
    let (lines, shown) = mpsc::channel::<String>();
    std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            if lines.send(line).is_err() {
                break;
            }
        }
    });

    let note = shown.recv().expect("goetia ended before it said it waits");
    assert_eq!(
        note,
        format!("note: a scheduled run of `{id}` is in progress; waiting for it to end before starting this run")
    );
    // The slipped-in run is in flight and `run` waits for it: it has not started its own.
    held.wait_started();
    assert_eq!(runs_of(&id), 1, "run waits for the run in flight");
    held.release();
    // Its own run, resumed from the hold: it announces itself and waits to be released.
    held.wait_started();
    held.release();

    let status = child.0.wait().expect("wait for goetia");
    assert_eq!(status.code(), Some(0), "{status:?}");
    let rest: Vec<String> = shown.iter().collect();
    assert_eq!(rest, ["ready"], "only its own run's output is shown");
    assert_eq!(runs_of(&id), 2);

    // The next fire is not held: a start straight away is an ordinary one.
    let started = cmd::run("launchctl", &["kickstart", "-p", &format!("system/{id}")]).expect_ok();
    let pid: u32 = started.stdout.trim().parse().expect("kickstart -p prints the pid");
    held.wait_started();
    let printed = print_job(&id).stdout;
    assert_eq!(pid_of(&id), Some(pid), "{printed}");
    assert_eq!(print_field(&printed, "started suspended = "), Some("0"), "{printed}");
    held.release();
}

/// A hold left pending by hand (a `run` that failed after it) is consumed by the next `run`, which
/// completes.
#[skuld::test]
fn a_stranded_hold_is_consumed_by_the_next_run(#[fixture(launchd_group)] _group: &Group) {
    let id = support::random_test_id();
    let task_id = id_of(&id);
    let dir = tempfile::tempdir().expect("tempdir");
    let log = dir.path().join("task.log");
    let mgr = Launchd::new();
    let _guard = Guard::install(&mgr, &with_logs(task(&id, sh("echo done"), Some("0 3 * * *")), &log));
    mgr.enable(&task_id).expect("enable");
    cmd::run("launchctl", &["debug", &format!("system/{id}"), "--start-suspended"]).expect_ok();

    let (exit, out) = run_task(&mgr, &task_id);

    assert_eq!(exit, Exit::Code(0));
    assert_eq!(String::from_utf8_lossy(&out), "done\n");
    assert_no_hold_pending(&id);
}

/// `uninstall` boots the job out, and with it a hold left pending: a task installed again under the same
/// id has none.
#[skuld::test]
fn uninstall_clears_a_stranded_hold(#[fixture(launchd_group)] _group: &Group) {
    let id = support::random_test_id();
    let task_id = id_of(&id);
    let mgr = Launchd::new();
    let spec = task(&id, sleeper(), Some("0 3 * * *"));
    let _guard = Guard::install(&mgr, &spec);
    mgr.enable(&task_id).expect("enable");
    cmd::run("launchctl", &["debug", &format!("system/{id}"), "--start-suspended"]).expect_ok();

    mgr.uninstall(&task_id).expect("uninstall");

    mgr.install(&spec, false).expect("install again");
    mgr.enable(&task_id).expect("enable again");
    assert_no_hold_pending(&id);
}

// Schedules launchd can and cannot hold ===============================================================================

/// `install` of `schedule` is refused as invalid for the task, and writes nothing. Returns the refusal's
/// text.
fn refused(schedule: &str) -> String {
    let id = support::random_test_id();
    let mgr = Launchd::new();
    let _guard = Guard(id_of(&id));

    let e = mgr
        .install(&task(&id, sleeper(), Some(schedule)), false)
        .expect_err("a schedule launchd cannot hold is refused");

    assert!(matches!(e.cause(), Error::Invalid { .. }), "{e:?}");
    for path in [
        staging_path(&id),
        timer_staging_path(&id),
        enabled_path(&id),
        timer_enabled_path(&id),
    ] {
        assert!(!path.exists(), "a refused install wrote {}", path.display());
    }
    e.cause().to_string()
}

/// A day of the month and a day of the week that must both match: launchd only ORs them.
#[skuld::test]
fn an_and_day_rule_is_refused_naming_the_schedule(#[fixture(launchd_group)] _group: &Group) {
    let message = refused("0 0 */2 * 1");

    assert!(
        message.contains("launchd matches a day of the month and a day of the week as either-or"),
        "{message}"
    );
    assert!(message.contains("`0 0 */2 * 1`"), "{message}");
}

#[skuld::test]
fn a_schedule_that_must_name_29_february_is_refused_at_install_and_writes_nothing(
    #[fixture(launchd_group)] _group: &Group,
) {
    let message = refused("0 0 29 * *");

    assert!(
        message.contains("schedule day-of-month field: launchd has no year, so it cannot run on 29 February"),
        "{message}"
    );
    assert!(message.contains("`0 0 29 * *`"), "{message}");
}

/// launchd ignores the month of an entry that names a weekday (measured), so a weekday schedule cannot
/// be limited to some months.
#[skuld::test]
fn a_weekday_under_a_restricted_month_is_refused_at_install_and_writes_nothing(
    #[fixture(launchd_group)] _group: &Group,
) {
    let message = refused("0 0 * 6 1");

    assert!(
        message.contains("schedule day-of-week field: launchd ignores the month in an entry that names a weekday"),
        "{message}"
    );
    assert!(message.contains("`0 0 * 6 1`"), "{message}");
}

/// Every day of a month range is the `{Month, Day}` pairs that exist: it installs and arms.
#[skuld::test]
fn a_month_range_with_every_day_installs_as_month_day_pairs_and_is_armed(#[fixture(launchd_group)] _group: &Group) {
    let id = support::random_test_id();
    let task_id = id_of(&id);
    let mgr = Launchd::new();
    let _guard = Guard::install(&mgr, &task(&id, sleeper(), Some("0 3 * 3-5 *")));

    let before = SystemTime::now();
    mgr.enable(&task_id).expect("enable");
    let status = task_status(&mgr, &task_id);
    let after = SystemTime::now();

    assert!(is_loaded(&id) && is_loaded(&timer_of(&id)));
    assert_next_run_strict(&status, "0 3 * 3-5 *", &task_id, before, after);
}

/// A date that never exists (30 February) leaves a timer with no calendar: it is armed, and never fires.
#[skuld::test]
fn a_schedule_with_no_date_installs_and_arms_a_timer_without_a_calendar(#[fixture(launchd_group)] _group: &Group) {
    let id = support::random_test_id();
    let task_id = id_of(&id);
    let mgr = Launchd::new();
    let _guard = Guard::install(&mgr, &task(&id, sleeper(), Some("0 0 30 2 *")));

    mgr.enable(&task_id).expect("enable");

    assert!(is_loaded(&id) && is_loaded(&timer_of(&id)));
    let timer = fs::read_to_string(timer_enabled_path(&id)).expect("read the timer's plist");
    assert!(!timer.contains("StartCalendarInterval"), "{timer}");
    let status = task_status(&mgr, &task_id);
    assert!(status.common.enabled, "{status:?}");
    assert_eq!(status.next_run, None, "{status:?}");
}

/// A schedule of 10,080 entries (every minute of the first seven days of a month) is installed and armed:
/// `enable` returns and both labels are loaded. launchd's registration of the entries is not waited for.
#[skuld::test]
fn a_schedule_of_ten_thousand_entries_installs_and_is_armed(#[fixture(launchd_group)] _group: &Group) {
    let id = support::random_test_id();
    let task_id = id_of(&id);
    let mgr = Launchd::new();
    let _guard = Guard::install(&mgr, &task(&id, sleeper(), Some("* * 1-7 * *")));

    mgr.enable(&task_id).expect("enable");

    assert!(is_loaded(&id) && is_loaded(&timer_of(&id)));
    let timer = fs::read_to_string(timer_enabled_path(&id)).expect("read the timer's plist");
    assert_eq!(timer.matches("<key>Minute</key>").count(), 10_080);
}

// Crashes =============================================================================================================

/// The two states a crash leaves a verb in, built by moving the files by hand, are finished by running
/// the verb again: a `disable` that moved the timer to staging and not the job, and an `enable` that moved
/// the timer to `LaunchDaemons` and did not load it.
#[skuld::test]
fn enable_and_disable_finish_after_a_crash_on_a_real_launchd(#[fixture(launchd_group)] _group: &Group) {
    let mgr = Launchd::new();

    let id = support::random_test_id();
    let task_id = id_of(&id);
    let _guard = Guard::install(&mgr, &task(&id, sleeper(), Some("0 3 * * *")));
    mgr.enable(&task_id).expect("enable");
    cmd::run("launchctl", &["bootout", &format!("system/{}", timer_of(&id))]).expect_ok();
    fs::rename(timer_enabled_path(&id), timer_staging_path(&id)).expect("move the timer to staging by hand");

    mgr.disable(&task_id).expect("disable finishes");

    assert!(staging_path(&id).is_file() && timer_staging_path(&id).is_file());
    assert!(!enabled_path(&id).exists() && !timer_enabled_path(&id).exists());
    assert!(is_loaded(&id), "the job is still loaded");
    assert!(!is_loaded(&timer_of(&id)), "the timer is not");
    let status = task_status(&mgr, &task_id);
    assert!(!status.common.enabled, "{status:?}");

    let id = support::random_test_id();
    let task_id = id_of(&id);
    let _guard = Guard::install(&mgr, &task(&id, sleeper(), Some("0 3 * * *")));
    mgr.enable(&task_id).expect("enable");
    cmd::run("launchctl", &["bootout", &format!("system/{}", timer_of(&id))]).expect_ok();
    assert!(timer_enabled_path(&id).is_file() && !is_loaded(&timer_of(&id)));

    mgr.enable(&task_id).expect("enable finishes");

    assert!(is_loaded(&id) && is_loaded(&timer_of(&id)), "both are loaded");
    assert!(enabled_path(&id).is_file() && timer_enabled_path(&id).is_file());
    let status = task_status(&mgr, &task_id);
    assert!(status.common.enabled, "{status:?}");
}
