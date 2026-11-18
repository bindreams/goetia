//! `Real` against the real system: what goetia owns over `cosca` and the file system. Every fact
//! about the supervisor's decisions is stated over the `Fake` in `stop_bus_tests` and
//! `service_tests`.
//!
//! The Event Log half of `Real` is covered by the shim integration tests: a unit test never writes
//! the real Event Log.

use std::collections::BTreeMap;
use std::fs::File;
use std::io::{BufRead as _, BufReader};
use std::os::windows::io::OwnedHandle;
use std::path::PathBuf;
use std::sync::Arc;

use crate::spec::{DaemonFields, DaemonSpec, ServiceFields, ServiceSpec, TaskFields, TaskSpec};
use crate::testing::base_fields;
use cosca::identity::Liveness;
use cosca::{Process, Stdio};
use skuld::temp_dir;
use windows_sys::Win32::Foundation::{ERROR_ACCESS_DENIED, ERROR_FILE_NOT_FOUND, ERROR_PATH_NOT_FOUND};

use super::*;
use crate::backend::scm::shim::run::logging::Log;
use crate::backend::scm::shim::run::os::fake::FakeGuard;
use crate::backend::scm::shim::run::service::build_command;
use crate::backend::scm::shim::run::test_support::not_found;
use crate::backend::scm::sid::sid_display;
use crate::backend::scm::test_support::{permit_only, withdraw_append};
use crate::elevation::test_support::process_user_sid;

/// Holds a real daemon and ends it when dropped, so a failing test leaves no sleeper behind. Made
/// before the spawn it will hold.
#[derive(Default)]
struct Reap {
    child: Option<Box<dyn Spawned>>,
}

impl Reap {
    fn hold(&mut self, child: Box<dyn Spawned>) -> &dyn Spawned {
        &**self.child.insert(child)
    }
}

impl Drop for Reap {
    fn drop(&mut self) {
        if let Some(Err(e)) = self.child.as_ref().map(|child| child.send_kill_tree()) {
            eprintln!("Reap: left a daemon running: {}", cosca::error::Report::new(&e));
        }
    }
}

fn powershell(script: &str) -> cosca::Command {
    let mut cmd = cosca::run(["powershell", "-NoProfile", "-NonInteractive", "-Command", script]);
    cmd.contain();
    cmd
}

// Spawned -------------------------------------------------------------------------------------------------------------

/// The grandchild is the part `kill` cannot reach: only the Job Object `send_kill_tree` terminates holds
/// it. Its pid arrives on the daemon's stdout, which is the event the test waits for.
#[skuld::test]
fn send_kill_tree_ends_a_job_contained_daemon_and_its_grandchild() {
    let (reader, writer) = std::io::pipe().expect("a pipe for the grandchild's pid");
    let mut cmd = powershell("& powershell -NoProfile -NonInteractive -Command '$PID; Start-Sleep -Seconds 300'");
    cmd.stdout(Stdio::from_file(File::from(OwnedHandle::from(writer))))
        .expect("redirect the daemon's stdout to the pipe");
    let mut reap = Reap::default();
    let daemon = reap.hold(Real::default().spawn(cmd).expect("spawn the daemon"));
    let mut line = String::new();
    BufReader::new(reader)
        .read_line(&mut line)
        .expect("the grandchild's pid line");
    let pid: u32 = line.trim().parse().expect("the line is a pid");
    let Some(grandchild) = Process::open_pid(pid).expect("open the grandchild") else {
        panic!("the grandchild {pid} is not running");
    };
    assert_eq!(grandchild.is_alive(), Liveness::Alive, "precondition");

    daemon.send_kill_tree().expect("kill the tree");

    grandchild.wait().expect("the grandchild ends once the tree is killed");
    daemon.wait().expect("the daemon ends once the tree is killed");
}

/// The direct-child kill, which `send_kill_tree` ends in: the daemon ends, and `wait` returns.
#[skuld::test]
fn kill_ends_a_real_daemon() {
    let mut reap = Reap::default();
    let daemon = reap.hold(
        Real::default()
            .spawn(powershell("Start-Sleep -Seconds 300"))
            .expect("spawn the daemon"),
    );

    daemon.kill().expect("kill the daemon");

    let status = daemon.wait().expect("the daemon ends once it is killed");
    assert!(!status.success(), "a killed daemon exited cleanly");
}

