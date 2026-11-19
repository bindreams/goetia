//! The owner's demonstration on launchd, through the `goetia` binary: a task with a cron `schedule` is
//! installed and enabled, fires by itself, shows its output through `run`, reports its last result and
//! next run through `status`, and is removed by `uninstall`.
//!
//! "Armed" and "fired" are two installs of the one id, so no read can straddle a fire: the first has a
//! daily schedule whose time is 12 hours away, the second is `* * * * *`.
//!
//! What launchd reports differently: `last-run` is null throughout (the manager keeps none), and
//! `next-run` is computed by goetia from the clock it reads, so it is judged by the strict oracle.
//!
//! The platform-neutral half (the manifest, the binary, the document) is `support::demonstration`.

use std::path::PathBuf;
use std::time::SystemTime;

use goetia::backend::launchd::manager::Launchd;

use crate::launchd::{
    Guard, UNIT_DIR_EXCLUSIVE, enabled_path, is_loaded, staging_path, timer_enabled_path, timer_staging_path,
};
use crate::support::cleanup::DirGuard;
use crate::support::cron_oracle::strict_next_runs_of;
use crate::support::demonstration::{
    Cli, DISARMED, Expect, LastResult, Manifest, Time, ZONE, assert_task_fields, daily_in_half_a_day, read_in_the_zone,
};
use crate::support::tasks::{Held, id_of, sh, status_with_result};
use crate::support::{self, Group, launchd_group};

const EVERY_MINUTE: &str = "* * * * *";

#[skuld::test(labels = [UNIT_DIR_EXCLUSIVE], serial = UNIT_DIR_EXCLUSIVE)]
fn the_demonstration_runs_through_the_binary(#[fixture(launchd_group)] _group: &Group) {
    let id = support::random_test_id();
    let task_id = id_of(&id);
    // `install` makes the directory of a `logs` file under `/Library/Logs`. Declared before the guard,
    // so it is removed after the job is gone.
    let logs_dir = PathBuf::from("/Library/Logs").join(&id);
    let _logs_dir = DirGuard::new(logs_dir.clone());
    let _guard = Guard(task_id.clone());
    let cli = Cli::new();
    let in_zone = Cli::in_zone(ZONE);
    let mut held = Held::new();
    let daily = daily_in_half_a_day();
    let mut manifest = Manifest::new(&id, held.command.clone(), &daily);

    // 1. Installed, the task is idle and disarmed.
    cli.install(&manifest);
    assert_task_fields(&cli.status_json(&id), &id, DISARMED);

    // 2. Armed. Enabled with a schedule 12 hours away, it is idle with the next run the cron names, never
    // run, in every form and at the offset of `TZ`. Nothing fires, so one read is the read.
    cli.daemon(&["enable", &id]);
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
    armed.assert_next_run("idle", |before, after| {
        strict_next_runs_of(&daily, &task_id, before, after).to_vec()
    });
    armed.assert_no_last_run();
    cli.daemon(&["disable", &id]);
    let disarmed = read_in_the_zone(&in_zone, &id);
    assert_task_fields(&disarmed.json, &id, DISARMED);
    disarmed.assert_state("idle");
    disarmed.assert_no_last_run();

    // 3. Fires. The same id, every minute, enabled: the schedule fires with nothing started by the test,
    // and the program announces itself. The run is held, so any later fire finds the job running and
    // starts nothing.
    assert_eq!(held.begun(), 0, "step 2's `enable` started a run");
    manifest.schedule = EVERY_MINUTE.to_string();
    cli.install(&manifest);
    let enabling_at = SystemTime::now();
    cli.daemon(&["enable", &id]);
    assert_eq!(held.wait_started(), 1, "the run is not the first");
    // The schedule fires at a minute boundary, so a run that began earlier than the first boundary after
    // `enable` began was started by `enable`.
    let [first_fire, _] = strict_next_runs_of(EVERY_MINUTE, &task_id, enabling_at, SystemTime::now());
    let started_at = held.started_at();
    assert!(
        started_at >= first_fire,
        "the run began at {started_at:?}, before the first fire {first_fire:?} after `enable` began at {enabling_at:?}"
    );
    // Running, with a pid and the next fire of the armed schedule. It has never exited, so there is no
    // result.
    let running = read_in_the_zone(&in_zone, &id);
    assert_task_fields(
        &running.json,
        &id,
        Expect {
            state: "running",
            enabled: true,
            pid: Time::Present,
            last_result: LastResult::Null,
            last_run: Time::Null,
            next_run: Time::Present,
        },
    );
    running.assert_next_run("running", |before, after| {
        strict_next_runs_of(EVERY_MINUTE, &task_id, before, after).to_vec()
    });
    running.assert_no_last_run();
    // No fire can come between the end of the run and the reads that follow it.
    cli.daemon(&["disable", &id]);
    held.release();
    status_with_result(&Launchd::new(), &task_id);
    let finished = read_in_the_zone(&in_zone, &id);
    assert_task_fields(
        &finished.json,
        &id,
        Expect {
            state: "idle",
            enabled: false,
            pid: Time::Null,
            last_result: LastResult::Code(0),
            last_run: Time::Null,
            next_run: Time::Null,
        },
    );
    finished.assert_state("idle");
    finished.assert_no_last_run();

    // 4. `run` shows the output and exits with the run's result. The same id, installed again with another
    // command and a `logs` file: a disarmed task is replaced.
    manifest.command = sh("echo a-known-line; exit 7");
    manifest.logs = Some(logs_dir.join("task.log"));
    cli.install(&manifest);
    let run = cli.run(&["daemon", "run", &id]);
    assert_eq!(run.code, Some(7), "{run}");
    assert_eq!(run.stdout, "a-known-line\n", "{run}");

    // 4b. Without a `logs` file launchd keeps no output: `run` still returns the program's exit, and says
    // on stdout why it shows nothing.
    manifest.command = sh("echo not-shown; exit 4");
    manifest.logs = None;
    cli.install(&manifest);
    let run = cli.run(&["daemon", "run", &id]);
    assert_eq!(run.code, Some(4), "{run}");
    assert_eq!(
        run.stdout,
        format!("note: `{id}` has no `logs` file, so its output is not shown; waiting for the run to end\n"),
        "{run}"
    );

    // 5. `uninstall` removes it.
    cli.daemon(&["uninstall", &id]);
    cli.assert_not_installed(&id);
    for path in [
        staging_path(&id),
        enabled_path(&id),
        timer_staging_path(&id),
        timer_enabled_path(&id),
    ] {
        assert!(
            std::fs::symlink_metadata(&path).is_err(),
            "{} is left behind",
            path.display()
        );
    }
    for label in [id.clone(), format!("{id}@timer")] {
        assert!(!is_loaded(&label), "{label} is still loaded");
    }
}
