//! The owner's demonstration on Task Scheduler, through the `goetia` binary: a task with a cron `schedule`
//! is installed and enabled, fires by itself, shows its output through `run`, reports its last result and
//! next run through `status`, and is removed by `uninstall`. The task runs as SYSTEM.
//!
//! "Armed" and "fired" are two installs of the one id, so no read can straddle a fire: the first has a
//! daily schedule whose time is 12 hours away, the second is `* * * * *`.
//!
//! The platform-neutral half (the manifest, the binary, the document) is `support::demonstration`.

use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use goetia::manager::{Exit, State};
use goetia::testing::task_run_times;
use jiff::civil::Date;
use jiff::tz::TimeZone;
use jiff::{Timestamp, Zoned};

use crate::managed::UNIT_DIR_EXCLUSIVE;
use crate::support::demonstration::{
    Cli, DISARMED, Expect, LastResult, Manifest, Time, ZONE, assert_task_fields, daily_in_half_a_day, instant,
    read_in_the_zone,
};
use crate::support::sentinel::RUN_UNTIL_RELEASED;
use crate::support::{self, ConnectBack, Group, ServiceGuard, scm_group, wait};
use crate::tasks::{
    prints_and_exits, records_of, runs_until_ended, status_once_ended, task_exists, task_status, test_id,
};

const EVERY_MINUTE: &str = "* * * * *";

/// What a `stop` leaves as `last-result`. `LastTaskResult` reads `0x41306` (`SCHED_S_TASK_TERMINATED`),
/// which goetia reports as `0xC000013A`, a console close event.
const STOPPED: i32 = 0xC000_013A_u32 as i32;

/// This binary, told to connect to `listener` and then run until the test drops that connection, and
/// exit 0.
fn runs_until_released(listener: &ConnectBack) -> Vec<String> {
    vec![
        support::current_exe_str(),
        RUN_UNTIL_RELEASED.to_owned(),
        listener.port().to_string(),
    ]
}

/// `time` as the report prints it: to the second.
fn to_the_second(time: SystemTime) -> SystemTime {
    let secs = time
        .duration_since(UNIX_EPOCH)
        .expect("a time after the epoch")
        .as_secs();
    UNIX_EPOCH + Duration::from_secs(secs)
}

/// `time` cut down to the minute it is in.
fn to_the_minute(time: SystemTime) -> SystemTime {
    let secs = to_the_second(time)
        .duration_since(UNIX_EPOCH)
        .expect("a time after the epoch")
        .as_secs();
    UNIX_EPOCH + Duration::from_secs(secs / 60 * 60)
}

/// The next time after `after` that the clock of the machine's zone reads the hour and minute of
/// `daily` (`M H * * *`): its civil date, hour and minute worked out here, not read from Task Scheduler.
fn next_civil_time(daily: &str, after: SystemTime) -> SystemTime {
    let fields: Vec<&str> = daily.split(' ').collect();
    let [minute, hour, "*", "*", "*"] = fields[..] else {
        panic!("`{daily}` is not a daily schedule");
    };
    let at = |date: Date| {
        date.at(hour.parse().expect("an hour"), minute.parse().expect("a minute"), 0, 0)
            .to_zoned(TimeZone::system())
            .unwrap_or_else(|e| panic!("`{daily}` on {date}: {e}"))
    };
    let after: Zoned = Timestamp::try_from(after)
        .expect("a time jiff holds")
        .to_zoned(TimeZone::system());
    let today = at(after.date());
    if today.timestamp() > after.timestamp() {
        today.timestamp().into()
    } else {
        at(after.date().tomorrow().expect("a day after today"))
            .timestamp()
            .into()
    }
}

