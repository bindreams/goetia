//! Tasks on a real systemd: arming, the timer firing, `run` following a run through the journal,
//! `start`, `stop`, `restart`, and what the journal and `journalctl` do that the unit tests assume.
//!
//! The installing, arming, retyping and uninstalling of an idle task is `linux.rs`'s ("Tasks"); this
//! file adds the runs. A task that has to be caught in flight is [`Held`]: it announces itself and waits
//! on a FIFO, so a test waits on the program, never on a time. The Linux lane runs one test at a time;
//! the tests that change journald's configuration share the `JOURNALD_CONFIG` label, so that no two of
//! them overlap where tests run in parallel.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use goetia::Error;
use goetia::backend::systemd::manager::Systemd;
use goetia::cli::ExitCode;
use goetia::decide::Outcome;
use goetia::manager::{Budget, Exit, Installed, ServiceManager, State, Status};
use goetia::spec::{DaemonSpec, HashSeed, Schedule, TaskSpec, User};
use serde_json::Value;

use crate::linux::{CreatedRoot, UNIT_DIR_EXCLUSIVE, create_root, exiting, main_pid, timer_path, timer_wants_symlink};
use crate::support::cleanup::{remove_or_warn, warn_left_behind};
use crate::support::cron_oracle::assert_next_run;
use crate::support::tasks::{
    Held, Lines, assert_same_text, id_of, run_task, sh, status_with_result, task_status, with_logs,
};
use crate::support::{self, Group, ServiceGuard, cmd, cron_oracle, kill, systemd_group};

/// Held by the tests that rewrite journald's configuration (and restart it) while they run.
#[skuld::label]
pub(crate) const JOURNALD_CONFIG: skuld::Label;

/// "Starting `<unit>`...": the manager began a run.
const STARTING: &str = "7d4958e842da4a758f6c1cdc7b36dcc5";

// Fixtures ============================================================================================================

fn unit_of(id: &str) -> String {
    format!("{id}.service")
}

fn task(id: &str, command: Vec<String>, schedule: Option<&str>) -> DaemonSpec {
    goetia::testing::task(id, command, schedule)
}

/// Stops `unit` on drop, so a thread waiting for a held run can end when its test panics.
struct StopOnDrop(String);

impl Drop for StopOnDrop {
    fn drop(&mut self) {
        drop(cmd::try_run("systemctl", &["stop", &self.0]));
    }
}

/// A journald drop-in for as long as it lives, and the restart that makes journald read it.
/// Registered before the file is written; removed, and journald restarted again, on drop.
struct JournaldConf {
    path: PathBuf,
    _root: CreatedRoot,
}

/// Restarts journald. Its units allow 5 starts in 10 seconds and then stay failed, which would leave
/// the host with no journal; the restart counters are reset first, as a restart of ours is not a
/// start the limit exists to stop.
fn restart_journald() -> Option<String> {
    cmd::run_cleanup(
        "systemctl",
        &[
            &[
                "reset-failed",
                "systemd-journald.service",
                "systemd-journald.socket",
                "systemd-journald-dev-log.socket",
            ],
            &["restart", "systemd-journald.service"],
        ],
        &[],
    )
}

impl JournaldConf {
    const DIR: &'static str = "/etc/systemd/journald.conf.d";

    fn set(settings: &str) -> JournaldConf {
        let root = create_root(Path::new(Self::DIR));
        let path = Path::new(Self::DIR).join("zz-goetia-test.conf");
        let conf = JournaldConf { path, _root: root };
        fs::write(
            &conf.path,
            format!(
                "[Journal]
{settings}"
            ),
        )
        .expect("write the journald drop-in");
        if let Some(problem) = restart_journald() {
            panic!("restart journald: {problem}");
        }
        conf
    }

    /// No rate limit: every line a run writes reaches the journal.
    fn unlimited() -> JournaldConf {
        Self::set(
            "RateLimitBurst=0
",
        )
    }
}

impl Drop for JournaldConf {
    fn drop(&mut self) {
        remove_or_warn(&self.path, false);
        warn_left_behind(restart_journald());
    }
}

/// The entries `journalctl` holds for `matches`, oldest first, as JSON objects.
fn journal(matches: &[&str]) -> Vec<Value> {
    cmd::run("journalctl", &["--sync"]).expect_ok();
    let mut args = vec!["-a", "-o", "json", "--no-pager"];
    args.extend_from_slice(matches);
    cmd::run("journalctl", &args)
        .expect_ok()
        .stdout
        .lines()
        .map(|line| serde_json::from_str(line).unwrap_or_else(|e| panic!("journalctl JSON `{line}`: {e}")))
        .collect()
}

fn field<'a>(entry: &'a Value, name: &str) -> Option<&'a str> {
    entry.get(name)?.as_str()
}

/// The invocation of every run of `id`'s unit the manager started, oldest first.
fn started_invocations(id: &str) -> Vec<String> {
    let unit = format!("UNIT={}", unit_of(id));
    journal(&[&unit, &format!("MESSAGE_ID={STARTING}")])
        .iter()
        .map(|entry| {
            field(entry, "INVOCATION_ID")
                .expect("a Starting entry has an invocation")
                .to_string()
        })
        .collect()
}

/// `(invocation, EXIT_CODE, EXIT_STATUS)` of every "Main process exited" entry of `id`'s unit.
fn exited(id: &str) -> Vec<(String, String, String)> {
    let unit = format!("UNIT={}", unit_of(id));
    journal(&[&unit, "MESSAGE_ID=98e322203f7a4ed290d09fe03c09fe15"])
        .iter()
        .map(|entry| {
            let get = |name| field(entry, name).unwrap_or_default().to_string();
            (get("INVOCATION_ID"), get("EXIT_CODE"), get("EXIT_STATUS"))
        })
        .collect()
}

/// `seq 1 <count>`'s output.
fn counted(count: u32) -> Vec<u8> {
    (1..=count).flat_map(|n| format!("{n}\n").into_bytes()).collect()
}

/// The cursor of the newest stdout entry of `id`'s unit, or `None` for a unit with none: what a run
/// that has not started yet writes is after it.
fn stdout_end(id: &str) -> Option<String> {
    let unit = format!("_SYSTEMD_UNIT={}", unit_of(id));
    journal(&[&unit, "_TRANSPORT=stdout", "-r", "-n", "1"])
        .first()
        .map(|entry| field(entry, "__CURSOR").expect("an entry has a cursor").to_string())
}

