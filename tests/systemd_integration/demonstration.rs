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
use crate::support::cron_oracle::{assert_next_run_time, expected_next_runs};
use crate::support::demonstration::{
    Cli, Expect, LastResult, Manifest, Time, ZONE, ZONE_OFFSET, assert_task_fields, instant, text_field, text_state,
};
use crate::support::tasks::{Held, id_of, sh, status_with_result};
use crate::support::{self, Group, ServiceGuard, systemd_group};

const SCHEDULE: &str = "* * * * *";

/// What one form of the task's status reported, read with the child's `TZ` set to [`ZONE`], and the clock
/// around that read. A time is `None` where the form shows none (the text form writes `-`).
#[derive(Debug)]
struct Read {
    form: &'static str,
    state: String,
    last_run: Option<String>,
    next_run: Option<String>,
    before: SystemTime,
    after: SystemTime,
}

impl Read {
    /// Every time shown is at the offset of `TZ`. CI hosts run in UTC, so this is where a caller of the
    /// system zone is seen not to pass UTC. Checked once, for every read.
    #[track_caller]
    fn at_the_zone(self) -> Read {
        for time in [&self.last_run, &self.next_run].into_iter().flatten() {
            assert!(
                time.ends_with(ZONE_OFFSET),
                "`{time}` is not at {ZONE_OFFSET}: {self:?}"
            );
        }
        self
    }

    /// A snapshot that holds until the test changes the task: armed and not yet fired, or the held first
    /// run in flight. Any other is a read that straddled the fire (`idle` with no next run, or with a
    /// last run), which `systemd`'s three reads of one task cannot rule out.
    fn is_stable(&self) -> bool {
        match self.state.as_str() {
            "idle" => self.next_run.is_some() && self.last_run.is_none(),
            "running" => true,
            _ => false,
        }
    }
}

/// `read` again until `wanted` holds of what it read. No cap: the condition is the real one.
fn settle<T>(wanted: impl Fn(&Read) -> bool, mut read: impl FnMut() -> (Read, T)) -> (Read, T) {
    loop {
        let (snapshot, shown) = read();
        if wanted(&snapshot) {
            return (snapshot, shown);
        }
    }
}

/// `daemon status <id> --json`, with its entry.
fn read_json(cli: &Cli, id: &str) -> (Read, Value) {
    let before = SystemTime::now();
    let entry = cli.status_json(id);
    let after = SystemTime::now();
    (entry_read("status --json", &entry, before, after), entry)
}

/// `daemon list --json`, with the task's entry.
fn read_listed(cli: &Cli, id: &str) -> (Read, Value) {
    let before = SystemTime::now();
    let entry = cli.listed(id);
    let after = SystemTime::now();
    (entry_read("list --json", &entry, before, after), entry)
}

/// `daemon status <id>`, with its line.
fn read_text(cli: &Cli, id: &str) -> (Read, String) {
    let before = SystemTime::now();
    let line = cli.status_text(id);
    let after = SystemTime::now();
    // The text form writes `-` for a time it has none of.
    let word = |key: &str| Some(text_field(&line, key).to_string()).filter(|word| word != "-");
    let read = Read {
        form: "status",
        state: text_state(&line).to_string(),
        last_run: word("last-run"),
        next_run: word("next-run"),
        before,
        after,
    };
    (read.at_the_zone(), line)
}

fn entry_read(form: &'static str, entry: &Value, before: SystemTime, after: SystemTime) -> Read {
    let field = |key: &str| entry[key].as_str().map(str::to_string);
    Read {
        form,
        state: entry["state"].as_str().expect("a state").to_string(),
        last_run: field("last-run"),
        next_run: field("next-run"),
        before,
        after,
    }
    .at_the_zone()
}

/// The three forms that print a task's times, each read until `wanted` holds of it, at the offset of the
/// child's `TZ`.
struct Zoned {
    /// The `status --json` entry, read by `json_read`.
    json: Value,
    json_read: Read,
    text_read: Read,
    list_read: Read,
}

fn read_in_the_zone(id: &str, wanted: impl Fn(&Read) -> bool) -> Zoned {
    let cli = Cli::in_zone(ZONE);
    let (json_read, json) = settle(&wanted, || read_json(&cli, id));
    let (text_read, _) = settle(&wanted, || read_text(&cli, id));
    let (list_read, _) = settle(&wanted, || read_listed(&cli, id));
    Zoned {
        json,
        json_read,
        text_read,
        list_read,
    }
}

impl Zoned {
    fn reads(&self) -> [&Read; 3] {
        [&self.json_read, &self.text_read, &self.list_read]
    }

    /// The forms other than `status --json`, whose entry `assert_task_fields` judges.
    fn others(&self) -> [&Read; 2] {
        [&self.text_read, &self.list_read]
    }

    /// Every `next-run` shown is a minute the cron names, near the clock at its own read.
    fn judge_next_runs(&self, id: &str) {
        let task_id = id_of(id);
        for read in self.reads() {
            if let Some(time) = &read.next_run {
                assert_next_run_time(Some(instant(time)), SCHEDULE, &task_id, read.before, read.after);
            }
        }
    }

    /// Every form shows a `last-run` in `[from, to)`.
    fn judge_last_runs(&self, from: SystemTime, to: SystemTime) {
        for read in self.reads() {
            let time = read
                .last_run
                .as_ref()
                .unwrap_or_else(|| panic!("a finished run has a last-run: {read:?}"));
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
/// The reads are settled (`Read::is_stable`): each is `idle` and armed, or `running`, the held first run
/// in flight, where systemd has no next run. The forms need not agree on the state.
fn judge_armed_or_running(zoned: &Zoned) {
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
    for read in zoned.others() {
        if read.state == "running" {
            assert!(read.next_run.is_none(), "a task in flight has no next-run: {read:?}");
        }
    }
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
    // of `TZ`; or, where the first minute came first, its held first run is in flight. The first fire is in
    // the minute after `enable` began, or the one after it returned.
    let enabled_at = SystemTime::now();
    cli.daemon(&["enable", &id]);
    let enabled_after = SystemTime::now();
    let [_, first_minute, last_minute] = expected_next_runs(SCHEDULE, &id_of(&id), enabled_at, enabled_after);
    let armed = read_in_the_zone(&id, Read::is_stable);
    judge_armed_or_running(&armed);
    armed.judge_next_runs(&id);

    // 3. The schedule fires with nothing started by the test: the program announces itself. systemd has no
    // next run for a timer whose service is running, and the run is held, so any later fire is dropped by
    // the manager. journald need not have the `Starting` entry `last-run` comes from yet: it is waited for.
    held.wait_started();
    let zoned = Cli::in_zone(ZONE);
    settle(
        |read| read.last_run.is_some(),
        || {
            let (read, entry) = read_json(&zoned, &id);
            assert_task_fields(
                &entry,
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
            (read, entry)
        },
    );
    cli.daemon(&["disable", &id]);
    held.release();
    status_with_result(&Systemd::new(), &id_of(&id));
    let finished = read_in_the_zone(&id, |_| true);
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
    for read in finished.others() {
        assert_eq!(read.state, "idle", "{read:?}");
        assert!(read.next_run.is_none(), "{read:?}");
    }
    // The run began in the first minute the cron named after `enable`.
    finished.judge_last_runs(first_minute, last_minute + Duration::from_secs(60));

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
