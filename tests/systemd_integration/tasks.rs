//! Tasks on a real systemd: arming, the timer firing, `run` following a run through the journal,
//! `start`, `stop`, `restart`, and what the journal and `journalctl` do that the unit tests assume.
//!
//! The installing, arming, retyping and uninstalling of an idle task is `linux.rs`'s ("Tasks"); this
//! file adds the runs. A task that has to be caught in flight is [`Held`]: it announces itself and waits
//! on a FIFO, so a test waits on the program, never on a time. The Linux lane runs one test at a time;
//! the tests that change journald's configuration share the `JOURNALD_CONFIG` label, so that no two of
//! them overlap where tests run in parallel.

use std::fs::{self, File, OpenOptions};
use std::io::{BufRead as _, BufReader, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::{Duration, SystemTime};

use goetia::Error;
use goetia::backend::systemd::manager::Systemd;
use goetia::cli::ExitCode;
use goetia::decide::Outcome;
use goetia::manager::{Budget, Exit, Installed, ServiceManager, State, Status, TaskStatus};
use goetia::spec::{DaemonSpec, HashSeed, Id, Schedule, TaskSpec, User};
use serde_json::Value;

use crate::linux::{
    CreatedRoot, UNIT_DIR_EXCLUSIVE, create_root, exiting, main_pid, mkfifo, task_status, timer_path,
    timer_wants_symlink,
};
use crate::support::cleanup::{remove_or_warn, warn_left_behind};
use crate::support::{self, Group, ServiceGuard, cmd, cron_oracle, kill, systemd_group};

/// Held by the tests that rewrite journald's configuration (and restart it) while they run.
#[skuld::label]
pub(crate) const JOURNALD_CONFIG: skuld::Label;

/// "Starting `<unit>`...": the manager began a run.
const STARTING: &str = "7d4958e842da4a758f6c1cdc7b36dcc5";

// Fixtures ============================================================================================================

fn id_of(id: &str) -> Id {
    Id::try_from(id).unwrap_or_else(|e| panic!("`{id}` is not an id: {e}"))
}

fn unit_of(id: &str) -> String {
    format!("{id}.service")
}

fn sh(script: &str) -> Vec<String> {
    ["/bin/sh", "-c", script].map(String::from).to_vec()
}

fn task(id: &str, command: Vec<String>, schedule: Option<&str>) -> DaemonSpec {
    goetia::testing::task(id, command, schedule)
}

/// `spec` with its output going to `logs`.
fn with_logs(spec: DaemonSpec, logs: &Path) -> DaemonSpec {
    let DaemonSpec::Task(task) = spec else {
        panic!("`with_logs` is given a task");
    };
    let mut fields = task.into_fields();
    fields.common.logs = Some(logs.to_path_buf());
    TaskSpec::new(fields).expect("a task with logs").into()
}

/// A task that is in flight until the test releases it. It prints `ready`, announces itself on a FIFO
/// and waits for the test to open the other. The announcing FIFO is opened read-write here, so the
/// task's write never waits for the test to read it: [`Held::wait_started`] reads it when it wants to
/// know a run has begun, once per run. Released, the run exits 0.
struct Held {
    started: BufReader<File>,
    hold: PathBuf,
    command: Vec<String>,
    _dir: tempfile::TempDir,
}

impl Held {
    fn new() -> Held {
        let dir = tempfile::tempdir().expect("tempdir");
        let (started, hold) = (dir.path().join("started"), dir.path().join("hold"));
        mkfifo(&started);
        mkfifo(&hold);
        let announce = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&started)
            .expect("open the announcing FIFO");
        let script = format!(
            "echo ready; echo started > \"{}\"; read line < \"{}\"; exit 0",
            started.display(),
            hold.display()
        );
        Held {
            started: BufReader::new(announce),
            hold,
            command: sh(&script),
            _dir: dir,
        }
    }

    /// Blocks until the next run has printed `ready`. A run that never starts hangs the test.
    fn wait_started(&mut self) {
        let mut line = String::new();
        self.started.read_line(&mut line).expect("read the announcing FIFO");
        assert_eq!(line, "started\n");
    }

    /// Lets the run in flight end. Blocks until the run is waiting for it, so only call it while one is.
    fn release(&self) {
        drop(
            OpenOptions::new()
                .write(true)
                .open(&self.hold)
                .expect("open the hold FIFO"),
        );
    }
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

/// What `run` shows, and how it ended.
fn run_task(mgr: &Systemd, id: &Id) -> (Exit, Vec<u8>) {
    let mut out = Vec::new();
    let exit = mgr
        .run(id, &mut out)
        .unwrap_or_else(|e| panic!("run {id}: {}", e.report()));
    (exit, out)
}

/// The status of `id` once its newest run has a result.
fn status_with_result(mgr: &Systemd, id: &Id) -> TaskStatus {
    let mut found = None;
    kill::until(|| {
        let status = task_status(mgr, id);
        let done = status.last_result.is_some();
        found = Some(status);
        done
    });
    found.expect("a status was read")
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

/// A diff small enough to read: the first line where `got` and `want` differ.
fn assert_same_text(run: usize, got: &[u8], want: &[u8]) {
    if got == want {
        return;
    }
    let (got, want) = (String::from_utf8_lossy(got), String::from_utf8_lossy(want));
    let (mut got_lines, mut want_lines) = (got.lines(), want.lines());
    let mut line = 1;
    loop {
        match (got_lines.next(), want_lines.next()) {
            (a, b) if a == b => line += 1,
            (a, b) => panic!(
                "run {run}: the output differs at line {line}: got {a:?}, want {b:?} ({} lines against {})",
                got.lines().count(),
                want.lines().count()
            ),
        }
    }
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

/// Both ends of a schedule's next minute across a window: a timer armed anywhere in `[before, after]`
/// reads one of them. `after` is taken once `next_run` has been read, so that a read that comes after
/// the timer fired (its next run then the occurrence after) is still inside the window.
fn expected_next_runs(text: &str, id: &Id, before: SystemTime, after: SystemTime) -> [SystemTime; 2] {
    let seed = HashSeed {
        hostname: "testing",
        id,
    };
    let schedule = Schedule::parse(text, &seed).expect("a schedule");
    [before, after].map(|at| cron_oracle::next_minute_after(&schedule, at))
}

fn assert_next_run(status: &TaskStatus, text: &str, id: &Id, before: SystemTime, after: SystemTime) {
    let expected = expected_next_runs(text, id, before, after);
    assert!(
        status.next_run.is_some_and(|next| expected.contains(&next)),
        "`{text}`: next_run {:?}, the cron says {expected:?}",
        status.next_run
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

/// `next_run` is the cron's own next minute, whatever the schedule's shape: both day fields set (OR),
/// steps, Sunday as 0 and as 7, lists.
#[skuld::test]
fn next_run_agrees_with_the_cron(#[fixture(systemd_group)] _group: &Group) {
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

/// An install that changes the schedule of an armed task is enough to move its next run.
#[skuld::test]
fn install_over_a_task_with_a_changed_schedule_rearms_it(#[fixture(systemd_group)] _group: &Group) {
    let id = support::random_test_id();
    let _guard = ServiceGuard::new(&id);
    let task_id = id_of(&id);
    let mgr = Systemd::new();
    let command = vec!["/bin/true".to_string()];
    mgr.install(&task(&id, command.clone(), Some("0 3 * * *")), false)
        .expect("install");
    mgr.enable(&task_id).expect("enable");

    let before = SystemTime::now();
    let outcome = mgr
        .install(&task(&id, command, Some("0 5 * * *")), false)
        .expect("install a new schedule");
    let status = task_status(&mgr, &task_id);
    let after = SystemTime::now();

    assert!(matches!(outcome, Outcome::Update { .. }), "{outcome:?}");
    assert!(status.common.enabled, "{status:?}");
    assert_next_run(&status, "0 5 * * *", &task_id, before, after);
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

/// An exit 0 unloads the unit, and the journal still says how the run went and when.
#[skuld::test]
fn status_after_a_successful_run_reads_exit_0_from_the_journal(#[fixture(systemd_group)] _group: &Group) {
    let id = support::random_test_id();
    let _guard = ServiceGuard::new(&id);
    let task_id = id_of(&id);
    let mgr = Systemd::new();
    mgr.install(&task(&id, vec!["/bin/true".to_string()], None), false)
        .expect("install");
    let before = SystemTime::now();

    mgr.start(&task_id, Budget::DEFAULT).expect("start");

    let status = status_with_result(&mgr, &task_id);
    assert_eq!(status.last_result, Some(Exit::Code(0)), "{status:?}");
    assert!(status.last_run.is_some_and(|at| at >= before), "{status:?}");
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
    struct Lines(mpsc::Sender<Vec<u8>>);
    impl Write for Lines {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.send(buf.to_vec()).map_err(std::io::Error::other)?;
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

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
        let (exit, out) = run_task(&mgr, &task_id);

        assert_eq!(exit, Exit::Code(0));
        assert_same_text(n, &out, &want);
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

/// A run starts right behind a chatty one (the first run's lines can still be on their way to the
/// journal) and shows none of its lines.
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
        let (_, out) = run_task(&mgr, &task_id);
        assert_same_text(n, &out, &first);

        let (exit, out) = run_task(&mgr, &task_id);
        assert_eq!(exit, Exit::Code(0));
        assert_same_text(n, &out, b"second-run\n");
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