/// The stdout lines of the newest run of `id`'s unit, written after `after` ([`stdout_end`] from
/// before the run), read by the one query `run` reads by (`_SYSTEMD_UNIT=` + `_TRANSPORT=stdout`),
/// from the run's own `goetia-run <INVOCATION_ID>` marker on, without the marker. Not `_STREAM_ID=`: a
/// rotation duplicate indexed under the unit's fields only would show here once what `run` shows
/// twice.
///
/// Panics when the entries after `after` are not exactly the run's own stream with its marker first.
///
/// Covers newline-terminated text lines only: an entry with no `MESSAGE` or with a `_LINE_BREAK`
/// panics.
fn run_stream_text(id: &str, after: Option<&str>) -> Vec<u8> {
    let invocation = started_invocations(id).pop().expect("a run started");
    let marker = format!("goetia-run {invocation}");
    let unit = format!("_SYSTEMD_UNIT={}", unit_of(id));
    let from = after.map(|cursor| format!("--after-cursor={cursor}"));
    let mut query = vec![unit.as_str(), "_TRANSPORT=stdout"];
    query.extend(from.as_deref());
    let entries = journal(&query);
    let first = entries.first().expect("a run wrote a stdout line");
    assert_eq!(
        field(first, "MESSAGE"),
        Some(marker.as_str()),
        "the first stdout entry after the run began is not its marker: a late line of an earlier run, which `run` may have missed"
    );
    let stream = field(first, "_STREAM_ID").expect("a stdout entry has a stream");
    entries[1..]
        .iter()
        .flat_map(|entry| {
            assert_eq!(
                field(entry, "_STREAM_ID"),
                Some(stream),
                "a stdout entry after the run began is of another stream: a late line of an earlier run, which `run` may have missed: {entry}"
            );
            assert!(
                entry.get("_LINE_BREAK").is_none(),
                "a line this helper does not cover: {entry}"
            );
            let mut line = field(entry, "MESSAGE")
                .expect("a line of the run is text")
                .as_bytes()
                .to_vec();
            line.push(b'\n');
            line
        })
        .collect()
}

/// Panics unless `got` holds every line of `want`, in order. Lines of its own besides are fine: the
/// journal's duplicates are not `run`'s to remove.
fn assert_holds_in_order(run: usize, got: &[u8], want: &[u8]) {
    let mut wanted = want.split_inclusive(|&b| b == b'\n').peekable();
    for line in got.split_inclusive(|&b| b == b'\n') {
        if wanted.peek() == Some(&line) {
            wanted.next();
        }
    }
    let missing = wanted.next();
    assert!(
        missing.is_none(),
        "run {run}: the journal lacks {:?} (and what follows it)",
        missing.map(String::from_utf8_lossy)
    );
}

/// The pids of this process's children that are `journalctl`, a zombie among them. The test binary is
/// one test's process under nextest, so a child of its own is a child of this test.
fn journalctl_children() -> Vec<u32> {
    let me = std::process::id();
    fs::read_dir("/proc")
        .expect("read /proc")
        .flatten()
        .filter_map(|entry| {
            let pid: u32 = entry.file_name().to_str()?.parse().ok()?;
            let stat = fs::read_to_string(entry.path().join("stat")).ok()?;
            let (comm, rest) = stat[stat.find('(')? + 1..].rsplit_once(')')?;
            let parent: u32 = rest.split_whitespace().nth(1)?.parse().ok()?;
            (comm == "journalctl" && parent == me).then_some(pid)
        })
        .collect()
}

/// The local-time wall clock `year-month-day hour:minute` as a time, as the oracle reads it.
fn local_time(year: i32, month: i32, day: i32, hour: i32, minute: i32) -> SystemTime {
    // SAFETY: an all-zero `tm` is a valid value.
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    tm.tm_year = year - 1900;
    tm.tm_mon = month - 1;
    tm.tm_mday = day;
    tm.tm_hour = hour;
    tm.tm_min = minute;
    tm.tm_isdst = -1;
    // SAFETY: `tm` is valid for the call.
    let secs = unsafe { libc::mktime(&mut tm) };
    UNIX_EPOCH + Duration::from_secs(u64::try_from(secs).expect("a time after the epoch"))
}

fn parse_schedule(text: &str) -> Schedule {
    let id = id_of("oracle-walk");
    let seed = HashSeed {
        hostname: "testing",
        id: &id,
    };
    Schedule::parse(text, &seed).expect("a schedule")
}

/// A minute that matches at `before` is its own latest match, whether `before` is on it or inside it.
#[skuld::test]
fn the_latest_minute_at_or_before_includes_the_minute_of_before() {
    let schedule = parse_schedule("* * * * *");
    let minute = local_time(2026, 5, 20, 9, 41);

    for before in [minute, minute + Duration::from_secs(25)] {
        assert_eq!(cron_oracle::latest_minute_at_or_before(&schedule, before), minute);
    }
}

/// The walk back crosses months: the 31st at midnight, looked for from mid-April, is 31 March.
#[skuld::test]
fn the_latest_minute_at_or_before_walks_back_across_months() {
    let schedule = parse_schedule("0 0 31 * *");

    assert_eq!(
        cron_oracle::latest_minute_at_or_before(&schedule, local_time(2026, 4, 15, 12, 0)),
        local_time(2026, 3, 31, 0, 0)
    );
}

// Arming and the timer ================================================================================================

/// `list` reports a task as a task: its spec and a task status.
#[skuld::test(labels = [UNIT_DIR_EXCLUSIVE], serial = UNIT_DIR_EXCLUSIVE)]
fn a_listed_task_reports_a_task_status(#[fixture(systemd_group)] _group: &Group) {
    let id = support::random_test_id();
    let _guard = ServiceGuard::new(&id);
    let mgr = Systemd::new();
    mgr.install(&exiting(&id, 0), false).expect("install");

    let listed = mgr.list().expect("list");

    let status = listed
        .into_iter()
        .find_map(|entry| match entry {
            Installed::Ours { spec, status } if spec.common().id.as_str() == id => Some(status),
            _ => None,
        })
        .expect("the task is listed");
    let Status::Task(status) = status else {
        panic!("a task is listed with a task status: {status:?}");
    };
    assert_eq!(status.common.state, State::Idle);
}

/// The `OnCalendar` goetia renders fires at the fields the schedule resolves to: `next_run` is the
/// oracle's next minute for each shape (both day fields set (OR), steps, Sunday as 0 and as 7, lists).
/// The oracle shares goetia's parser, so this checks the rendering and systemd's reading of it, not the
/// parsing.
#[skuld::test]
fn the_rendered_on_calendar_fires_at_the_resolved_fields(#[fixture(systemd_group)] _group: &Group) {
    let mgr = Systemd::new();
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
        "0 12 1 */3 *",
    ] {
        let id = support::random_test_id();
        let _guard = ServiceGuard::new(&id);
        mgr.install(&task(&id, vec!["/bin/true".to_string()], Some(text)), false)
            .expect("install");
        let before = SystemTime::now();
        mgr.enable(&id_of(&id)).expect("enable");
        let status = task_status(&mgr, &id_of(&id));
        let after = SystemTime::now();

        assert_next_run(&status, text, &id_of(&id), before, after);
    }
}

/// Where systemd keeps the stamp of `id`'s timer: when the timer last elapsed, which a timer with
/// `Persistent=yes` reads when it starts, and starts the job at once if its schedule named a time since.
fn stamp_path(id: &str) -> PathBuf {
    PathBuf::from(support::SYSTEMD_STAMP_DIR).join(format!("stamp-{id}.timer"))
}

/// A daily schedule (cron text) at the local time `when` names, in `date -d` words.
fn daily_at(when: &str) -> String {
    let at = cmd::run("date", &["-d", when, "+%-M %-H"]).expect_ok().stdout;
    format!("{} * * *", at.trim())
}

/// Gives `id`'s timer a stamp from two hours ago: what it remembers of a last elapse, which a timer
/// with `Persistent=yes` reads when it starts, and starts the job at once if its schedule named a time
/// since.
fn plant_stale_stamp(id: &str) {
    cmd::run("touch", &["-d", "-2 hours", &stamp_path(id).to_string_lossy()]).expect_ok();
}

