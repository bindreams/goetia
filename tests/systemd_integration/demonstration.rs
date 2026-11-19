//! The owner's demonstration on systemd, through the `goetia` binary: a task with a cron `schedule` is
//! installed and enabled, fires by itself, shows its output through `run`, reports its last result and
//! next run through `status`, and is removed by `uninstall`.
//!
//! The platform-neutral half (the manifest, the binary, the document) is `support::demonstration`.

use std::path::PathBuf;
use std::time::{Duration, SystemTime};

use serde_json::Value;

use crate::linux::{timer_path, timer_wants_symlink, unit_path};
use crate::support::cron_oracle::assert_next_run_time;
use crate::support::demonstration::{
    Cli, Expect, LastResult, Manifest, Time, ZONE, ZONE_OFFSET, assert_task_fields, instant, text_field,
};
use crate::support::tasks::{Held, id_of, sh};
use crate::support::{self, Group, ServiceGuard, kill, systemd_group};

const SCHEDULE: &str = "* * * * *";

/// The task's entry, once its newest run has a result.
fn status_once_it_has_a_result(cli: &Cli, id: &str) -> Value {
    let mut found = Value::Null;
    kill::until(|| {
        found = cli.status_json(id);
        !found["last-result"].is_null()
    });
    found
}

/// Every form that prints a task's times prints them at the offset of the child's `TZ`, and the instants
/// are the ones the backend knows: `next-run` is a minute the cron names, and `last-run` is in the minute
/// `scheduled` names (given where the task has a `last_run`). `last_run` and `next_run` say which of the two the
/// task has at this step. Returns the `status --json` entry. CI hosts run in UTC, so this is the one place a
/// caller of the system zone is seen not to pass UTC.
fn read_times_in_the_zone(id: &str, scheduled: Option<SystemTime>, last_run: Time, next_run: Time) -> Value {
    let zoned = Cli::in_zone(ZONE);
    let task_id = id_of(id);

    let before = SystemTime::now();
    let json = zoned.status_json(id);
    let text = zoned.status_text(id);
    let listed = zoned.listed(id);
    let after = SystemTime::now();

    // The text form writes `-` for a time it has none of.
    let field = |entry: &Value, key: &str| entry[key].as_str().map(str::to_string);
    let word = |key: &str| Some(text_field(&text, key).to_string()).filter(|word| word != "-");
    let forms = [
        ("status --json", field(&json, "last-run"), field(&json, "next-run")),
        ("status", word("last-run"), word("next-run")),
        ("list --json", field(&listed, "last-run"), field(&listed, "next-run")),
    ];
    for (form, shown_last_run, shown_next_run) in forms {
        assert_eq!(
            shown_last_run.is_some(),
            last_run == Time::Present,
            "{form}: {json} / {text} / {listed}"
        );
        assert_eq!(
            shown_next_run.is_some(),
            next_run == Time::Present,
            "{form}: {json} / {text} / {listed}"
        );
        for time in shown_last_run.iter().chain(&shown_next_run) {
            assert!(time.ends_with(ZONE_OFFSET), "{form}: `{time}` is not at {ZONE_OFFSET}");
        }
        if let Some(time) = shown_last_run {
            let fired = instant(&time);
            let scheduled = scheduled.expect("a task with a last run has a scheduled minute");
            assert!(
                fired >= scheduled && fired < scheduled + Duration::from_secs(60),
                "{form}: last-run {fired:?} is not in the minute of {scheduled:?}"
            );
        }
        if let Some(time) = shown_next_run {
            assert_next_run_time(Some(instant(&time)), SCHEDULE, &task_id, before, after);
        }
    }
    json
}

#[skuld::test]
fn the_demonstration_runs_through_the_binary(#[fixture(systemd_group)] _group: &Group) {
    let id = support::random_test_id();
    let _guard = ServiceGuard::new(&id);
    let cli = Cli::new();
    let mut held = Held::new();
    let mut manifest = Manifest::new(&id, held.command.clone(), SCHEDULE);

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

    // 2. Enabled, it has a next run, the minute the cron names, in every form and at the offset of `TZ`.
    cli.daemon(&["enable", &id]);
    let armed = read_times_in_the_zone(&id, None, Time::Null, Time::Present);
    assert_task_fields(
        &armed,
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
    let scheduled = instant(armed["next-run"].as_str().expect("a next run"));

    // 3. The schedule fires with nothing started by the test: the program announces itself. systemd has no
    // next run for a timer whose service is running. The run is held, so any later fire is dropped by the
    // manager, and the schedule is disarmed while it is.
    held.wait_started();
    assert_task_fields(
        &cli.status_json(&id),
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
    read_times_in_the_zone(&id, Some(scheduled), Time::Present, Time::Null);
    cli.daemon(&["disable", &id]);
    held.release();
    let finished = status_once_it_has_a_result(&cli, &id);
    assert_task_fields(
        &finished,
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
    let fired = instant(finished["last-run"].as_str().expect("a last run"));
    assert!(
        fired >= scheduled && fired < scheduled + Duration::from_secs(60),
        "last-run {fired:?} is not in the minute of {scheduled:?}"
    );

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
