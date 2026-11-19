//! The owner's demonstration on systemd, through the `goetia` binary: a task with a cron `schedule` is
//! installed and enabled, fires by itself, shows its output through `run`, reports its last result and
//! next run through `status`, and is removed by `uninstall`.
//!
//! "Armed" and "fired" are two installs of the one id, so no read can straddle a fire: the first has a
//! daily schedule whose time has just passed (its next fire is a day away), the second is `* * * * *`.
//!
//! The platform-neutral half (the manifest, the binary, the document) is `support::demonstration`.

use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use goetia::backend::systemd::manager::Systemd;
use serde_json::Value;

use crate::linux::{UNIT_DIR_EXCLUSIVE, started_invocations, timer_path, timer_wants_symlink, unit_path};
use crate::support::cron_oracle::expected_next_runs;
use crate::support::demonstration::{
    Cli, Expect, LastResult, Manifest, Time, ZONE, ZONE_OFFSET, assert_task_fields, instant, text_field, text_state,
};
use crate::support::tasks::{Held, id_of, sh, status_with_result};
use crate::support::{self, Group, ServiceGuard, kill, systemd_group};

const EVERY_MINUTE: &str = "* * * * *";

/// A daily schedule whose time was two minutes ago, local time: its next fire is a day away.
fn daily_just_passed() -> String {
    let past = SystemTime::now() - Duration::from_secs(120);
    let secs = past
        .duration_since(UNIX_EPOCH)
        .expect("a time after the epoch")
        .as_secs();
    let time = libc::time_t::try_from(secs).expect("a time_t");
    // SAFETY: an all-zero `tm` is a valid value, and `localtime_r` fills the one it is given.
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    // SAFETY: both pointers are valid for the call.
    let filled = unsafe { libc::localtime_r(&time, &mut tm) };
    assert!(!filled.is_null(), "localtime_r failed for {secs}");
    format!("{} {} * * *", tm.tm_min, tm.tm_hour)
}

/// What one form of the task's status reported, read with the child's `TZ` set to [`ZONE`]. A time is
/// `None` where the form shows none (the text form writes `-`).
#[derive(Debug)]
struct Read {
    form: &'static str,
    state: String,
    last_run: Option<String>,
    next_run: Option<String>,
}

impl Read {
    /// Every time shown is at the offset of `TZ`. CI hosts run in UTC, so this is where a caller of the
    /// system zone is seen not to pass UTC.
    fn at_the_zone(self) -> Read {
        for time in [&self.last_run, &self.next_run].into_iter().flatten() {
            assert!(
                time.ends_with(ZONE_OFFSET),
                "`{time}` is not at {ZONE_OFFSET}: {self:?}"
            );
        }
        self
    }

    fn of_entry(form: &'static str, entry: &Value) -> Read {
        let field = |key: &str| entry[key].as_str().map(str::to_string);
        Read {
            form,
            state: entry["state"].as_str().expect("a state").to_string(),
            last_run: field("last-run"),
            next_run: field("next-run"),
        }
        .at_the_zone()
    }

    fn of_text(line: &str) -> Read {
        // The text form writes `-` for a time it has none of.
        let word = |key: &str| Some(text_field(line, key).to_string()).filter(|word| word != "-");
        Read {
            form: "status",
            state: text_state(line).to_string(),
            last_run: word("last-run"),
            next_run: word("next-run"),
        }
        .at_the_zone()
    }
}

/// The three forms that print a task's times, each read once, at the offset of the child's `TZ`.
#[derive(Debug)]
struct Zoned {
    /// The `status --json` entry, read by `json_read`.
    json: Value,
    json_read: Read,
    text_read: Read,
    list_read: Read,
}

fn read_in_the_zone(in_zone: &Cli, id: &str) -> Zoned {
    let json = in_zone.status_json(id);
    let text = in_zone.status_text(id);
    let listed = in_zone.listed(id);
    Zoned {
        json_read: Read::of_entry("status --json", &json),
        text_read: Read::of_text(&text),
        list_read: Read::of_entry("list --json", &listed),
        json,
    }
}

impl Zoned {
    fn reads(&self) -> [&Read; 3] {
        [&self.json_read, &self.text_read, &self.list_read]
    }

    /// Every form is in `state` with no `next-run`, or with the one `next_run`.
    #[track_caller]
    fn assert_state(&self, state: &str, next_run: Option<SystemTime>) {
        for read in self.reads() {
            assert_eq!(read.state, state, "{self:?}");
            assert_eq!(read.next_run.as_deref().map(instant), next_run, "{self:?}");
        }
    }
}

#[skuld::test(labels = [UNIT_DIR_EXCLUSIVE], serial = UNIT_DIR_EXCLUSIVE)]
fn the_demonstration_runs_through_the_binary(#[fixture(systemd_group)] _group: &Group) {
    let id = support::random_test_id();
    let _guard = ServiceGuard::new(&id);
    let cli = Cli::new();
    let in_zone = Cli::in_zone(ZONE);
    let mut held = Held::new();
    let daily = daily_just_passed();
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

    // 2. Armed. Enabled with a schedule whose time has passed, it is idle with the next run the cron names
    // (a day away), never run, in every form and at the offset of `TZ`. Nothing fires, so one read is the
    // read.
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
    let [_, first, last] = expected_next_runs(EVERY_MINUTE, &id_of(&id), enabling_at, SystemTime::now());
    held.wait_started();
    assert_task_fields(
        &in_zone.status_json(&id),
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
    // `last-run` is read from the journal, which need not hold the run's `Starting` entry yet: wait for it
    // (the entry `goetia` matches), and then one read must show the `last-run`.
    kill::until(|| !started_invocations(&id).is_empty());
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
        let fired = instant(time);
        assert!(
            fired >= first && fired < last + Duration::from_secs(60),
            "{}: last-run {fired:?} is not in [{first:?}, {last:?} + 60s)",
            read.form
        );
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