/// When systemd last elapsed `id`'s timer, as it holds it now: empty for a timer that remembers no
/// elapse. Starting a timer loads it from the stamp, and a run made up for a missed time sets it, so an
/// empty answer after an arm is the proof that neither happened, with no wait for a run that did not come.
fn last_trigger(id: &str) -> String {
    let shown = cmd::run(
        "systemctl",
        &["show", "--property=LastTriggerUSec", "--value", &format!("{id}.timer")],
    );
    shown.expect_ok().stdout.trim().to_string()
}

/// Arming a task again makes up no run its schedule named while goetia's `disable` had it disarmed: the
/// arm clears the timer's stamp first, and the timer then remembers no elapse and waits for the next
/// time its schedule names. The stamp is planted two hours old, and the schedule's time was a minute ago.
#[skuld::test]
fn arming_a_disabled_task_makes_up_no_run_for_the_time_it_was_disarmed(#[fixture(systemd_group)] _group: &Group) {
    let id = support::random_test_id();
    let _guard = ServiceGuard::new(&id);
    let task_id = id_of(&id);
    let mgr = Systemd::new();
    let schedule = daily_at("-1 min");
    mgr.install(&task(&id, vec!["/bin/true".to_string()], Some(&schedule)), false)
        .expect("install");
    mgr.enable(&task_id).expect("enable");
    mgr.disable(&task_id).expect("disable");
    plant_stale_stamp(&id);

    let before = SystemTime::now();
    mgr.enable(&task_id).expect("enable again");
    let status = task_status(&mgr, &task_id);
    let after = SystemTime::now();

    assert_eq!(
        last_trigger(&id),
        "",
        "the timer remembers no elapse: nothing was made up"
    );
    assert!(status.common.enabled, "{status:?}");
    assert_next_run(&status, &schedule, &task_id, before, after);
}

/// A stamp that `uninstall` leaves behind (systemd keeps it when the timer's file goes) is cleared by
/// the next arm of that id, which is the arm of a new install.
#[skuld::test]
fn a_stamp_left_by_uninstall_is_cleared_by_the_next_arm_of_the_id(#[fixture(systemd_group)] _group: &Group) {
    let id = support::random_test_id();
    let _guard = ServiceGuard::new(&id);
    let task_id = id_of(&id);
    let mgr = Systemd::new();
    let schedule = daily_at("-1 min");
    let spec = task(&id, vec!["/bin/true".to_string()], Some(&schedule));
    mgr.install(&spec, false).expect("install");
    mgr.enable(&task_id).expect("enable");
    mgr.uninstall(&task_id).expect("uninstall");
    assert!(stamp_path(&id).exists(), "systemd keeps the stamp: the precondition");
    plant_stale_stamp(&id);

    mgr.install(&spec, false).expect("install again");
    let before = SystemTime::now();
    mgr.enable(&task_id).expect("enable");
    let status = task_status(&mgr, &task_id);
    let after = SystemTime::now();

    // A catch-up fires from an event source dispatched after `enable` returns, so `last_trigger` can
    // still be empty then. The next run is read at once: a timer that has started to make up a run has
    // none until the run is over, and one that made up none waits for the schedule's next time.
    assert_next_run(&status, &schedule, &task_id, before, after);
    assert_eq!(
        last_trigger(&id),
        "",
        "the timer remembers no elapse: nothing was made up"
    );
}

/// An active timer is armed already: `enable` leaves its stamp, and with it the catch-up of a downtime.
#[skuld::test]
fn enable_of_an_armed_task_leaves_its_stamp(#[fixture(systemd_group)] _group: &Group) {
    let id = support::random_test_id();
    let _guard = ServiceGuard::new(&id);
    let task_id = id_of(&id);
    let mgr = Systemd::new();
    mgr.install(&exiting(&id, 0), false).expect("install");
    mgr.enable(&task_id).expect("enable");
    let before = fs::metadata(stamp_path(&id))
        .and_then(|m| m.modified())
        .expect("an armed timer has a stamp");

    mgr.enable(&task_id).expect("enable of an armed task");

    let after = fs::metadata(stamp_path(&id))
        .and_then(|m| m.modified())
        .expect("the stamp");
    assert_eq!(before, after, "the stamp is the one it was");
}

/// A timer whose `Persistent=` an administrator turned off keeps no stamp, and systemd refuses to clean
/// it: arming it asks nothing of `clean`, and `disable` and `uninstall` still work.
#[skuld::test]
fn a_timer_with_persistence_turned_off_still_arms_disarms_and_uninstalls(#[fixture(systemd_group)] _group: &Group) {
    let id = support::random_test_id();
    let _guard = ServiceGuard::new(&id);
    let task_id = id_of(&id);
    let mgr = Systemd::new();
    mgr.install(&exiting(&id, 0), false).expect("install");
    let dropin = PathBuf::from(support::SYSTEMD_UNIT_DIR).join(format!("{id}.timer.d"));
    fs::create_dir(&dropin).expect("a drop-in directory");
    fs::write(dropin.join("override.conf"), "[Timer]\nPersistent=no\n").expect("a drop-in");
    cmd::run("systemctl", &["daemon-reload"]).expect_ok();

    mgr.enable(&task_id).expect("enable");
    assert!(task_status(&mgr, &task_id).common.enabled);
    mgr.disable(&task_id).expect("disable");
    assert!(!task_status(&mgr, &task_id).common.enabled);
    mgr.enable(&task_id).expect("enable again");
    mgr.uninstall(&task_id).expect("uninstall");
    assert!(!timer_path(&id).exists());
}

/// An install that moves an armed task's schedule to a time between the timer's last elapse and now makes
/// up no run: the timer is disarmed, written, reloaded and armed again (a reload alone would start the
/// job at once), and it waits for the new schedule's next time. The armed timer is given a last elapse
/// two hours old by arming it by hand over a stale stamp; goetia's own arm would clear that.
#[skuld::test]
fn install_over_an_armed_task_with_a_schedule_already_past_makes_up_no_run(#[fixture(systemd_group)] _group: &Group) {
    let id = support::random_test_id();
    let _guard = ServiceGuard::new(&id);
    let task_id = id_of(&id);
    let mgr = Systemd::new();
    let command = vec!["/bin/true".to_string()];
    mgr.install(&task(&id, command.clone(), Some(&daily_at("+1 hour"))), false)
        .expect("install");
    plant_stale_stamp(&id);
    cmd::run("systemctl", &["enable", "--now", &format!("{id}.timer")]).expect_ok();
    assert_ne!(
        last_trigger(&id),
        "",
        "the precondition: an armed timer that remembers an elapse"
    );

    let schedule = daily_at("-1 min");
    let before = SystemTime::now();
    let outcome = mgr
        .install(&task(&id, command, Some(&schedule)), false)
        .expect("install a new schedule");
    let status = task_status(&mgr, &task_id);
    let after = SystemTime::now();

    assert!(matches!(outcome, Outcome::Update { .. }), "{outcome:?}");
    assert_eq!(
        last_trigger(&id),
        "",
        "the timer remembers no elapse: nothing was made up"
    );
    assert!(status.common.enabled, "{status:?}");
    assert_next_run(&status, &schedule, &task_id, before, after);
}

