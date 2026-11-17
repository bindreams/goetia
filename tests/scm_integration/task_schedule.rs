//! A task's schedule on the real Task Scheduler: that it fires, and that what it fires at is what the
//! cron says. Task Scheduler's own `GetRunTimes` (`testing::task_run_times`) is the oracle for the
//! second: it computes the run times of a disarmed task too (M2), so those tests arm nothing.

use std::collections::BTreeSet;
use std::time::SystemTime;

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
use crate::support::{self, ConnectBack, Group, kill, scm_group};
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

    kill::until(|| task_status(&mgr, id).next_run.is_some_and(|next| next > fire));

    let after = task_status(&mgr, id);
    assert_eq!(after.common.state, State::Running);
    assert_eq!(after.common.pid, pid, "the run that was there");
    assert!(!listener.connected_yet(), "no second program was started");
    mgr.stop(id, Budget::DEFAULT).expect("stop");
    mgr.uninstall(id).expect("uninstall");
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

/// The civil minutes, in the machine's zone, that Task Scheduler runs the task `text` at in the two years
/// 2027 and 2028 (2028 is a leap year), compared with every minute of them against the cron's own reading
/// of it, in both directions: a run time the cron does not name and a minute it names that has none. The
/// range is fixed and not derived from today's date; the zone is the machine's, and the comparison
/// assumes it has no daylight-saving change in those years (CI's is UTC).
fn assert_runs_as_cron_says(texts: &[&str]) {
    let mgr = support::stock_shim_manager();
    let zone = TimeZone::system();
    let first = civil::date(2027, 1, 1).at(0, 0, 0, 0);
    let last = civil::date(2028, 12, 31).at(23, 59, 0, 0);
    // The window is `(from, to]`.
    let instant = |at: DateTime| SystemTime::from(at.to_zoned(zone.clone()).expect("a time in the zone").timestamp());
    let (from, to) = (instant(first - 1.minute()), instant(last));

    for text in texts {
        let guard = TaskGuard::new();
        let id = &guard.id;
        mgr.install(&task_as(id, User::Root, Some(text)), false)
            .unwrap_or_else(|e| panic!("`{text}` is refused: {}", e.report()));
        let times =
            task_run_times(id, from, to).unwrap_or_else(|e| panic!("the run times of `{text}`: {}", e.report()));
        let fired: BTreeSet<DateTime> = times
            .into_iter()
            .map(|time| {
                Timestamp::try_from(time)
                    .expect("a run time is a timestamp")
                    .to_zoned(zone.clone())
                    .datetime()
            })
            .collect();
        let seed = HashSeed {
            hostname: "testing",
            id,
        };
        let schedule = Schedule::parse(text, &seed).unwrap_or_else(|e| panic!("`{text}`: {e}"));

        let mut wrong = Vec::new();
        let mut minute = first;
        while minute <= last {
            let named = cron_matches(
                &schedule,
                u8::try_from(minute.minute()).expect("a minute"),
                u8::try_from(minute.hour()).expect("an hour"),
                u8::try_from(minute.day()).expect("a day"),
                u8::try_from(minute.month()).expect("a month"),
                u8::try_from(minute.weekday().to_sunday_zero_offset()).expect("a weekday"),
            );
            if named != fired.contains(&minute) {
                let (cron, scheduler) = if named {
                    ("names it", "has no run")
                } else {
                    ("does not", "runs")
                };
                wrong.push(format!("{minute}: the cron {cron}, Task Scheduler {scheduler}"));
            }
            minute += 1.minute();
        }
        assert!(
            wrong.is_empty(),
            "`{text}`: {} minutes disagree, the first: {}",
            wrong.len(),
            wrong[0]
        );
        // A run time that is no whole minute of the range is an extra the loop above cannot see.
        assert!(
            fired
                .iter()
                .all(|time| time.second() == 0 && *time >= first && *time <= last),
            "`{text}`: a run time that is no minute of the range: {:?}",
            fired
                .iter()
                .find(|time| time.second() != 0 || **time < first || **time > last)
        );
        mgr.uninstall(id).expect("uninstall");
    }
}

#[skuld::test(labels = [UNIT_DIR_EXCLUSIVE], serial = UNIT_DIR_EXCLUSIVE)]
fn every_cron_shape_matches_the_schedulers_run_times(#[fixture(scm_group)] _group: &Group) {
    assert_runs_as_cron_says(FIRING_SCHEDULES[0]);
}

#[skuld::test(labels = [UNIT_DIR_EXCLUSIVE], serial = UNIT_DIR_EXCLUSIVE)]
fn a_time_with_no_single_progression_matches_the_schedulers_run_times(#[fixture(scm_group)] _group: &Group) {
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
