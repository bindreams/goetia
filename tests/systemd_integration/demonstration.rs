//! The owner's demonstration on systemd, through the `goetia` binary: a task with a cron `schedule` is
//! installed and enabled, fires by itself, shows its output through `run`, reports its last result and
//! next run through `status`, and is removed by `uninstall`.
//!
//! "Armed" and "fired" are two installs of the one id, so no read can straddle a fire: the first has a
//! daily schedule whose time is 12 hours away, the second is `* * * * *`.
//!
//! The platform-neutral half (the manifest, the binary, the document) is `support::demonstration`.

use std::path::PathBuf;
use std::time::{Duration, SystemTime};

use goetia::backend::systemd::manager::Systemd;
use serde_json::Value;

use crate::linux::{UNIT_DIR_EXCLUSIVE, timer_path, timer_wants_symlink, unit_path, wait_for_started};
use crate::support::cron_oracle::expected_next_runs;
use crate::support::demonstration::{
    Cli, Expect, LastResult, Manifest, Time, ZONE, assert_task_fields, daily_in_half_a_day, instant, read_in_the_zone,
};
use crate::support::tasks::{Held, id_of, sh, status_with_result};
use crate::support::{self, Group, ServiceGuard, systemd_group};

const EVERY_MINUTE: &str = "* * * * *";

#[skuld::test(labels = [UNIT_DIR_EXCLUSIVE], serial = UNIT_DIR_EXCLUSIVE)]
fn the_demonstration_runs_through_the_binary(#[fixture(systemd_group)] _group: &Group) {
    let id = support::random_test_id();
    let _guard = ServiceGuard::new(&id);
    let cli = Cli::new();
    let in_zone = Cli::in_zone(ZONE);
    let mut held = Held::new();
    let daily = daily_in_half_a_day();
    let mut manifest = Manifest::new(&id, held.command.clone(), &daily);

    // 1. Installed, the task is idle and disarmed.
    cli.install(&manifest);
    assert_task_fields(
        &cli.status_json(&id),
        &id,
        Expect {
            state: "idle",
            enabled: false,
            pid: Time::Null,
            last_result: LastResult::Null,
            last_run: Time::Null,
            next_run: Time::Null,
        },
    );

    // 2. Armed. Enabled with a schedule 12 hours away, it is idle with the next run the cron names, never
    // run, in every form and at the offset of `TZ`. Nothing fires, so one read is the read.
    let enabled_at = SystemTime::now();
    cli.daemon(&["enable", &id]);
    let [_, next, _] = expected_next_runs(&daily, &id_of(&id), enabled_at, SystemTime::now());
    let armed = read_in_the_zone(&in_zone, &id);
    assert_task_fields(
        &armed.json,
        &id,
        Expect {
            state: "idle",
            enabled: true,
            pid: Time::Null,
            last_result: LastResult::Null,
            last_run: Time::Null,
            next_run: Time::Present,
        },
    );
    armed.assert_state("idle", Some(next));
    for read in armed.reads() {
        assert!(read.last_run.is_none(), "{armed:?}");
    }
    cli.daemon(&["disable", &id]);
    let disarmed = read_in_the_zone(&in_zone, &id);
    assert_task_fields(
        &disarmed.json,
        &id,
        Expect {
            state: "idle",
            enabled: false,
            pid: Time::Null,
            last_result: LastResult::Null,
            last_run: Time::Null,
            next_run: Time::Null,
        },
    );
    disarmed.assert_state("idle", None);

    // 3. Fires. The same id, every minute, enabled: the schedule fires with nothing started by the test,
    // and the program announces itself. systemd has no next run for a timer whose service is running, and
    // the run is held, so any later fire is dropped by the manager.
    manifest.schedule = EVERY_MINUTE.to_string();
    cli.install(&manifest);
    let enabling_at = SystemTime::now();
    cli.daemon(&["enable", &id]);
    let candidates = expected_next_runs(EVERY_MINUTE, &id_of(&id), enabling_at, SystemTime::now());
    held.wait_started();
    // The unit is active, and journald need not hold the run's `Starting` entry yet: the task is running
    // with a pid and no next run, and `last-run` is there or not.
    let before_the_journal = cli.status_json(&id);
    assert_task_fields(
        &before_the_journal,
        &id,
        Expect {
            state: "running",
            enabled: true,
            pid: Time::Present,
            last_result: LastResult::Null,
            last_run: Time::Any,
            next_run: Time::Null,
        },
    );
    // One range: `enable` may span a minute boundary or more, and the minutes between candidates are
    // the schedule's too.
    let (first, last) = (candidates[0], candidates[2] + Duration::from_secs(60));
    let assert_fired_in_range = |form: &str, time: &str| {
        let fired = instant(time);
        assert!(
            fired >= first && fired < last,
            "{form}: last-run {fired:?} is not in [{first:?}, {last:?}) of {candidates:?}"
        );
    };
    if let Some(time) = before_the_journal["last-run"].as_str() {
        assert_fired_in_range("status --json before the journal", time);
    }
    // `last-run` is read from the journal: wait for the `Starting` entry `goetia` matches, and then one
    // read must show the `last-run`.
    wait_for_started(&id);
    let running = read_in_the_zone(&in_zone, &id);
    assert_task_fields(
        &running.json,
        &id,
        Expect {
            state: "running",
            enabled: true,
            pid: Time::Present,
            last_result: LastResult::Null,
            last_run: Time::Present,
            next_run: Time::Null,
        },
    );
    running.assert_state("running", None);
    for read in running.reads() {
        let time = read.last_run.as_deref().unwrap_or_else(|| {
            panic!("journald holds the run's Starting entry, and a read shows no last-run: {running:?}")
        });
        assert_fired_in_range(read.form, time);
    }
    // No fire can come between the end of the run and the reads that follow it.
    cli.daemon(&["disable", &id]);
    held.release();
    status_with_result(&Systemd::new(), &id_of(&id));
    let finished = read_in_the_zone(&in_zone, &id);
    assert_task_fields(
        &finished.json,
        &id,
        Expect {
            state: "idle",
            enabled: false,
            pid: Time::Null,
            last_result: LastResult::Code(0),
            last_run: Time::Present,
            next_run: Time::Null,
        },
    );
    finished.assert_state("idle", None);
    for (before, after) in running.reads().into_iter().zip(finished.reads()) {
        assert_eq!(before.last_run, after.last_run, "{running:?} then {finished:?}");
    }

    // 4. `run` shows the output and exits with the run's result. The same id, installed again with another
    // command: a disarmed task is replaced.
    let logs: PathBuf = manifest.dir().join("task.log");
    manifest.command = sh("echo a-known-line; exit 7");
    manifest.logs = Some(logs);
    cli.install(&manifest);
    let run = cli.run(&["daemon", "run", &id]);
    assert_eq!(run.code, Some(7), "{run}");
    assert_eq!(run.stdout, "a-known-line\n", "{run}");

    // 5. `uninstall` removes it.
    cli.daemon(&["uninstall", &id]);
    let gone = cli.run(&["daemon", "status", &id, "--json"]);
    assert_eq!(gone.code, Some(1), "{gone}");
    let doc: Value = serde_json::from_str(&gone.stdout).unwrap_or_else(|e| panic!("not JSON ({e}): {gone}"));
    assert_eq!(doc["errors"][0]["kind"], "not-installed", "{gone}");
    for path in [unit_path(&id), timer_path(&id), timer_wants_symlink(&id)] {
        assert!(
            std::fs::symlink_metadata(&path).is_err(),
            "{} is left behind",
            path.display()
        );
    }
}