/// A disabled task's changed schedule is only written: the timer is not armed, so it never starts.
#[skuld::test]
fn install_over_a_disabled_task_with_a_changed_schedule_only_writes_it(#[fixture(systemd_group)] _group: &Group) {
    let id = support::random_test_id();
    let _guard = ServiceGuard::new(&id);
    let task_id = id_of(&id);
    let mgr = Systemd::new();
    let command = vec!["/bin/true".to_string()];
    mgr.install(&task(&id, command.clone(), Some("0 3 * * *")), false)
        .expect("install");

    let outcome = mgr
        .install(&task(&id, command, Some("0 5 * * *")), false)
        .expect("install a new schedule");

    assert!(matches!(outcome, Outcome::Update { .. }), "{outcome:?}");
    let status = task_status(&mgr, &task_id);
    assert!(!status.common.enabled, "{status:?}");
    assert_eq!(status.next_run, None, "{status:?}");
    assert!(!stamp_path(&id).exists(), "the timer never started");
}

/// A task with its schedule dropped has no timer to fire, and the timer it had is gone and not running.
#[skuld::test]
fn dropping_the_schedule_disarms_the_timer(#[fixture(systemd_group)] _group: &Group) {
    let id = support::random_test_id();
    let _guard = ServiceGuard::new(&id);
    let task_id = id_of(&id);
    let mgr = Systemd::new();
    let command = vec!["/bin/true".to_string()];
    mgr.install(&task(&id, command.clone(), Some("0 3 * * *")), false)
        .expect("install");
    mgr.enable(&task_id).expect("enable");
    assert!(timer_wants_symlink(&id).exists(), "the timer is armed");

    let outcome = mgr
        .install(&task(&id, command, None), false)
        .expect("install without a schedule");

    assert!(matches!(outcome, Outcome::Update { .. }), "{outcome:?}");
    assert!(!timer_path(&id).exists(), "no schedule, no timer");
    assert!(fs::symlink_metadata(timer_wants_symlink(&id)).is_err());
    let active = cmd::run("systemctl", &["is-active", &format!("{id}.timer")]);
    assert!(!active.ok(), "the timer is not running: {active}");
    let status = task_status(&mgr, &task_id);
    assert!(!status.common.enabled, "{status:?}");
    assert_eq!(status.next_run, None, "{status:?}");
}

/// The timer starts the task by itself: the test starts nothing, and blocks on the program's own
/// announcement. `last_run` is when the timer fired, in the minute the schedule named.
#[skuld::test]
fn the_timer_fires_by_itself(#[fixture(systemd_group)] _group: &Group) {
    let id = support::random_test_id();
    let _guard = ServiceGuard::new(&id);
    let task_id = id_of(&id);
    let mut held = Held::new();
    let mgr = Systemd::new();
    mgr.install(&task(&id, held.command.clone(), Some("* * * * *")), false)
        .expect("install");
    mgr.enable(&task_id).expect("enable");
    let scheduled = task_status(&mgr, &task_id)
        .next_run
        .expect("an armed timer has a next run");

    held.wait_started();

    let running = task_status(&mgr, &task_id);
    assert_eq!(running.common.state, State::Running, "{running:?}");
    held.release();
    let finished = status_with_result(&mgr, &task_id);
    assert_eq!(finished.last_result, Some(Exit::Code(0)), "{finished:?}");
    let fired = finished.last_run.expect("a run that ended started");
    assert!(
        fired >= scheduled && fired < scheduled + Duration::from_secs(60),
        "last_run {fired:?} is not in the minute of {scheduled:?}"
    );
    assert!(finished.common.enabled, "the schedule stays armed: {finished:?}");
}

/// `disable` stops the schedule, not the run in flight.
#[skuld::test]
fn disable_leaves_a_run_in_flight_running(#[fixture(systemd_group)] _group: &Group) {
    let id = support::random_test_id();
    let _guard = ServiceGuard::new(&id);
    let task_id = id_of(&id);
    let mut held = Held::new();
    let mgr = Systemd::new();
    mgr.install(&task(&id, held.command.clone(), Some("0 3 * * *")), false)
        .expect("install");
    mgr.enable(&task_id).expect("enable");
    mgr.start(&task_id, Budget::DEFAULT).expect("start");
    held.wait_started();

    mgr.disable(&task_id).expect("disable");

    let status = task_status(&mgr, &task_id);
    assert_eq!(status.common.state, State::Running, "{status:?}");
    assert!(!status.common.enabled, "{status:?}");
    held.release();
    assert_eq!(status_with_result(&mgr, &task_id).last_result, Some(Exit::Code(0)));
}

/// An exit 0 unloads the unit, and the journal still says how the run went and when: `last_run` is
/// the time of the unit's one "Starting" entry. (Not compared with a `SystemTime::now()` taken
/// before the start: the wall clock of a VM can step back between two reads.)
#[skuld::test]
fn status_after_a_successful_run_reads_exit_0_from_the_journal(#[fixture(systemd_group)] _group: &Group) {
    let id = support::random_test_id();
    let _guard = ServiceGuard::new(&id);
    let task_id = id_of(&id);
    let mgr = Systemd::new();
    mgr.install(&task(&id, vec!["/bin/true".to_string()], None), false)
        .expect("install");

    mgr.start(&task_id, Budget::DEFAULT).expect("start");

    let status = status_with_result(&mgr, &task_id);
    assert_eq!(status.last_result, Some(Exit::Code(0)), "{status:?}");
    let unit = format!("UNIT={}", unit_of(&id));
    let starting = journal(&[&unit, &format!("MESSAGE_ID={STARTING}")]);
    let [entry] = starting.as_slice() else {
        panic!("a task run once has one Starting entry: {starting:?}");
    };
    let micros: u64 = field(entry, "__REALTIME_TIMESTAMP")
        .and_then(|micros| micros.parse().ok())
        .expect("an entry has a time");
    assert_eq!(
        status.last_run,
        Some(SystemTime::UNIX_EPOCH + Duration::from_micros(micros)),
        "{status:?}"
    );
    assert_eq!(status.common.state, State::Idle, "{status:?}");
}

// start, stop, restart, uninstall =====================================================================================

#[skuld::test]
fn start_returns_before_the_run_ends(#[fixture(systemd_group)] _group: &Group) {
    let id = support::random_test_id();
    let _guard = ServiceGuard::new(&id);
    let task_id = id_of(&id);
    let mut held = Held::new();
    let mgr = Systemd::new();
    mgr.install(&task(&id, held.command.clone(), None), false)
        .expect("install");

    mgr.start(&task_id, Budget::DEFAULT).expect("start");

    let status = task_status(&mgr, &task_id);
    assert_eq!(status.common.state, State::Running, "{status:?}");
    held.wait_started();
    held.release();
    assert_eq!(status_with_result(&mgr, &task_id).last_result, Some(Exit::Code(0)));
}