/// The supervisor asks for the status twice: the waiter's wait, then the one that reads the code.
#[skuld::test]
fn wait_gives_the_same_exit_code_every_time() {
    let mut reap = Reap::default();
    let daemon = reap.hold(Real::default().spawn(powershell("exit 3")).expect("spawn the daemon"));

    let first = daemon.wait().expect("the first wait");
    let second = daemon.wait().expect("the second wait");

    assert_eq!((first.code(), second.code()), (Some(3), Some(3)));
}

/// A missing binary reaches SCM as `ERROR_FILE_NOT_FOUND` either way it is named. A bare name is
/// searched for by `std` before `CreateProcessW` is called, and its miss is `Io(NotFound)` with no
/// OS error code, so `spawn_error_code` supplies it; an absolute path fails in
/// `CreateProcessW` with the code.
#[skuld::test]
fn a_spawn_of_a_missing_binary_is_refused_as_not_found(#[fixture(temp_dir)] dir: &Path) {
    let absolute = dir.join("goetia-shim-test-no-such-binary.exe");
    let os_code = [
        (Path::new("goetia-shim-test-no-such-binary"), None),
        (absolute.as_path(), Some(ERROR_FILE_NOT_FOUND as i32)),
    ];
    for (missing, raw) in os_code {
        let refused = Real::default()
            .spawn(cosca::run([missing]))
            .err()
            .expect("the binary does not exist");

        assert!(
            matches!(&refused, cosca::error::Error::Io(e) if e.kind() == io::ErrorKind::NotFound && e.raw_os_error() == raw),
            "{missing:?}: {refused:?}"
        );
        assert_eq!(
            spawn_error_code(&refused),
            Some(ERROR_FILE_NOT_FOUND as i32),
            "{missing:?}: {refused:?}"
        );
    }
}

// Error codes ---------------------------------------------------------------------------------------------------------

/// cosca wraps every failure after `CreateProcess` in `MayHaveStarted`; the code SCM is told is the
/// wrapped failure's.
#[skuld::test]
fn a_failure_after_the_spawn_is_told_to_scm_with_the_code_of_its_source() {
    let after_spawn = cosca::error::Error::MayHaveStarted {
        fate: cosca::error::ChildFate::Unknown,
        wrapper_elevated: false,
        detail: None,
        source: Arc::new(cosca::error::Error::Io(std::io::Error::from_raw_os_error(
            ERROR_ACCESS_DENIED as i32,
        ))),
        unreaped: None,
        cleanup: None,
    };

    assert_eq!(spawn_error_code(&after_spawn), Some(ERROR_ACCESS_DENIED as i32));
}

/// `within` keeps the kind but not the code on its layer: a `NotFound` with no code of its own, over
/// one that has it.
#[skuld::test]
fn a_code_on_the_chain_wins_over_a_code_less_not_found_above_it() {
    let wrapped = cosca::error::Error::Io(crate::error::within(
        "resolve the program",
        std::io::Error::from_raw_os_error(ERROR_PATH_NOT_FOUND as i32),
    ));

    assert_eq!(spawn_error_code(&wrapped), Some(ERROR_PATH_NOT_FOUND as i32));
}

#[skuld::test]
fn a_refused_spawn_without_a_code_that_is_not_found_is_told_to_scm_as_file_not_found() {
    assert_eq!(spawn_error_code(&not_found()), Some(ERROR_FILE_NOT_FOUND as i32));
    // A real code wins over the kind's: ERROR_PATH_NOT_FOUND is `NotFound` too.
    assert_eq!(
        spawn_error_code(&cosca::error::Error::Io(std::io::Error::from_raw_os_error(
            ERROR_PATH_NOT_FOUND as i32
        ))),
        Some(ERROR_PATH_NOT_FOUND as i32)
    );
    assert_eq!(
        spawn_error_code(&cosca::error::Error::Io(std::io::Error::other("no os code"))),
        None
    );
}

// build_command -------------------------------------------------------------------------------------------------------