#[skuld::test(labels = [UNIT_DIR_EXCLUSIVE], serial = UNIT_DIR_EXCLUSIVE)]
fn the_demonstration_runs_through_the_binary(#[fixture(scm_group)] _group: &Group) {
    let task_id = test_id();
    let id = task_id.as_str();
    let cli = Cli::new();
    let in_zone = Cli::in_zone(ZONE);
    let mgr = support::stock_shim_manager();
    let listener = ConnectBack::listen();
    let daily = daily_in_half_a_day();
    let mut manifest = Manifest::new(id, runs_until_released(&listener), &daily);
    // Declared after the manifest, so the task is removed before the directory its log is in.
    let _guard = ServiceGuard::new(id);

    // 1. Installed, the task is idle and disarmed.
    cli.install(&manifest);
    assert_task_fields(&cli.status_json(id), id, DISARMED);

    // 2. Armed. Enabled with a schedule 12 hours away, it is idle with the next run Task Scheduler itself
    // lists first, never run, in every form and at the offset of `TZ`. Nothing fires, so one read is the read.
    // That run time is also the next time the machine's clock reads the hour and minute of `daily`, which
    // is worked out here from the civil time: a wrong translation of the cron is not Task Scheduler's own.
    let enabling_at = SystemTime::now();
    cli.daemon(&["enable", id]);
    let listed = task_run_times(&task_id, enabling_at, enabling_at + Duration::from_secs(36 * 3600))
        .unwrap_or_else(|e| panic!("the run times of `{daily}`: {}", e.report()));
    let next = to_the_second(
        *listed
            .first()
            .unwrap_or_else(|| panic!("`{daily}` has a run time within 36 hours")),
    );
    assert_eq!(
        next,
        next_civil_time(&daily, enabling_at),
        "`{daily}`: Task Scheduler's first run time is not the next time the clock reads it"
    );
    let armed = read_in_the_zone(&in_zone, id);
    assert_task_fields(
        &armed.json,
        id,
        Expect {
            state: "idle",
            enabled: true,
            pid: Time::Null,
            last_result: LastResult::Null,
            last_run: Time::Null,
            next_run: Time::Present,
        },
    );
    armed.assert_next_run("idle", |_, _| vec![next]);
    armed.assert_no_last_run();
    cli.daemon(&["disable", id]);
    let disarmed = read_in_the_zone(&in_zone, id);
    assert_task_fields(&disarmed.json, id, DISARMED);
    disarmed.assert_state("idle");
    disarmed.assert_no_last_run();

    // 3. Fires. The same id, every minute, enabled: the schedule fires with nothing started by the test,
    // and the program connects back. It then runs until the test drops the connection, so any later fire
    // finds a run alive and starts nothing (`IgnoreNew`).
    manifest.schedule = EVERY_MINUTE.to_string();
    cli.install(&manifest);
    let enabling_at = SystemTime::now();
    cli.daemon(&["enable", id]);
    // The program connects when the schedule fires, at a minute Task Scheduler chooses: no bound fits.
    let held = listener.accept_unbounded();
    let fires = task_run_times(&task_id, enabling_at, SystemTime::now())
        .unwrap_or_else(|e| panic!("the run times of `{EVERY_MINUTE}`: {}", e.report()));
    let (Some(&first), Some(&last)) = (fires.first(), fires.last()) else {
        panic!("a run started and Task Scheduler lists no run time since {enabling_at:?}");
    };
    let (first, last) = (to_the_second(first), last + Duration::from_secs(60));
    let assert_fired_in_range = |form: &str, time: &str| {
        let fired = instant(time);
        assert!(
            fired >= first && fired < last,
            "{form}: last-run {fired:?} is not in [{first:?}, {last:?}) of {fires:?}"
        );
    };
    // The run is in flight: `last-result` reads null (`LastTaskResult` says it is running), `last-run` is
    // the run's start, and the armed schedule still has a next run, a later minute.
    let upcoming = |read: &str| {
        task_run_times(&task_id, enabling_at, SystemTime::now() + Duration::from_secs(600))
            .unwrap_or_else(|e| panic!("{read}: the run times: {}", e.report()))
            .into_iter()
            .map(to_the_second)
            .collect::<Vec<_>>()
    };
    let running = read_in_the_zone(&in_zone, id);
    assert_task_fields(
        &running.json,
        id,
        Expect {
            state: "running",
            enabled: true,
            pid: Time::Present,
            last_result: LastResult::Null,
            last_run: Time::Present,
            next_run: Time::Present,
        },
    );
    let times = upcoming("running");
    let mut fire_minute = None;
    for read in running.reads() {
        let started = read.last_run.as_deref().unwrap_or_else(|| {
            panic!("a run is in flight, and a read shows no last-run: {running:?}");
        });
        assert_fired_in_range(read.form, started);
        let minute = to_the_minute(instant(started));
        fire_minute = Some(fire_minute.map_or(minute, |earliest: SystemTime| earliest.min(minute)));
    }
    let fire_minute = fire_minute.expect("three reads");
    // Not strictly after the fire: `NextRunTime` may not have moved past it when the program connects.
    running.assert_next_run("running", |_, _| {
        times.iter().copied().filter(|time| *time >= fire_minute).collect()
    });
    // No fire can come between the end of the run and the reads that follow it.
    cli.daemon(&["disable", id]);
    drop(held);
    status_once_ended(&mgr, &task_id);
    let finished = read_in_the_zone(&in_zone, id);
    assert_task_fields(
        &finished.json,
        id,
        Expect {
            state: "idle",
            enabled: false,
            pid: Time::Null,
            last_result: LastResult::Code(0),
            last_run: Time::Present,
            next_run: Time::Null,
        },
    );
    finished.assert_state("idle");
    for (before, after) in running.reads().into_iter().zip(finished.reads()) {
        assert_eq!(before.last_run, after.last_run, "{running:?} then {finished:?}");
        assert_fired_in_range(
            after.form,
            after.last_run.as_deref().expect("a finished run has a last-run"),
        );
    }

    // 4. `run` shows the output and exits with the run's result. The same id, installed again with another
    // command: a disarmed task is replaced.
    let logs: PathBuf = manifest.dir().join("task.log");
    manifest.command = prints_and_exits(7, "a-known-line");
    manifest.logs = Some(logs);
    cli.install(&manifest);
    let run = cli.run(&["daemon", "run", id]);
    assert_eq!(run.code, Some(7), "{run}");
    assert!(
        run.stdout.lines().count() == 1 && run.stdout.starts_with("a-known-line "),
        "the program's line, and nothing else: {run}"
    );

    // 5. A run that `start` began and `stop` ended is idle, and its result is the console close event.
    manifest.command = runs_until_ended(&listener);
    cli.install(&manifest);
    cli.daemon(&["start", id]);
    listener.accept("the task's program to start");
    cli.daemon(&["stop", id]);
    // The run has ended once the task is idle with no pid and a result other than step 4's. The instance
    // may stay listed for a few ms after the result is written, so the result is read once that is over,
    // and a wrong result fails here with its value instead of hanging.
    let mut ended = None;
    wait::until(|| {
        let status = task_status(&mgr, &task_id);
        let done = status.common.state == State::Idle
            && status.common.pid.is_none()
            && status.last_result.is_some()
            && status.last_result != Some(Exit::Code(7));
        ended = Some(status);
        done
    });
    let ended = ended.expect("a status was read");
    assert_eq!(
        ended.last_result,
        Some(Exit::Code(STOPPED)),
        "a stopped run's result: {ended:?}"
    );
    assert_task_fields(
        &cli.status_json(id),
        id,
        Expect {
            state: "idle",
            enabled: false,
            pid: Time::Null,
            last_result: LastResult::Code(i64::from(STOPPED)),
            last_run: Time::Present,
            next_run: Time::Null,
        },
    );

    // 6. `uninstall` removes it.
    cli.daemon(&["uninstall", id]);
    cli.assert_not_installed(id);
    assert!(!task_exists(&task_id), "the task is left behind");
    assert!(
        !records_of(&task_id).exists(),
        "{} is left behind",
        records_of(&task_id).display()
    );
}
