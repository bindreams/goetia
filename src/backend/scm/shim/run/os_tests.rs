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

use crate::spec::test_support::plain_fields;
use crate::spec::{DaemonFields, DaemonSpec};
use cosca::identity::{Liveness, Resolved};
use cosca::{Process, Stdio};
use skuld::temp_dir;
use windows_sys::Win32::Foundation::{ERROR_ACCESS_DENIED, ERROR_FILE_NOT_FOUND, ERROR_PATH_NOT_FOUND};

use super::*;
use crate::backend::scm::shim::run::os::fake::FakeGuard;
use crate::backend::scm::shim::run::service::build_command;
use crate::backend::scm::shim::run::test_support::not_found;

/// Holds a real daemon and ends it when dropped, so a failing test leaves no sleeper behind. Made
/// before the spawn it will hold.
#[derive(Default)]
struct Reap {
    child: Option<Box<dyn Spawned>>,
    /// A daemon whose `Spawned` was detached: held by identity.
    detached: Option<Process>,
}

impl Reap {
    fn hold(&mut self, child: Box<dyn Spawned>) -> &dyn Spawned {
        &**self.child.insert(child)
    }

    fn hold_detached(&mut self, process: Process) -> &Process {
        self.detached.insert(process)
    }
}

impl Drop for Reap {
    fn drop(&mut self) {
        if let Some(Err(e)) = self.child.as_ref().map(|child| child.kill_tree()) {
            eprintln!("Reap: left a daemon running: {e}");
        }
        if let Some(Err(e)) = self.detached.as_ref().map(Process::kill) {
            eprintln!("Reap: left a daemon running: {e}");
        }
    }
}

fn powershell(script: &str) -> cosca::Command {
    let mut cmd = cosca::run(["powershell", "-NoProfile", "-NonInteractive", "-Command", script]);
    cmd.contain();
    cmd
}

// Spawned -------------------------------------------------------------------------------------------------------------

/// The grandchild is the part `kill` cannot reach: only the Job Object `kill_tree` terminates holds
/// it. Its pid arrives on the daemon's stdout, which is the event the test waits for.
#[skuld::test]
fn kill_tree_ends_a_job_contained_daemon_and_its_grandchild() {
    let (reader, writer) = std::io::pipe().expect("a pipe for the grandchild's pid");
    let mut cmd = powershell("& powershell -NoProfile -NonInteractive -Command '$PID; Start-Sleep -Seconds 300'");
    cmd.stdout(Stdio::from_file(File::from(OwnedHandle::from(writer))))
        .expect("redirect the daemon's stdout to the pipe");
    let mut reap = Reap::default();
    let daemon = reap.hold(Real.spawn(cmd).expect("spawn the daemon"));
    let mut line = String::new();
    BufReader::new(reader)
        .read_line(&mut line)
        .expect("the grandchild's pid line");
    let pid: u32 = line.trim().parse().expect("the line is a pid");
    let Resolved::Found(grandchild) = Process::from_pid(pid) else {
        panic!("the grandchild {pid} is not running");
    };
    assert_eq!(grandchild.is_alive(), Liveness::Alive, "precondition");

    daemon.kill_tree().expect("kill the tree");

    grandchild.wait().expect("the grandchild ends once the tree is killed");
    daemon.wait().expect("the daemon ends once the tree is killed");
}

/// The direct-child kill, which `kill_tree` ends in: the daemon ends, and `wait` returns.
#[skuld::test]
fn kill_ends_a_real_daemon() {
    let mut reap = Reap::default();
    let daemon = reap.hold(
        Real.spawn(powershell("Start-Sleep -Seconds 300"))
            .expect("spawn the daemon"),
    );

    daemon.kill().expect("kill the daemon");

    let status = daemon.wait().expect("the daemon ends once it is killed");
    assert!(!status.success(), "a killed daemon exited cleanly");
}