const ID: &str = "goetia-shim-os-test";

/// What `build_command` sets up, observed from a real daemon spawned through `Real`: the Job Object
/// `StopBus` kills the tree through, stdout and stderr both in `spec.logs`, `cwd` and `env`. The
/// daemon prints its working directory and the variable, writes a line to stderr and exits. The
/// log is read as a set of lines: the order across the two streams is not fixed.
#[skuld::test]
fn build_command_makes_a_job_contained_daemon_with_its_cwd_env_and_log(#[fixture(temp_dir)] dir: &Path) {
    const CWD_LEAF: &str = "goetia-cwd-probe";
    let cwd = dir.join(CWD_LEAF);
    std::fs::create_dir(&cwd).expect("the daemon's working directory");
    let logs = dir.join("daemon.log");
    // `ServiceSpec::new` wants an absolute program; `ComSpec` is the path of `cmd.exe`.
    let comspec = std::env::var("ComSpec").expect("ComSpec names cmd.exe");
    let command = [
        comspec.as_str(),
        "/c",
        "echo %CD%& echo %GOETIA_TEST_VAR%& echo to-stderr>&2",
    ];
    let spec = ServiceSpec::new({
        let base = base_fields(ID, command.map(str::to_string).to_vec());
        ServiceFields {
            common: DaemonFields {
                cwd: Some(cwd.clone()),
                env: BTreeMap::from([("GOETIA_TEST_VAR".to_string(), "from-spec".to_string())]),
                logs: Some(logs.clone()),
                ..base.common
            },
            ..base
        }
    })
    .map(DaemonSpec::from)
    .unwrap();
    let fake = FakeGuard::new();
    let mut reap = Reap::default();

    let daemon = reap.hold(
        Real::default()
            .spawn(build_command(&spec, &Real::default(), &fake.log(), ID))
            .expect("spawn the daemon"),
    );
    let contained = daemon.containment();
    daemon.wait().expect("the daemon exits");

    assert_eq!(contained, Containment::JobObject);
    let log = std::fs::read_to_string(&logs).expect("the daemon's log");
    let lines: Vec<&str> = log.lines().map(str::trim).collect();
    assert_eq!(
        lines.len(),
        3,
        "the daemon's log is not exactly its three lines: {log:?}"
    );
    assert!(
        lines.iter().any(|line| line.ends_with(CWD_LEAF)),
        "the daemon ran outside its `cwd`: {log:?}"
    );
    assert!(
        lines.contains(&"from-spec"),
        "the daemon did not get its `env`: {log:?}"
    );
    assert!(
        lines.contains(&"to-stderr"),
        "stderr is not in the daemon's log: {log:?}"
    );
}

/// A task that runs `cmd /c <script>` as root, with the `cwd`, `env` and `logs` given.
fn task_running(
    script: &str,
    cwd: Option<PathBuf>,
    env: BTreeMap<String, String>,
    logs: Option<PathBuf>,
) -> DaemonSpec {
    let comspec = std::env::var("ComSpec").expect("ComSpec names cmd.exe");
    let base = base_fields(ID, vec![comspec, "/c".to_string(), script.to_string()]);
    TaskSpec::new(TaskFields {
        common: DaemonFields {
            cwd,
            env,
            logs,
            ..base.common
        },
        schedule: None,
    })
    .map(DaemonSpec::from)
    .unwrap()
}

