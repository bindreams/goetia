//! A task's schedule on the real Task Scheduler: that it fires, and that what it fires at is what the
//! cron says. Task Scheduler's own `GetRunTimes` (`testing::task_run_times`) is the oracle for the
//! second: it computes the run times of a disarmed task too (M2), so those tests arm nothing.

use std::collections::BTreeSet;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use goetia::error::Error;
use goetia::manager::{Budget, Exit, ServiceManager as _, State};
use goetia::spec::{HashSeed, Schedule, User};
use goetia::testing::{FIRING_SCHEDULES, cron_matches, task_run_times};
use jiff::civil::{self, DateTime};
use jiff::tz::TimeZone;
use jiff::{Timestamp, ToSpan as _};

use crate::managed::UNIT_DIR_EXCLUSIVE;
use crate::support::sentinel::CONNECT_BACK;
use crate::support::windows_tasks::task_spec;
use crate::support::{self, ConnectBack, Group, scm_group, wait};
use crate::tasks::{
    SCHEDULE, TaskGuard, records_of, runs_until_ended, status_once_ended, task_as, task_exists, task_status,
};

/// This binary, told to connect to `listener` and exit.
fn connects_back(listener: &ConnectBack) -> Vec<String> {
    vec![
        support::current_exe_str(),
        CONNECT_BACK.to_owned(),
        listener.port().to_string(),
    ]
}

// Firing ==============================================================================================================

/// An armed task starts by itself at its next minute, and a run that Task Scheduler began is one it
/// records: the task is idle again with the program's own result. The next minute is an event of the
/// machine, so it is waited for with no cap: CI's watchdog is the bound.
#[skuld::test(labels = [UNIT_DIR_EXCLUSIVE], serial = UNIT_DIR_EXCLUSIVE)]
fn the_scheduler_fires_the_task_by_itself(#[fixture(scm_group)] _group: &Group) {
    let guard = TaskGuard::new();
    let id = &guard.id;
    let mgr = support::stock_shim_manager();
    let listener = ConnectBack::listen();
    mgr.install(
        &task_spec(id, connects_back(&listener), Some("* * * * *"), User::Root, None),
        false,
    )
    .expect("install");
    mgr.enable(id).expect("enable");

    listener.accept_unbounded();

    let status = status_once_ended(&mgr, id);
    assert_eq!(status.last_result, Some(Exit::Code(0)));
    assert!(status.last_run.is_some(), "{status:?}");
    mgr.disable(id).expect("disable");
    mgr.uninstall(id).expect("uninstall");
}

/// A fire while a run is alive starts nothing (`IgnoreNew`): the run is the one that was there, and its
/// program is not started again. The fire has been handled once the next run time is a later one, which is
/// the only sign of it a test can read; a start that came after that sign would pass unseen.
#[skuld::test(labels = [UNIT_DIR_EXCLUSIVE], serial = UNIT_DIR_EXCLUSIVE)]
fn a_fire_during_a_run_starts_nothing(#[fixture(scm_group)] _group: &Group) {
    let guard = TaskGuard::new();
    let id = &guard.id;
    let mgr = support::stock_shim_manager();
    let listener = ConnectBack::listen();
    mgr.install(
        &task_spec(id, runs_until_ended(&listener), Some("* * * * *"), User::Root, None),
        false,
    )
    .expect("install");
    // Started before it is armed, so the first fire finds the run alive.
    mgr.start(id, Budget::DEFAULT).expect("start");
    listener.accept("the task's program to start");
    mgr.enable(id).expect("enable");
    let status = task_status(&mgr, id);
    let (pid, fire) = (
        status.common.pid,
        status.next_run.expect("an armed task has a next run"),
    );

    wait::until(|| task_status(&mgr, id).next_run.is_some_and(|next| next > fire));

    let after = task_status(&mgr, id);
    assert_eq!(after.common.state, State::Running);
    assert_eq!(after.common.pid, pid, "the run that was there");
    assert!(!listener.connected_yet(), "no second program was started");
    mgr.stop(id, Budget::DEFAULT).expect("stop");
    mgr.uninstall(id).expect("uninstall");
}

