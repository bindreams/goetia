//! The owner's demonstration on systemd, through the `goetia` binary: a task with a cron `schedule` is
//! installed and enabled, fires by itself, shows its output through `run`, reports its last result and
//! next run through `status`, and is removed by `uninstall`.
//!
//! The platform-neutral half (the manifest, the binary, the document) is `support::demonstration`.

use std::path::PathBuf;
use std::time::{Duration, SystemTime};

use goetia::backend::systemd::manager::Systemd;
use serde_json::Value;

use crate::linux::{UNIT_DIR_EXCLUSIVE, timer_path, timer_wants_symlink, unit_path};
use crate::support::cron_oracle::assert_next_run_time;
use crate::support::demonstration::{
    Cli, Expect, LastResult, Manifest, Time, ZONE, ZONE_OFFSET, assert_task_fields, instant, text_field, text_state,
};
use crate::support::tasks::{Held, id_of, sh, status_with_result};
use crate::support::{self, Group, ServiceGuard, systemd_group};

const SCHEDULE: &str = "* * * * *";

/// What one form of the task's status reported, read with the child's `TZ` set to [`ZONE`]. A time is
/// `None` where the form shows none (the text form writes `-`).
struct Read {
    form: &'static str,
    state: String,
    last_run: Option<String>,
    next_run: Option<String>,
}

/// The three forms that print a task's times, read one after another, and the clock around them.
struct Zoned {
    json: Value,
    reads: Vec<Read>,
    before: SystemTime,
    after: SystemTime,
}

/// Reads the task through `status --json`, `status` and `list --json`, each at the offset of the child's
/// `TZ`. CI hosts run in UTC, so this is the one place a caller of the system zone is seen not to pass UTC.
fn read_in_the_zone(id: &str) -> Zoned {
    let zoned = Cli::in_zone(ZONE);

    let before = SystemTime::now();
    let json = zoned.status_json(id);
    let text = zoned.status_text(id);
    let listed = zoned.listed(id);
    let after = SystemTime::now();

    // The text form writes `-` for a time it has none of.
    let field = |entry: &Value, key: &str| entry[key].as_str().map(str::to_string);
    let word = |key: &str| Some(text_field(&text, key).to_string()).filter(|word| word != "-");
    let reads = vec![
        Read {
            form: "status --json",
            state: json["state"].as_str().expect("a state").to_string(),
            last_run: field(&json, "last-run"),
            next_run: field(&json, "next-run"),
        },
        Read {
            form: "status",
            state: text_state(&text).to_string(),
            last_run: word("last-run"),
            next_run: word("next-run"),
        },
        Read {
            form: "list --json",
            state: listed["state"].as_str().expect("a state").to_string(),
            last_run: field(&listed, "last-run"),
            next_run: field(&listed, "next-run"),
        },
    ];
    Zoned {
        json,
        reads,
        before,
        after,
    }
}

impl Zoned {
    /// Every `next-run` shown is at the offset of `TZ` and is a minute the cron names. Returns the
    /// first one shown.
    fn judge_next_runs(&self, id: &str) -> Option<SystemTime> {
        let task_id = id_of(id);
        let mut first = None;
        for read in &self.reads {
            if let Some(time) = &read.next_run {
                assert!(
                    time.ends_with(ZONE_OFFSET),
                    "{}: `{time}` is not at {ZONE_OFFSET}",
                    read.form
                );
                let next = instant(time);
                assert_next_run_time(Some(next), SCHEDULE, &task_id, self.before, self.after);
                first.get_or_insert(next);
            }
        }
        first
    }

    /// Every form shows a `last-run`, at the offset of `TZ`, in `[from, to)`.
    fn judge_last_runs(&self, from: SystemTime, to: SystemTime) {
        for read in &self.reads {
            let time = read
                .last_run
                .as_ref()
                .unwrap_or_else(|| panic!("{}: a finished run has a last-run: {}", read.form, self.json));
            assert!(
                time.ends_with(ZONE_OFFSET),
                "{}: `{time}` is not at {ZONE_OFFSET}",
                read.form
            );
            let fired = instant(time);
            assert!(
                fired >= from && fired < to,
                "{}: last-run {fired:?} is not in [{from:?}, {to:?})",
                read.form
            );
        }
    }
}

/// `* * * * *` fires at the next minute, which may land before or after the reads that follow `enable`.
/// Each read is judged by the state it reports: `idle` is armed (a next run, no last run); `running` is
/// the held first run in flight, where systemd has no next run and `last-run` is not yet known to be
/// readable. The reads need not agree on the state.
fn judge_armed_or_running(zoned: &Zoned) {
    for read in &zoned.reads {
        match read.state.as_str() {
            "idle" => {
                assert!(
                    read.next_run.is_some(),
                    "{}: idle and armed has a next-run: {}",
                    read.form,
                    zoned.json
                );
                assert!(
                    read.last_run.is_none(),
                    "{}: no run has begun: {}",
                    read.form,
                    zoned.json
                );
            }
            "running" => assert!(
                read.next_run.is_none(),
                "{}: a task in flight has no next-run: {}",
                read.form,
                zoned.json
            ),
            other => panic!("{}: `{other}` is neither idle nor running: {}", read.form, zoned.json),
        }
    }
    let in_flight = zoned.json["state"] == "running";
    assert_task_fields(
        &zoned.json,
        zoned.json["id"].as_str().expect("an id"),
        Expect {
            state: if in_flight { "running" } else { "idle" },
            enabled: true,
            pid: if in_flight { Time::Present } else { Time::Null },
            last_result: LastResult::Null,
            last_run: if in_flight { Time::Any } else { Time::Null },
            next_run: if in_flight { Time::Null } else { Time::Present },
        },
    );
}

#[skuld::test(labels = [UNIT_DIR_EXCLUSIVE], serial = UNIT_DIR_EXCLUSIVE)]
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

    // 2. Enabled, it is armed with a next run, the minute the cron names, in every form and at the offset
    // of `TZ`; or, where the first minute came first, its held first run is in flight.
    let enabled_at = SystemTime::now();
    cli.daemon(&["enable", &id]);
    let armed = read_in_the_zone(&id);
    judge_armed_or_running(&armed);
    let scheduled = armed.judge_next_runs(&id);

    // 3. The schedule fires with nothing started by the test: the program announces itself. systemd has no
    // next run for a timer whose service is running, and the run is held, so any later fire is dropped by
    // the manager. `last-run` is not asserted while the run is in flight: journald need not have the
    // `Starting` entry it comes from yet.
    held.wait_started();
    assert_task_fields(
        &cli.status_json(&id),
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
    cli.daemon(&["disable", &id]);
    held.release();
    status_with_result(&Systemd::new(), &id_of(&id));
    let finished = read_in_the_zone(&id);
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
    for read in &finished.reads {
        assert_eq!(read.state, "idle", "{}: {}", read.form, finished.json);
        assert!(read.next_run.is_none(), "{}: {}", read.form, finished.json);
    }
    // The run began in the minute `scheduled` names; where the reads after `enable` met it in flight, in
    // some minute after `enable`.
    match scheduled {
        Some(minute) => finished.judge_last_runs(minute, minute + Duration::from_secs(60)),
        None => finished.judge_last_runs(enabled_at, finished.after),
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