/// The program's own variables and directory are the spec's, over what the shim inherited: a task
/// starts in `system32` with the machine's variables, and the spec's `env` replaces one of them and
/// adds another. The program's output is in the spec's `logs`.
#[skuld::test]
fn a_task_programs_env_and_cwd_are_the_specs_over_the_inherited_ones(#[fixture(temp_dir)] dir: &Path) {
    let cwd = dir.join("goetia-task-cwd");
    std::fs::create_dir(&cwd).expect("the program's working directory");
    let logs = dir.join("task.log");
    // `USERNAME` is there in every session that runs the tests: the spec's value replaces it.
    let env = BTreeMap::from([
        ("USERNAME".to_string(), "from-spec".to_string()),
        ("GOETIA_TEST_VAR".to_string(), "added".to_string()),
    ]);
    let spec = task_running(
        "echo %CD%& echo %USERNAME%& echo %GOETIA_TEST_VAR%",
        Some(cwd),
        env,
        Some(logs.clone()),
    );
    let fake = FakeGuard::new();
    let mut reap = Reap::default();

    let program = reap.hold(
        Real::default()
            .spawn(build_command(&spec, &Real::default(), &fake.log(), ID))
            .expect("spawn the program"),
    );
    program.wait().expect("the program exits");

    let log = std::fs::read_to_string(&logs).expect("the program's log");
    let lines: Vec<&str> = log.lines().map(str::trim).collect();
    assert_eq!(lines.len(), 3, "{log:?}");
    assert!(
        lines[0].ends_with("goetia-task-cwd"),
        "the program ran outside its `cwd`: {log:?}"
    );
    assert_eq!(&lines[1..], ["from-spec", "added"], "{log:?}");
}

/// A task with no `logs` has its output discarded: no log is made for it, not even the default one a
/// service gets, and nothing is reported of that.
#[skuld::test]
fn a_task_with_no_logs_leaves_no_log_and_reports_nothing(#[fixture(temp_dir)] dir: &Path) {
    let defaults = dir.join("default-logs");
    let fake = FakeGuard::new();
    let log = Arc::new(Log::new(fake.channels(), defaults.clone()));
    let spec = task_running("echo discarded", None, BTreeMap::new(), None);
    let mut reap = Reap::default();

    let program = reap.hold(
        Real::default()
            .spawn(build_command(&spec, &Real::default(), &log, ID))
            .expect("spawn the program"),
    );
    program.wait().expect("the program exits");

    assert!(!defaults.exists(), "a default log was made for a task with no `logs`");
    assert_eq!(
        fake.take_channel_calls(),
        vec![],
        "something was reported of the missing log"
    );
}

// Channels ------------------------------------------------------------------------------------------------------------

#[skuld::test]
fn append_creates_missing_directories_and_writes_the_line(#[fixture(temp_dir)] dir: &Path) {
    let path = dir.join("Goetia").join("logs").join("x.log");

    Real::default().append(&path, "the line", LogKind::Own).expect("append");

    assert_eq!(
        std::fs::read_to_string(&path).expect("the file was written"),
        "the line\n"
    );
}

/// The shim makes nothing under `Goetia` (an elevated install made the folder and the file, and judged
/// them): its default log, opened as [`LogKind::Default`], is an append failure when it is not there, with
/// the folder or without it, and nothing is made.
#[skuld::test]
fn append_makes_nothing_of_the_default_log_and_fails_when_it_is_missing(#[fixture(temp_dir)] dir: &Path) {
    let missing = dir.join("Goetia").join("logs");

    let result = Real::default().append(&missing.join("x.log"), "the line", LogKind::Default);

    assert_eq!(result.expect_err("no folder").kind(), io::ErrorKind::NotFound);
    assert!(!dir.join("Goetia").exists(), "a folder was made");

    std::fs::create_dir_all(&missing).expect("the folder an install made");
    let result = Real::default().append(&missing.join("x.log"), "the line", LogKind::Default);
    assert_eq!(result.expect_err("no file").kind(), io::ErrorKind::NotFound);
    assert!(!missing.join("x.log").exists(), "the file was made");

    std::fs::write(missing.join("x.log"), "").expect("the file an install made");
    Real::default()
        .append(&missing.join("x.log"), "the line", LogKind::Default)
        .expect("append where the file is");
    assert_eq!(
        std::fs::read_to_string(missing.join("x.log")).expect("the file was written"),
        "the line\n"
    );
}

/// A default log that is another name for a file (a bit-29 tag) is not what the install made: the line is
/// not written, and the error names the file.
#[skuld::test]
fn append_refuses_a_default_log_that_is_a_link(#[fixture(temp_dir)] dir: &Path) {
    let log = dir.join("x.log");
    std::fs::write(&log, "").expect("the file an install made");
    crate::backend::scm::test_support::set_reparse_point(&log, 0xA000_0001, b"x");

    let refused = Real::default()
        .append(&log, "the line", LogKind::Default)
        .expect_err("a link");

    assert!(refused.to_string().contains(&log.display().to_string()), "{refused}");
}