/// `disable` stops a repetition in progress: a disabled `* * * * *` task starts nothing at later boundaries.
/// A control task armed alike marks when boundary B1 (the first after `disable` returned) has passed; the
/// disabled task's last run must predate B1 - 1 s (tolerating a start dispatched before `disable` returned,
/// which the task was still armed for, and millisecond skew). `enable` then starts it again.
#[skuld::test(labels = [UNIT_DIR_EXCLUSIVE], serial = UNIT_DIR_EXCLUSIVE)]
fn a_disabled_minutely_task_starts_nothing_at_the_boundaries_that_follow(#[fixture(scm_group)] _group: &Group) {
    let (subject, control) = (TaskGuard::new(), TaskGuard::new());
    let mgr = support::stock_shim_manager();
    let (subject_listener, control_listener) = (ConnectBack::listen(), ConnectBack::listen());
    for (guard, listener) in [(&subject, &subject_listener), (&control, &control_listener)] {
        mgr.install(
            &task_spec(&guard.id, connects_back(listener), Some("* * * * *"), User::Root, None),
            false,
        )
        .expect("install");
        mgr.enable(&guard.id).expect("enable");
    }
    // Both have started, so the repetition of the hour is in progress.
    subject_listener.accept_unbounded();
    control_listener.accept_unbounded();

    mgr.disable(&subject.id).expect("disable");
    let disabled_at = SystemTime::now();

    let minute = Duration::from_secs(60);
    let epoch_secs = disabled_at
        .duration_since(UNIX_EPOCH)
        .expect("after the epoch")
        .as_secs();
    let b1 = UNIX_EPOCH + minute * (epoch_secs / 60 + 1).try_into().expect("a minute count fits");
    let slack = Duration::from_secs(1);
    let mut seen = b1;
    wait::until(|| match task_status(&mgr, &control.id).last_run {
        Some(run) if run + slack >= b1 + minute => {
            seen = run;
            true
        }
        _ => false,
    });

    // A run dispatched before the disable is over before the last start is read.
    let after = status_once_ended(&mgr, &subject.id);
    assert!(
        after.last_run.is_none_or(|run| run + slack < b1),
        "the disabled task started at or after the boundary {b1:?}, which the control passed: {after:?}"
    );

    mgr.enable(&subject.id).expect("enable again");
    wait::until(|| task_status(&mgr, &subject.id).last_run.is_some_and(|run| run > seen));
}

/// A task that is not armed still starts when asked, and stays not armed.
#[skuld::test(labels = [UNIT_DIR_EXCLUSIVE], serial = UNIT_DIR_EXCLUSIVE)]
fn start_of_a_disarmed_task_runs_it(#[fixture(scm_group)] _group: &Group) {
    let guard = TaskGuard::new();
    let id = &guard.id;
    let mgr = support::stock_shim_manager();
    let listener = ConnectBack::listen();
    mgr.install(
        &task_spec(id, connects_back(&listener), Some(SCHEDULE), User::Root, None),
        false,
    )
    .expect("install");
    assert!(!task_status(&mgr, id).common.enabled, "precondition: disarmed");

    mgr.start(id, Budget::DEFAULT).expect("start");

    listener.accept("the disarmed task's program to start");
    let status = status_once_ended(&mgr, id);
    assert_eq!(status.last_result, Some(Exit::Code(0)));
    assert!(!status.common.enabled, "a start does not arm it");
    mgr.uninstall(id).expect("uninstall");
}