#[skuld::test]
fn start_of_a_task_while_it_runs_starts_nothing_new(#[fixture(systemd_group)] _group: &Group) {
    let id = support::random_test_id();
    let _guard = ServiceGuard::new(&id);
    let task_id = id_of(&id);
    let mut held = Held::new();
    let mgr = Systemd::new();
    mgr.install(&task(&id, held.command.clone(), None), false)
        .expect("install");
    mgr.start(&task_id, Budget::DEFAULT).expect("start");
    held.wait_started();
    let pid = task_status(&mgr, &task_id).common.pid;

    mgr.start(&task_id, Budget::DEFAULT).expect("start again");

    assert_eq!(started_invocations(&id).len(), 1, "a second run began");
    let status = task_status(&mgr, &task_id);
    assert_eq!((status.common.state, status.common.pid), (State::Running, pid));
}

/// `stop` ends the run in flight with SIGTERM and leaves the task idle; `run`, following that run,
/// returns the signal.
#[skuld::test]
fn run_of_a_stopped_run_reports_sigterm(#[fixture(systemd_group)] _group: &Group) {
    let id = support::random_test_id();
    let _guard = ServiceGuard::new(&id);
    let task_id = id_of(&id);
    let held = Held::new();
    let mgr = Systemd::new();
    mgr.install(&task(&id, held.command.clone(), None), false)
        .expect("install");
    let (lines, shown) = mpsc::channel();

    let exit = std::thread::scope(|scope| {
        let _stop = StopOnDrop(unit_of(&id));
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
}

/// `run` of a task in flight is refused and starts nothing: the run in flight is the only one.
#[skuld::test]
fn run_refuses_a_run_in_flight(#[fixture(systemd_group)] _group: &Group) {
    let id = support::random_test_id();
    let _guard = ServiceGuard::new(&id);
    let task_id = id_of(&id);
    let mut held = Held::new();
    let mgr = Systemd::new();
    mgr.install(&task(&id, held.command.clone(), None), false)
        .expect("install");
    mgr.start(&task_id, Budget::DEFAULT).expect("start");
    held.wait_started();
    let pid = task_status(&mgr, &task_id).common.pid;
    let mut out = Vec::new();

    let e = mgr.run(&task_id, &mut out).expect_err("a task in flight is not run");

    assert!(matches!(e, Error::AlreadyRunning { .. }), "{e:?}");
    assert!(out.is_empty());
    assert_eq!(started_invocations(&id).len(), 1, "a second run began");
    assert_eq!(task_status(&mgr, &task_id).common.pid, pid);
}

/// `restart` of a task in flight ends that run (SIGTERM) and starts a new one under the same job; the
/// first run's result is its own, in the journal. Both forms: the waited one stops, then starts, and
/// the unwaited one is one `systemctl restart --no-block`.
#[skuld::test]
fn restart_of_a_running_task_ends_it_and_starts_a_new_run(#[fixture(systemd_group)] _group: &Group) {
    let id = support::random_test_id();
    let _guard = ServiceGuard::new(&id);
    let task_id = id_of(&id);
    let mut held = Held::new();
    let mgr = Systemd::new();
    mgr.install(&task(&id, held.command.clone(), None), false)
        .expect("install");
    mgr.start(&task_id, Budget::DEFAULT).expect("start");
    held.wait_started();
    let restart = |timeout: Option<Duration>| {
        let args = goetia::cli::restart::Args {
            ids: vec![id.clone()],
            wait: goetia::cli::wait::WaitArgs {
                timeout,
                no_timeout: false,
            },
        };
        let get_manager = || -> goetia::Result<Box<dyn ServiceManager>> { Ok(Box::new(Systemd::new())) };
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
    };

    restart(None);
    held.wait_started();
    restart(Some(Duration::ZERO));
    held.wait_started();

    let runs = started_invocations(&id);
    assert_eq!(runs.len(), 3, "{runs:?}");
    let ended = exited(&id);
    assert_eq!(
        ended,
        [
            (runs[0].clone(), "killed".to_string(), "15".to_string()),
            (runs[1].clone(), "killed".to_string(), "15".to_string())
        ],
        "each run restarted ended with SIGTERM"
    );
    let status = task_status(&mgr, &task_id);
    assert_eq!(status.common.state, State::Running, "{status:?}");
    held.release();
    assert_eq!(status_with_result(&mgr, &task_id).last_result, Some(Exit::Code(0)));
}

#[skuld::test]
fn uninstall_of_a_running_armed_task_stops_it_first(#[fixture(systemd_group)] _group: &Group) {
    let id = support::random_test_id();
    let _guard = ServiceGuard::new(&id);
    let task_id = id_of(&id);
    let mut held = Held::new();
    let mgr = Systemd::new();
    mgr.install(&task(&id, held.command.clone(), Some("0 3 * * *")), false)
        .expect("install");
    mgr.enable(&task_id).expect("enable");
    mgr.start(&task_id, Budget::DEFAULT).expect("start");
    held.wait_started();
    let pid: u32 = main_pid(&id).parse().expect("a pid");
    assert_ne!(pid, 0, "the run has a process");

    mgr.uninstall(&task_id).expect("uninstall");

    assert!(
        !Path::new(&format!("/proc/{pid}")).exists(),
        "the run's process is gone"
    );
    assert!(!crate::linux::unit_path(&id).exists() && !timer_path(&id).exists());
    assert!(fs::symlink_metadata(timer_wants_symlink(&id)).is_err());
    let active = cmd::run("systemctl", &["is-active", &format!("{id}.timer")]);
    assert!(!active.ok(), "the timer is not running: {active}");
    let e = mgr.status(&task_id).expect_err("nothing is installed");
    assert!(matches!(e, Error::NotInstalled { .. }), "{e:?}");
}

// install =============================================================================================================

/// `env` that `/bin/sh` resets is refused, and nothing is written.
#[skuld::test]
fn a_task_whose_env_sh_resets_is_refused_before_anything_is_written(#[fixture(systemd_group)] _group: &Group) {
    let id = support::random_test_id();
    let _guard = ServiceGuard::new(&id);
    let DaemonSpec::Task(spec) = task(&id, vec!["/bin/true".to_string()], Some("0 3 * * *")) else {
        unreachable!("`task` builds a task");
    };
    let mut fields = spec.into_fields();
    fields.common.env.insert("IFS".to_string(), ",".to_string());
    let spec = DaemonSpec::from(TaskSpec::new(fields).expect("the spec rules do not know sh"));

    let e = Systemd::new().install(&spec, false).expect_err("refused");

    assert!(matches!(e, Error::Invalid { .. }), "{e:?}");
    let message = e.to_string();
    assert!(message.contains("IFS") && message.contains("/bin/sh"), "{message}");
    assert!(!crate::linux::unit_path(&id).exists() && !timer_path(&id).exists());
}

/// `sh` execs a task's `command[0]` as written, so the program runs and its output arrives.
#[skuld::test]
fn a_task_whose_command_has_a_backslash_quote_or_trailing_space_installs_and_runs(
    #[fixture(systemd_group)] _group: &Group,
) {
    use std::os::unix::fs::PermissionsExt as _;

    let dir = tempfile::tempdir().expect("tempdir");
    for name in [r"a\b", "a\"b", "a'b", "a ", "a\\b\"c'd "] {
        let program = dir.path().join(name);
        fs::write(&program, "#!/bin/sh\necho announced\n").expect("write the program");
        fs::set_permissions(&program, fs::Permissions::from_mode(0o755)).expect("make it executable");
        let id = support::random_test_id();
        let _guard = ServiceGuard::new(&id);
        let task_id = id_of(&id);
        let mgr = Systemd::new();
        let command = vec![program.to_str().expect("a UTF-8 path").to_string()];
        mgr.install(&task(&id, command, None), false)
            .unwrap_or_else(|e| panic!("{name:?}: install: {}", e.report()));

        let (exit, out) = run_task(&mgr, &task_id);

        assert_eq!(exit, Exit::Code(0), "{name:?}");
        assert_eq!(String::from_utf8_lossy(&out), "announced\n", "{name:?}");
    }
}

// run =================================================================================================================

/// What a run shows and how it ends, through the library and through `goetia daemon run`: stdout and
/// stderr lines in order, and the program's own exit code.
#[skuld::test]
fn run_streams_journal_output_and_returns_the_exit_code(#[fixture(systemd_group)] _group: &Group) {
    let id = support::random_test_id();
    let _guard = ServiceGuard::new(&id);
    let task_id = id_of(&id);
    let mgr = Systemd::new();
    mgr.install(&task(&id, sh("echo out-line; echo err-line >&2; exit 3"), None), false)
        .expect("install");

    let (exit, out) = run_task(&mgr, &task_id);

    assert_eq!(exit, Exit::Code(3));
    assert_eq!(String::from_utf8_lossy(&out), "out-line\nerr-line\n");
    let status = task_status(&mgr, &task_id);
    assert_eq!(status.common.state, State::Idle, "{status:?}");
    assert_eq!(status.last_result, Some(Exit::Code(3)), "{status:?}");

    let args = goetia::cli::run::Args { id: id.clone() };
    let get_manager = || -> goetia::Result<Box<dyn ServiceManager>> { Ok(Box::new(Systemd::new())) };
    let (mut out, mut err) = (Vec::new(), Vec::new());
    // Truthful, not a stub: the group's elevation check is this test's own precondition.
    let code = goetia::cli::run::run(&args, &get_manager, &|| true, &mut out, &mut err);
    assert_eq!(code, ExitCode::Run(3), "stderr:\n{}", String::from_utf8_lossy(&err));
    assert_eq!(String::from_utf8_lossy(&out), "out-line\nerr-line\n");
}

/// What a run shows is what the program wrote: a last line with no newline has none, and a line longer
/// than journald's `LineMax` (which it stores in pieces) is one line again.
#[skuld::test]
fn run_shows_the_programs_own_line_ends(#[fixture(systemd_group)] _group: &Group) {
    let id = support::random_test_id();
    let _guard = ServiceGuard::new(&id);
    let task_id = id_of(&id);
    let mgr = Systemd::new();
    // One process writes it all (a second writer would end an unterminated line at its pid change): 60000
    // bytes, past the default LineMax of 48K (49152), then a line with no newline at its end.
    let script =
        r#"awk 'BEGIN { s = "x"; while (length(s) < 60000) s = s s; print substr(s, 1, 60000); printf "a\nlast" }'"#;
    mgr.install(&task(&id, sh(script), None), false).expect("install");

    let (exit, out) = run_task(&mgr, &task_id);

    assert_eq!(exit, Exit::Code(0));
    let expected = format!("{}\na\nlast", "x".repeat(60000));
    assert_eq!(String::from_utf8_lossy(&out), expected);
}

/// A start job queued behind another unit's and canceled before it runs (a `stop` racing the request)
/// writes no journal entry, so `run` learns of its end from the manager no longer holding it, and says
/// the outcome is unknown: a canceled job and a journal that keeps no manager messages look the same.
#[skuld::test]
fn run_of_a_start_job_canceled_before_it_ran_ends_with_an_unknown_outcome(#[fixture(systemd_group)] _group: &Group) {
    let blocker_id = support::random_test_id();
    let _blocker_guard = ServiceGuard::new(&blocker_id);
    let id = support::random_test_id();
    let _guard = ServiceGuard::new(&id);
    let task_id = id_of(&id);
    let mut held = Held::new();
    let mgr = Systemd::new();
    mgr.install(&task(&blocker_id, held.command.clone(), None), false)
        .expect("install the blocker");
    mgr.install(&task(&id, vec!["/bin/true".to_string()], None), false)
        .expect("install");
    // The task's job is ordered after the blocker's, so its start job waits while the blocker runs.
    // The guard removes the `.d` directory with the unit.
    let dropin = Path::new(support::SYSTEMD_UNIT_DIR).join(format!("{}.d", unit_of(&id)));
    fs::create_dir(&dropin).expect("the drop-in directory");
    fs::write(
        dropin.join("after.conf"),
        format!("[Unit]\nAfter={}\n", unit_of(&blocker_id)),
    )
    .expect("the drop-in");
    cmd::run("systemctl", &["daemon-reload"]).expect_ok();
    mgr.start(&id_of(&blocker_id), Budget::DEFAULT)
        .expect("start the blocker");
    held.wait_started();
    let _release = StopOnDrop(unit_of(&blocker_id));

    let result = std::thread::scope(|scope| {
        let running = scope.spawn(|| mgr.run(&task_id, &mut Vec::new()));
        // The job is queued once the manager lists it; then it is canceled.
        kill::until(|| {
            cmd::run("systemctl", &["list-jobs", "--no-legend"])
                .stdout
                .lines()
                .any(|job| job.contains(&unit_of(&id)))
        });
        cmd::run("systemctl", &["stop", &unit_of(&id)]).expect_ok();
        running.join().expect("the run did not panic")
    });

    let e = result.expect_err("the job was canceled");
    assert!(
        matches!(e, Error::RequestInDoubt { reached: true, .. }),
        "{}",
        e.report()
    );
    assert!(e.is_indeterminate());
    assert!(
        e.report().to_string().contains("canceled before it ran"),
        "{}",
        e.report()
    );
    held.release();
}

/// Every line of a run that prints a great deal arrives, in order, run after run: the terminal entry
/// comes before most of such a run's output, and the rest is read after it.
#[skuld::test(labels = [JOURNALD_CONFIG], serial = JOURNALD_CONFIG)]
fn run_returns_every_line_of_a_chatty_run(#[fixture(systemd_group)] _group: &Group) {
    let _journald = JournaldConf::unlimited();
    let id = support::random_test_id();
    let _guard = ServiceGuard::new(&id);
    let task_id = id_of(&id);
    let mgr = Systemd::new();
    mgr.install(
        &task(
            &id,
            vec!["/usr/bin/seq".to_string(), "1".to_string(), "20000".to_string()],
            None,
        ),
        false,
    )
    .expect("install");
    let want = counted(20000);

    for n in 1..=20 {
        let before = stdout_end(&id);
        let (exit, out) = run_task(&mgr, &task_id);

        assert_eq!(exit, Exit::Code(0));
        let journaled = run_stream_text(&id, before.as_deref());
        assert_same_text(n, &out, &journaled);
        assert_holds_in_order(n, &journaled, &want);
    }
}

/// A line of 5000 bytes (over the 4088 `journalctl` nulls without `-a`) and one with a control byte.
#[skuld::test]
fn a_line_of_5000_bytes_and_a_control_byte_arrive_whole(#[fixture(systemd_group)] _group: &Group) {
    let id = support::random_test_id();
    let _guard = ServiceGuard::new(&id);
    let task_id = id_of(&id);
    let long = "x".repeat(5000);
    let mgr = Systemd::new();
    mgr.install(
        &task(
            &id,
            ["/usr/bin/printf", "%s\\001end\\n", long.as_str()]
                .map(String::from)
                .to_vec(),
            None,
        ),
        false,
    )
    .expect("install");

    let (exit, out) = run_task(&mgr, &task_id);

    assert_eq!(exit, Exit::Code(0));
    let mut want = long.into_bytes();
    want.extend_from_slice(b"\x01end\n");
    assert_eq!(out, want);
}

/// A run starts right behind a chatty one and shows none of its lines: what it shows is its own stream
/// in the journal, which holds `second-run` and nothing of the first run's. The second run starts
/// after the first `run` has synced and returned, so any first-run line after `before` is a line
/// `run` missed.
#[skuld::test(labels = [JOURNALD_CONFIG], serial = JOURNALD_CONFIG)]
fn a_second_run_shows_none_of_the_first_runs_lines(#[fixture(systemd_group)] _group: &Group) {
    let _journald = JournaldConf::unlimited();
    let id = support::random_test_id();
    let _guard = ServiceGuard::new(&id);
    let task_id = id_of(&id);
    let dir = tempfile::tempdir().expect("tempdir");
    let flag = dir.path().join("flag");
    let script = format!(
        "if [ -e \"{flag}\" ]; then rm \"{flag}\"; echo second-run; else : > \"{flag}\"; seq 1 20000; fi",
        flag = flag.display()
    );
    let mgr = Systemd::new();
    mgr.install(&task(&id, sh(&script), None), false).expect("install");
    let first = counted(20000);

    for n in 1..=20 {
        let before = stdout_end(&id);
        let (_, out) = run_task(&mgr, &task_id);
        let journaled = run_stream_text(&id, before.as_deref());
        assert_same_text(n, &out, &journaled);
        assert_holds_in_order(n, &journaled, &first);

        let before = stdout_end(&id);
        let (exit, out) = run_task(&mgr, &task_id);
        assert_eq!(exit, Exit::Code(0));
        let journaled = run_stream_text(&id, before.as_deref());
        assert_same_text(n, &out, &journaled);
        assert_holds_in_order(n, &journaled, b"second-run\n");
        assert!(
            journaled
                .split_inclusive(|&b| b == b'\n')
                .all(|line| line == b"second-run\n"),
            "run {n}: the second run's stream holds a line of another run"
        );
    }
}

/// The program counts its runs in `dir`: run `n` prints `run <n>`.
fn counting(dir: &Path) -> Vec<String> {
    let counter = dir.join("count");
    sh(&format!(
        "n=$(cat \"{c}\" 2>/dev/null || echo 0); n=$((n + 1)); echo $n > \"{c}\"; echo \"run $n\"",
        c = counter.display()
    ))
}

/// Run after run, each shows its own line and no other run's, in the journal.
#[skuld::test]
fn back_to_back_runs_show_only_their_own_lines(#[fixture(systemd_group)] _group: &Group) {
    let id = support::random_test_id();
    let _guard = ServiceGuard::new(&id);
    let task_id = id_of(&id);
    let dir = tempfile::tempdir().expect("tempdir");
    let mgr = Systemd::new();
    mgr.install(&task(&id, counting(dir.path()), None), false)
        .expect("install");

    for n in 1..=20 {
        let (exit, out) = run_task(&mgr, &task_id);

        assert_eq!(exit, Exit::Code(0));
        assert_same_text(n, &out, format!("run {n}\n").as_bytes());
    }
}

/// A task with `logs` shows its file from the run's own start, not what was in it before, and the
/// marker line is never shown.
#[skuld::test]
fn run_streams_the_logs_file(#[fixture(systemd_group)] _group: &Group) {
    let id = support::random_test_id();
    let _guard = ServiceGuard::new(&id);
    let task_id = id_of(&id);
    let dir = tempfile::tempdir().expect("tempdir");
    let log = dir.path().join("task.log");
    fs::write(&log, "old line\n").expect("seed the log");
    let mgr = Systemd::new();
    mgr.install(
        &with_logs(task(&id, sh("echo one; echo two >&2; exit 2"), None), &log),
        false,
    )
    .expect("install");

    let (exit, out) = run_task(&mgr, &task_id);

    assert_eq!(exit, Exit::Code(2));
    assert_eq!(String::from_utf8_lossy(&out), "one\ntwo\n");
    let file = fs::read_to_string(&log).expect("read the log");
    assert!(file.starts_with("old line\n"), "{file}");
    assert!(file.contains("goetia-run "), "the file holds the run's marker: {file}");
}

/// Run after run in a `logs` file, each shows only its own lines: the marker of the next run ends
/// the one before.
#[skuld::test]
fn back_to_back_runs_show_only_their_own_lines_in_the_logs_file(#[fixture(systemd_group)] _group: &Group) {
    let id = support::random_test_id();
    let _guard = ServiceGuard::new(&id);
    let task_id = id_of(&id);
    let dir = tempfile::tempdir().expect("tempdir");
    let log = dir.path().join("task.log");
    let mgr = Systemd::new();
    mgr.install(&with_logs(task(&id, counting(dir.path()), None), &log), false)
        .expect("install");

    for n in 1..=20 {
        let (exit, out) = run_task(&mgr, &task_id);

        assert_eq!(exit, Exit::Code(0));
        assert_same_text(n, &out, format!("run {n}\n").as_bytes());
    }
}

/// A task that runs as an account of a regular uid (>= 1000) shows its output on a host that keeps a
/// journal and splits it by uid, through `goetia daemon run`: journald stores such a unit's lines in the
/// account's user journal, which a read of the system journal alone would not find. The test checks
/// that the lines are where it says, so that it exercises the split. The storage setting is the
/// fixture's and goes back with the drop-in.
#[skuld::test(labels = [JOURNALD_CONFIG], serial = JOURNALD_CONFIG)]
fn run_of_a_task_as_a_regular_uid_shows_its_output_on_persistent_storage(#[fixture(systemd_group)] _group: &Group) {
    let _journald = JournaldConf::set("Storage=persistent\n");
    let id = support::random_test_id();
    let _guard = ServiceGuard::new(&id);
    let account = support::random_test_id();
    let _account = AccountGuard(account.clone());
    cmd::run(
        "useradd",
        &[
            "--no-create-home",
            "--no-user-group",
            "--shell",
            "/usr/sbin/nologin",
            &account,
        ],
    )
    .expect_ok();
    let uid: u32 = cmd::run("id", &["-u", &account])
        .expect_ok()
        .stdout
        .trim()
        .parse()
        .expect("a uid");
    assert!(uid >= 1000, "{account} has uid {uid}, which is a system uid");
    let DaemonSpec::Task(spec) = task(&id, sh("echo one; echo two >&2; exit 2"), None) else {
        unreachable!("`task` builds a task");
    };
    let mut fields = spec.into_fields();
    fields.common.user = User::Name(account);
    let mgr = Systemd::new();
    mgr.install(&TaskSpec::new(fields).expect("a task as an account").into(), false)
        .expect("install");

    let args = goetia::cli::run::Args { id: id.clone() };
    let get_manager = || -> goetia::Result<Box<dyn ServiceManager>> { Ok(Box::new(Systemd::new())) };
    let (mut out, mut err) = (Vec::new(), Vec::new());
    // Truthful, not a stub: the group's elevation check is this test's own precondition.
    let code = goetia::cli::run::run(&args, &get_manager, &|| true, &mut out, &mut err);

    assert_eq!(code, ExitCode::Run(2), "stderr:\n{}", String::from_utf8_lossy(&err));
    assert_eq!(String::from_utf8_lossy(&out), "one\ntwo\n");
    let unit = format!("_SYSTEMD_UNIT={}", unit_of(&id));
    let lines = |system: bool| {
        let mut args = vec!["-a", "-o", "json", "--no-pager", unit.as_str(), "_TRANSPORT=stdout"];
        if system {
            args.push("--system");
        }
        cmd::run("journalctl", &args).expect_ok().stdout.lines().count()
    };
    assert_eq!(lines(true), 0, "the account's lines are not in the system journal");
    assert_eq!(
        lines(false),
        3,
        "the marker and the two lines are in the account's own journal"
    );
}

/// A run whose marker journald dropped shows the note and still returns the run's exit. Journald
/// allows a unit 200 messages per hour here (its limit is `RateLimitBurst` scaled up by the disk's free
/// space); the first run's 50000 lines use that up, so the second run's marker is dropped. Journald's
/// limit on the manager's own entries is a separate one, which a few entries a run do not reach.
#[skuld::test(labels = [JOURNALD_CONFIG], serial = JOURNALD_CONFIG)]
fn a_dropped_marker_is_noted_and_the_runs_exit_is_still_reported(#[fixture(systemd_group)] _group: &Group) {
    let _journald = JournaldConf::set(
        "RateLimitIntervalSec=1h
RateLimitBurst=200
",
    );
    let id = support::random_test_id();
    let _guard = ServiceGuard::new(&id);
    let task_id = id_of(&id);
    let mgr = Systemd::new();
    mgr.install(
        &task(
            &id,
            vec!["/usr/bin/seq".to_string(), "1".to_string(), "50000".to_string()],
            None,
        ),
        false,
    )
    .expect("install");
    let (exit, _) = run_task(&mgr, &task_id);
    assert_eq!(exit, Exit::Code(0));

    let (exit, out) = run_task(&mgr, &task_id);

    assert_eq!(exit, Exit::Code(0));
    let out = String::from_utf8_lossy(&out);
    assert!(
        out.starts_with("note: the marker line of this run is not in the journal") && out.lines().count() == 1,
        "{out}"
    );
}

// What the implementation assumes of systemd and journalctl ===========================================================

/// Measured, not assumed:
/// - a run's marker line is in the journal after its own Starting entry;
/// - an `INVOCATION_ID` is 32 lowercase hex digits, and the marker names the Starting entry's;
/// - `--after-cursor` returns the entries after one and not that one;
/// - once `run` has returned, none of its `journalctl` children is left, reaped or not.
#[skuld::test]
fn a_runs_marker_follows_its_starting_entry_and_no_journalctl_is_left(#[fixture(systemd_group)] _group: &Group) {
    let id = support::random_test_id();
    let _guard = ServiceGuard::new(&id);
    let task_id = id_of(&id);
    let mgr = Systemd::new();
    mgr.install(&task(&id, sh("echo one; echo two"), None), false)
        .expect("install");

    let (exit, out) = run_task(&mgr, &task_id);

    assert_eq!(exit, Exit::Code(0));
    assert_eq!(String::from_utf8_lossy(&out), "one\ntwo\n");
    assert_eq!(journalctl_children(), Vec::<u32>::new(), "run left a journalctl behind");

    let unit = unit_of(&id);
    let (by_manager, by_unit) = (format!("UNIT={unit}"), format!("_SYSTEMD_UNIT={unit}"));
    let entries = journal(&[&by_manager, "+", &by_unit, "_TRANSPORT=stdout"]);
    let starting = entries
        .iter()
        .position(|entry| field(entry, "MESSAGE_ID") == Some(STARTING))
        .expect("a Starting entry");
    let invocation = field(&entries[starting], "INVOCATION_ID").expect("an invocation");
    assert!(
        invocation.len() == 32
            && invocation
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
        "`{invocation}` is not 32 lowercase hex digits"
    );
    let marker = format!("goetia-run {invocation}");
    let at = entries
        .iter()
        .position(|entry| field(entry, "MESSAGE") == Some(marker.as_str()))
        .expect("the marker line is in the journal");
    assert!(
        at > starting,
        "the marker (entry {at}) is before the Starting entry ({starting})"
    );

    let after = format!("--after-cursor={}", field(&entries[at], "__CURSOR").expect("a cursor"));
    let later = journal(&[&after, &by_unit, "_TRANSPORT=stdout"]);
    let lines: Vec<_> = later.iter().filter_map(|entry| field(entry, "MESSAGE")).collect();
    assert_eq!(lines, ["one", "two"]);
}

/// Removes the local account `name` on drop.
struct AccountGuard(String);

impl Drop for AccountGuard {
    fn drop(&mut self) {
        warn_left_behind(cmd::run_cleanup("userdel", &[&[&self.0]], &[6]));
    }
}

/// An account that may not read the system journal gets an error for a task's status, not a status
/// that says the task never ran: even with a user journal of its own, which `journalctl` without
/// `--system` would open and answer from with nothing. (The raw `journalctl` facts are in the
/// knowledge base.) A service's status needs no journal.
#[skuld::test]
fn status_of_a_task_for_an_account_that_cannot_read_the_system_journal_is_the_error(
    #[fixture(systemd_group)] _group: &Group,
) {
    let id = support::random_test_id();
    let _guard = ServiceGuard::new(&id);
    let task_id = id_of(&id);
    let mgr = Systemd::new();
    mgr.install(&exiting(&id, 0), false).expect("install");
    mgr.start(&task_id, Budget::DEFAULT).expect("start");
    status_with_result(&mgr, &task_id);
    let service = support::random_test_id();
    let _service_guard = ServiceGuard::new(&service);
    mgr.install(&crate::linux::mk(&service), false)
        .expect("install a service");

    let name = support::random_test_id();
    let _account = AccountGuard(name.clone());
    cmd::run(
        "useradd",
        &[
            "--no-create-home",
            "--no-user-group",
            "--shell",
            "/usr/sbin/nologin",
            &name,
        ],
    )
    .expect_ok();
    cmd::run(
        "runuser",
        &[
            "-u",
            &name,
            "--",
            "systemd-cat",
            "-t",
            "goetia-test",
            "echo",
            "its own line",
        ],
    )
    .expect_ok();
    cmd::run("journalctl", &["--sync"]).expect_ok();

    let output = crate::linux::run_as(&name, &["daemon", "status", &id]);
    let (stdout, stderr) = (
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    let context = format!(
        "stdout:
{stdout}
stderr:
{stderr}"
    );
    assert_ne!(output.status.code(), Some(0), "{context}");
    assert!(
        stderr.contains("No journal files were opened due to insufficient permissions"),
        "{context}"
    );
    assert!(
        stderr.contains("run as root or a member of `systemd-journal`"),
        "{context}"
    );
    assert!(!stdout.contains("idle"), "a status was printed: {context}");

    let output = crate::linux::run_as(&name, &["daemon", "status", &service]);
    assert_eq!(
        output.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}