/// The shim holds its default log from its start: a line is written through that handle, whatever has
/// happened to the path since (here, the file is moved; a line written by opening the path again would be
/// lost or would make a new file).
#[skuld::test]
fn a_held_default_log_is_appended_through_the_handle_that_was_held(#[fixture(temp_dir)] dir: &Path) {
    let log = dir.join("x.log");
    let moved = dir.join("moved.log");
    std::fs::write(&log, "first\n").expect("the file an install made");
    let real = Real::default();
    real.hold_default(&log).expect("hold the default log");
    std::fs::rename(&log, &moved).expect("move the file under the handle");

    real.append(&log, "the line", LogKind::Default)
        .expect("append through the held handle");

    assert_eq!(
        std::fs::read_to_string(&moved).expect("the held file"),
        "first\nthe line\n"
    );
    assert!(!log.exists(), "the path was opened again");
}

/// The handle a shim holds is its default log's for the whole run: a use of another default path opens that
/// path and does not give up the held one, so the shim's lines still go through it.
#[skuld::test]
fn using_another_default_path_keeps_the_log_that_was_held(#[fixture(temp_dir)] dir: &Path) {
    let held = dir.join("held.log");
    let other = dir.join("other.log");
    let moved = dir.join("moved.log");
    std::fs::write(&held, "").expect("the file an install made");
    std::fs::write(&other, "").expect("another file an install made");
    let real = Real::default();
    real.hold_default(&held).expect("hold the default log");

    real.append(&other, "elsewhere", LogKind::Default)
        .expect("a line in the other log");
    std::fs::rename(&held, &moved).expect("move the held file under the handle");
    real.append(&held, "the line", LogKind::Default)
        .expect("append through the handle that is still held");

    assert_eq!(std::fs::read_to_string(&moved).expect("the held file"), "the line\n");
    assert_eq!(std::fs::read_to_string(&other).expect("the other"), "elsewhere\n");
}

/// Holding makes nothing, as appending the default log does not: a log that is not there is
/// [`NotFound`](io::ErrorKind::NotFound), and a line for it is then tried by its path.
#[skuld::test]
fn holding_the_default_log_makes_nothing_and_fails_when_it_is_missing(#[fixture(temp_dir)] dir: &Path) {
    let missing = dir.join("Goetia").join("logs").join("x.log");
    let real = Real::default();

    let refused = real.hold_default(&missing).expect_err("no folder, no file");

    assert_eq!(refused.kind(), io::ErrorKind::NotFound);
    assert!(!dir.join("Goetia").exists(), "a folder was made");
    assert_eq!(
        real.append(&missing, "the line", LogKind::Default)
            .expect_err("nothing held, nothing there")
            .kind(),
        io::ErrorKind::NotFound
    );
}

/// An account change takes the shim's account out of the log's permissions while the old shim still runs
/// (SCM does not restart a service it updates). Its own lines still reach the log, through the handle it
/// opened at its start; a shim that opened the path for each line would lose them.
#[skuld::test]
fn the_shims_own_lines_after_its_account_was_taken_out_land_in_the_held_log(#[fixture(temp_dir)] dir: &Path) {
    let log = dir.join("x.log");
    let me = sid_display(&process_user_sid());
    std::fs::write(&log, "").expect("the file an install made");
    permit_only(&log, &me);
    let real = Real::default();
    real.hold_default(&log).expect("hold the default log");

    withdraw_append(&log, &me);

    real.append(&log, "the line", LogKind::Default)
        .expect("a line through the held handle");
    let reopened = Real::default().append(&log, "lost", LogKind::Default);
    assert_eq!(
        reopened.expect_err("a shim that opens it for each line").kind(),
        io::ErrorKind::PermissionDenied
    );
    permit_only(&log, &me);
    assert_eq!(std::fs::read_to_string(&log).expect("the log"), "the line\n");
}