/// A date no month has (`30 2`) cannot be written as a trigger, so it is a one-time trigger in the past:
/// the task installs, `enable` arms it, and nothing is ever next.
#[skuld::test(labels = [UNIT_DIR_EXCLUSIVE], serial = UNIT_DIR_EXCLUSIVE)]
fn a_date_no_month_has_is_installed_armed_and_never_fires(#[fixture(scm_group)] _group: &Group) {
    let guard = TaskGuard::new();
    let id = &guard.id;
    let mgr = support::stock_shim_manager();
    let spec = task_as(id, User::Root, Some("0 0 30 2 *"));
    mgr.install(&spec, false).expect("install");
    assert!(!task_status(&mgr, id).common.enabled, "installed disarmed");

    mgr.enable(id).expect("enable arms a schedule that never fires");

    let armed = task_status(&mgr, id);
    assert!(armed.common.enabled, "{armed:?}");
    assert_eq!(armed.next_run, None, "no date is next");
    assert_eq!(armed.last_run, None, "and nothing has run");
    let again = mgr.install(&spec, false).expect("install again");
    assert!(matches!(again, goetia::decide::Outcome::UpToDate), "{again:?}");
    mgr.uninstall(id).expect("uninstall");
}

// The run times =======================================================================================================

/// The end of 2028, a leap year.
fn end_of_two_years() -> DateTime {
    civil::date(2028, 12, 31).at(23, 59, 0, 0)
}

/// The end of January 2027.
fn end_of_january() -> DateTime {
    civil::date(2027, 1, 31).at(23, 59, 0, 0)
}

/// More run times in the two years than this, and a schedule is checked over January 2027 only.
/// `GetRunTimes` cost grows with the square of the run times it returns (about 150 s for 261,000), and the
/// nextest `ci` profile kills a test after 300 s.
const MANY_RUN_TIMES: usize = 100_000;

/// The schedules of [`FIRING_SCHEDULES`] with more than [`MANY_RUN_TIMES`] run times in the two years,
/// and so the only ones cut to January 2027. The check is two-way: a schedule over the limit that is not
/// listed fails, and so does a listed one that is not in [`FIRING_SCHEDULES`] or no longer over it.
const CUT_TO_JANUARY: [&str; 2] = ["* * * * *", "*/7 * * * *"];

fn cron_names(schedule: &Schedule, minute: DateTime) -> bool {
    cron_matches(
        schedule,
        u8::try_from(minute.minute()).expect("a minute"),
        u8::try_from(minute.hour()).expect("an hour"),
        u8::try_from(minute.day()).expect("a day"),
        u8::try_from(minute.month()).expect("a month"),
        u8::try_from(minute.weekday().to_sunday_zero_offset()).expect("a weekday"),
    )
}

/// Compares Task Scheduler's run times with the cron's own reading, every minute of the range, both
/// directions: an extra run time and a missing one. The zone is the machine's and must have no DST change
/// in the range (CI's is UTC).
fn assert_runs_as_cron_says(texts: &[&str]) {
    let mgr = support::stock_shim_manager();
    let zone = TimeZone::system();
    let first = civil::date(2027, 1, 1).at(0, 0, 0, 0);
    // The window is `(from, to]`.
    let instant = |at: DateTime| SystemTime::from(at.to_zoned(zone.clone()).expect("a time in the zone").timestamp());

    let end = end_of_two_years();
    for text in CUT_TO_JANUARY {
        assert!(
            FIRING_SCHEDULES.iter().any(|group| group.contains(&text)),
            "`{text}` is in CUT_TO_JANUARY but not in FIRING_SCHEDULES"
        );
    }
    for text in texts {
        let guard = TaskGuard::new();
        let id = &guard.id;
        let seed = HashSeed {
            hostname: "testing",
            id,
        };
        let schedule = Schedule::parse(text, &seed).unwrap_or_else(|e| panic!("`{text}`: {e}"));
        let named_until = |last: DateTime| {
            std::iter::successors(Some(first), |&minute| Some(minute + 1.minute()))
                .take_while(move |&minute| minute <= last)
                .filter(|&minute| cron_names(&schedule, minute))
        };
        let many = named_until(end).count() > MANY_RUN_TIMES;
        assert_eq!(
            many,
            CUT_TO_JANUARY.contains(text),
            "`{text}`: over {MANY_RUN_TIMES} run times in the two years is {many}, and CUT_TO_JANUARY must agree"
        );
        let last = if many { end_of_january() } else { end };
        let named: BTreeSet<DateTime> = named_until(last).collect();
        let (from, to) = (instant(first - 1.minute()), instant(last));

        mgr.install(&task_as(id, User::Root, Some(text)), false)
            .unwrap_or_else(|e| panic!("`{text}` is refused: {}", e.report()));
        let times =
            task_run_times(id, from, to).unwrap_or_else(|e| panic!("the run times of `{text}`: {}", e.report()));
        let mut listed: Vec<DateTime> = times
            .into_iter()
            .map(|time| {
                Timestamp::try_from(time)
                    .expect("a run time is a timestamp")
                    .to_zoned(zone.clone())
                    .datetime()
            })
            .collect();
        // A minute listed twice would be a double start: a set would merge it.
        listed.sort();
        if let Some(twice) = listed.windows(2).find(|pair| pair[0] == pair[1]) {
            panic!("`{text}`: Task Scheduler lists {} twice", twice[0]);
        }
        let fired: BTreeSet<DateTime> = listed.into_iter().collect();

        let wrong: BTreeSet<DateTime> = named.symmetric_difference(&fired).copied().collect();
        if let Some(minute) = wrong.first() {
            let (cron, scheduler) = if named.contains(minute) {
                ("names it", "has no run")
            } else {
                ("does not", "runs")
            };
            panic!(
                "`{text}`: {} minutes disagree, the first: {minute}: the cron {cron}, Task Scheduler {scheduler}",
                wrong.len()
            );
        }
        mgr.uninstall(id).expect("uninstall");
    }
}