/// Dropping the `Spawned` would kill the daemon's job. The daemon echoes its stdin, so a line
/// written after the detach and read back proves it still ran then: for a daemon the job had
/// ended, the write fails or the read sees end of file. The test then ends it through the `Reap`.
/// This pins `Real::detach`'s delegation: without it `Child::drop` kills and waits before
/// returning. Whether cosca's disarm itself holds is cosca's to test.
#[skuld::test]
fn detach_leaves_the_daemon_running_after_its_handle_is_gone() {
    let mut reap = Reap::default();
    let mut cmd = powershell("while (($l = [Console]::In.ReadLine()) -ne $null) { [Console]::Out.WriteLine($l) }");
    cmd.stdin(Stdio::pipe()).expect("pipe the daemon's stdin");
    cmd.stdout(Stdio::pipe()).expect("pipe the daemon's stdout");
    let mut child = cmd.spawn().expect("spawn the daemon");
    reap.hold_detached(Process::from_id(child.id()));
    let mut stdin = child.stdin().expect("the daemon's stdin");
    let mut stdout = BufReader::new(child.stdout().expect("the daemon's stdout"));

    Spawned::detach(Box::new(child));

    if let Err(e) = stdin.write_all(b"ping\n") {
        panic!("the daemon did not outlive its handle: the write to its stdin failed: {e}");
    }
    let mut echo = String::new();
    stdout.read_line(&mut echo).expect("read the daemon's echo");
    assert_eq!(echo.trim_end(), "ping", "the daemon did not outlive its handle");
    // Killed before its stdin closes, which would end it on its own and race the kill.
    drop(reap);
}

/// The supervisor asks for the status twice: the waiter's wait, then the one that reads the code.
#[skuld::test]
fn wait_gives_the_same_exit_code_every_time() {
    let mut reap = Reap::default();
    let daemon = reap.hold(Real.spawn(powershell("exit 3")).expect("spawn the daemon"));

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
        let refused = Real
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

#[skuld::test]
fn the_os_error_of_an_io_failure_is_kept_and_other_failures_have_none() {
    assert_eq!(
        os_error_of(&cosca::error::Error::Io(std::io::Error::from_raw_os_error(
            ERROR_ACCESS_DENIED as i32
        ))),
        Some(ERROR_ACCESS_DENIED as i32)
    );
    // A failure without a code has none, whatever its kind.
    assert_eq!(os_error_of(&not_found()), None);
    assert_eq!(
        os_error_of(&cosca::error::Error::Unassessable {
            detail: "query".to_string(),
            source: Some(std::io::Error::from(std::io::ErrorKind::NotFound)),
        }),
        None
    );
    assert_eq!(
        os_error_of(&cosca::error::Error::Io(std::io::Error::other("no os code"))),
        None
    );
    assert_eq!(
        os_error_of(&cosca::error::Error::Containment {
            detail: "attach".to_string()
        }),
        None
    );
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
    // `DaemonSpec::new` wants an absolute program; `ComSpec` is the path of `cmd.exe`.
    let comspec = std::env::var("ComSpec").expect("ComSpec names cmd.exe");
    let command = [
        comspec.as_str(),
        "/c",
        "echo %CD%& echo %GOETIA_TEST_VAR%& echo to-stderr>&2",
    ];
    let spec = DaemonSpec::new(DaemonFields {
        cwd: Some(cwd.clone()),
        env: BTreeMap::from([("GOETIA_TEST_VAR".to_string(), "from-spec".to_string())]),
        logs: Some(logs.clone()),
        ..plain_fields(ID, command.map(str::to_string).to_vec())
    })
    .unwrap();
    let fake = FakeGuard::new();
    let mut reap = Reap::default();

    let daemon = reap.hold(
        Real.spawn(build_command(&spec, &Real, &fake.log(), ID))
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

// Channels ------------------------------------------------------------------------------------------------------------

#[skuld::test]
fn append_creates_missing_directories_and_writes_the_line(#[fixture(temp_dir)] dir: &Path) {
    let path = dir.join("Goetia").join("logs").join("x.log");

    Real.append(&path, "the line").expect("append");

    assert_eq!(
        std::fs::read_to_string(&path).expect("the file was written"),
        "the line\n"
    );
}

#[skuld::test]
fn append_appends_rather_than_truncates(#[fixture(temp_dir)] dir: &Path) {
    let path = dir.join("x.log");

    Real.append(&path, "first").expect("append");
    Real.append(&path, "second").expect("append");

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
    let result = Real.append(&file.join("x.log"), "lost");

    assert!(result.is_err());
    assert_eq!(std::fs::read_to_string(&file).expect("the file is untouched"), "x");
}

// Os ------------------------------------------------------------------------------------------------------------------

#[skuld::test]
fn open_log_fails_with_the_os_reason_when_the_path_cannot_be_opened(#[fixture(temp_dir)] dir: &Path) {
    let file = dir.join("a-file");
    std::fs::write(&file, "x").expect("a regular file to use as a parent");

    // A directory cannot be created under a regular file.
    let refused = Real
        .open_log(&file.join("daemon.log"))
        .expect_err("a log under a regular file");

    assert!(refused.raw_os_error().is_some(), "no OS reason: {refused:?}");
    assert_eq!(std::fs::read_to_string(&file).expect("the file is untouched"), "x");
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