/// The daemon's output and the shim's own lines are one handle: the log the shim held at its start is what
/// [`Os::open_log`] hands the daemon, so an account change that takes the account out of the file's permissions
/// while the service starts cannot leave the daemon's output without its log. A shim that opened the path
/// again would be refused.
#[skuld::test]
fn the_daemons_output_goes_through_the_handle_the_log_held(#[fixture(temp_dir)] dir: &Path) {
    let log_file = dir.join("svc.log");
    let me = sid_display(&process_user_sid());
    std::fs::write(&log_file, "").expect("the file an install made");
    permit_only(&log_file, &me);
    let real = Arc::new(Real::default());
    let log = Log::new(real.clone(), dir.to_path_buf());
    log.hold_default("svc");

    withdraw_append(&log_file, &me);

    let mut output = real
        .open_log(&log_file, LogKind::Default)
        .expect("the held handle, for the daemon");
    output.write_all(b"output\n").expect("the daemon writes");
    real.append(&log_file, "the line", LogKind::Default)
        .expect("a line through the same handle");
    let reopened = Real::default().open_log(&log_file, LogKind::Default);
    assert_eq!(
        reopened.expect_err("a shim that opens the path again").kind(),
        io::ErrorKind::PermissionDenied
    );
    permit_only(&log_file, &me);
    assert_eq!(
        std::fs::read_to_string(&log_file).expect("the log"),
        "output\nthe line\n"
    );
}

/// The README's recovery deletes `<id>.log` and installs again while a shim may run. The held handle is then
/// on a file with no name, which no one reads: the next line opens the path, which an install made again, and
/// holds that file.
#[skuld::test]
fn a_held_default_log_that_was_deleted_is_opened_again_for_the_next_line(#[fixture(temp_dir)] dir: &Path) {
    let log = dir.join("x.log");
    std::fs::write(&log, "old\n").expect("the file an install made");
    let real = Real::default();
    real.hold_default(&log).expect("hold the default log");
    std::fs::remove_file(&log).expect("delete the log");
    std::fs::write(&log, "").expect("the install makes it again");

    real.append(&log, "first", LogKind::Default)
        .expect("a line, in the new file");
    real.append(&log, "second", LogKind::Default)
        .expect("another, through the handle it holds now");

    assert_eq!(std::fs::read_to_string(&log).expect("the new log"), "first\nsecond\n");
}

/// A delete that waits for the last handle (the name stays until it is closed) leaves the held handle as the one
/// thing that keeps the name. It is released before the path is opened again, so that an open that fails leaves
/// nothing holding the name, and the install that makes the log again can.
#[skuld::test]
fn a_held_default_log_that_was_deleted_is_released_before_it_is_opened_again(#[fixture(temp_dir)] dir: &Path) {
    use std::os::windows::fs::OpenOptionsExt as _;
    use windows_sys::Win32::Storage::FileSystem::{DELETE, FILE_FLAG_DELETE_ON_CLOSE};

    let log = dir.join("x.log");
    std::fs::write(&log, "").expect("the file an install made");
    let real = Real::default();
    real.hold_default(&log).expect("hold the default log");
    // Closed at once: the delete is pending on the held handle.
    OpenOptions::new()
        .access_mode(DELETE)
        .custom_flags(FILE_FLAG_DELETE_ON_CLOSE)
        .open(&log)
        .expect("mark the file for deletion");

    real.append(&log, "lost", LogKind::Default)
        .expect_err("nothing is there to write");

    std::fs::write(&log, "").expect("the install makes the log again: the held handle keeps no name");
}

/// The same for the daemon's output: a daemon started after the recovery writes the new file.
#[skuld::test]
fn the_daemons_output_after_the_held_log_was_deleted_goes_to_the_new_file(#[fixture(temp_dir)] dir: &Path) {
    let log = dir.join("x.log");
    std::fs::write(&log, "").expect("the file an install made");
    let real = Real::default();
    real.hold_default(&log).expect("hold the default log");
    std::fs::remove_file(&log).expect("delete the log");
    std::fs::write(&log, "").expect("the install makes it again");

    let mut output = real.open_log(&log, LogKind::Default).expect("the new file");
    output.write_all(b"output\n").expect("the daemon writes");

    assert_eq!(std::fs::read_to_string(&log).expect("the new log"), "output\n");
}