#[skuld::test(labels = [UNIT_DIR_EXCLUSIVE], serial = UNIT_DIR_EXCLUSIVE)]
fn every_cron_shape_matches_the_schedulers_run_times(#[fixture(scm_group)] _group: &Group) {
    assert_runs_as_cron_says(FIRING_SCHEDULES[0]);
}

#[skuld::test(labels = [UNIT_DIR_EXCLUSIVE], serial = UNIT_DIR_EXCLUSIVE)]
fn irregular_times_of_day_match_the_schedulers_run_times(#[fixture(scm_group)] _group: &Group) {
    assert_runs_as_cron_says(FIRING_SCHEDULES[1]);
}

#[skuld::test(labels = [UNIT_DIR_EXCLUSIVE], serial = UNIT_DIR_EXCLUSIVE)]
fn a_fifth_weekday_matches_the_schedulers_run_times(#[fixture(scm_group)] _group: &Group) {
    assert_runs_as_cron_says(FIRING_SCHEDULES[2]);
}

// Refusals ============================================================================================================

/// A schedule a task cannot hold is refused naming the limit, before anything is written: no task, no
/// records. Each text is pinned, with its sentence, by a unit test of the compilation: the 48-trigger cap,
/// an AND of two restricted day fields, and an either-or of two.
#[skuld::test(labels = [UNIT_DIR_EXCLUSIVE], serial = UNIT_DIR_EXCLUSIVE)]
fn a_refused_schedule_writes_nothing(#[fixture(scm_group)] _group: &Group) {
    let mgr = support::stock_shim_manager();
    for text in ["1,2,4,8,16 * * * *", "0 0 */2 * 1", "0 0 1 * 1", "30 3 1,15 * 1-5"] {
        let guard = TaskGuard::new();
        let id = &guard.id;

        let refused = mgr
            .install(&task_as(id, User::Root, Some(text)), false)
            .expect_err("a schedule a task cannot hold is refused");

        assert!(
            matches!(refused.cause(), Error::Invalid { .. }),
            "`{text}`: {refused:?}"
        );
        assert!(!task_exists(id), "`{text}` registered a task");
        assert!(!records_of(id).exists(), "`{text}` made records");
        let absent = mgr.status(id).expect_err("nothing is installed");
        assert!(
            matches!(absent.cause(), Error::NotInstalled { .. }),
            "`{text}`: {absent:?}"
        );
    }
}