/// A deleted log that no install has made again is not written into the file that has no name, and makes
/// nothing; once it is made, the next line lands in it.
#[skuld::test]
fn a_held_default_log_that_was_deleted_is_not_written_until_it_is_there_again(#[fixture(temp_dir)] dir: &Path) {
    let log = dir.join("x.log");
    std::fs::write(&log, "").expect("the file an install made");
    let real = Real::default();
    real.hold_default(&log).expect("hold the default log");
    std::fs::remove_file(&log).expect("delete the log");

    let refused = real
        .append(&log, "lost", LogKind::Default)
        .expect_err("nothing is there to write");

    assert_eq!(refused.kind(), io::ErrorKind::NotFound);
    assert!(!log.exists(), "the shim made the file");
    std::fs::write(&log, "").expect("the install makes it again");
    real.append(&log, "the line", LogKind::Default)
        .expect("the new file, once it is there");
    assert_eq!(std::fs::read_to_string(&log).expect("the new log"), "the line\n");
}

#[skuld::test]
fn append_appends_rather_than_truncates(#[fixture(temp_dir)] dir: &Path) {
    let path = dir.join("x.log");

    Real::default().append(&path, "first", LogKind::Own).expect("append");
    Real::default().append(&path, "second", LogKind::Own).expect("append");

    assert_eq!(
        std::fs::read_to_string(&path).expect("the file was written"),
        "first\nsecond\n"
    );
}

#[skuld::test]
fn append_fails_and_writes_nothing_when_the_path_cannot_be_opened(#[fixture(temp_dir)] dir: &Path) {
    let file = dir.join("a-file");
    std::fs::write(&file, "x").expect("a regular file to use as a parent");

    // A directory cannot be created under a regular file.
    let result = Real::default().append(&file.join("x.log"), "lost", LogKind::Own);

    assert!(result.is_err());
    assert_eq!(std::fs::read_to_string(&file).expect("the file is untouched"), "x");
}

// Os ------------------------------------------------------------------------------------------------------------------

#[skuld::test]
fn open_log_fails_with_the_os_reason_when_the_path_cannot_be_opened(#[fixture(temp_dir)] dir: &Path) {
    let file = dir.join("a-file");
    std::fs::write(&file, "x").expect("a regular file to use as a parent");

    // A directory cannot be created under a regular file.
    let refused = Real::default()
        .open_log(&file.join("daemon.log"), LogKind::Own)
        .expect_err("a log under a regular file");

    assert!(refused.raw_os_error().is_some(), "no OS reason: {refused:?}");
    assert_eq!(std::fs::read_to_string(&file).expect("the file is untouched"), "x");
}

/// The daemon's default output is opened as the default failure log is: no folder and no file is made
/// for it. A `logs:` path of the user's own is made, with its folders.
#[skuld::test]
fn open_log_makes_nothing_of_the_default_log(#[fixture(temp_dir)] dir: &Path) {
    let log = dir.join("Goetia").join("logs").join("daemon.log");

    let refused = Real::default().open_log(&log, LogKind::Default).expect_err("no folder");

    assert_eq!(refused.kind(), io::ErrorKind::NotFound);
    assert!(!dir.join("Goetia").exists(), "a folder was made");
    Real::default()
        .open_log(&log, LogKind::Own)
        .expect("a user's own `logs` parents are made");
    assert!(log.is_file());
}

/// Records each `write` call; fails them all when `fail` is set.
#[derive(Default)]
struct Recorder {
    writes: Vec<Vec<u8>>,
    fail: bool,
}

impl std::io::Write for Recorder {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        if self.fail {
            return Err(std::io::Error::other("disk full"));
        }
        self.writes.push(buf.to_vec());
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[skuld::test]
fn append_line_is_one_write_so_daemon_output_cannot_land_between_text_and_newline() {
    let mut out = Recorder::default();

    append_line(&mut out, "the line").expect("the write succeeds");

    assert_eq!(out.writes, vec![b"the line\n".to_vec()]);
}

#[skuld::test]
fn append_line_reports_a_failed_write() {
    let mut out = Recorder {
        fail: true,
        ..Recorder::default()
    };

    assert!(append_line(&mut out, "the line").is_err());
}
