//! `Real` against the real system: what goetia owns over `cosca` and the file system. Every fact
//! about the supervisor's decisions is stated over the `Fake` in `stop_bus_tests` and
//! `service_tests`.
//!
//! The Event Log half of `Real` is covered by the shim integration tests: a unit test never writes
//! the real Event Log.

use std::fs::File;
use std::io::{BufRead as _, BufReader};
use std::os::windows::io::OwnedHandle;

use cosca::identity::{Liveness, Resolved};
use cosca::{Process, Stdio};

use super::*;

/// Holds a real daemon and ends its tree when dropped, so a failing test leaves no sleeper behind.
/// Made before the spawn it will hold.
#[derive(Default)]
struct Reap(Option<Box<dyn Spawned>>);

impl Reap {
    fn hold(&mut self, child: Box<dyn Spawned>) -> &dyn Spawned {
        &**self.0.insert(child)
    }
}

impl Drop for Reap {
    fn drop(&mut self) {
        if let Some(child) = &self.0 {
            let _ = child.kill_tree();
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

/// The supervisor asks for the status twice: the waiter's wait, then the one that reads the code.
#[skuld::test]
fn wait_gives_the_same_exit_code_every_time() {
    let mut reap = Reap::default();
    let daemon = reap.hold(Real.spawn(powershell("exit 3")).expect("spawn the daemon"));

    let first = daemon.wait().expect("the first wait");
    let second = daemon.wait().expect("the second wait");

    assert_eq!((first.code(), second.code()), (Some(3), Some(3)));
}

/// `cosca` reports it as `Io(NotFound)` with no OS error code, which `service::os_error_of` reads
/// as none.
#[skuld::test]
fn a_spawn_of_a_missing_binary_is_refused_as_not_found() {
    let refused = Real
        .spawn(cosca::run(["goetia-shim-test-no-such-binary"]))
        .err()
        .expect("the binary does not exist");

    assert!(
        matches!(&refused, cosca::error::Error::Io(e) if e.kind() == io::ErrorKind::NotFound),
        "{refused:?}"
    );
}

// Channels ------------------------------------------------------------------------------------------------------------

#[skuld::test]
fn append_creates_missing_directories_and_writes_the_line() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let path = dir.path().join("Goetia").join("logs").join("x.log");

    Real.append(&path, "the line").expect("append");

    assert_eq!(
        std::fs::read_to_string(&path).expect("the file was written"),
        "the line\n"
    );
}

#[skuld::test]
fn append_appends_rather_than_truncates() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let path = dir.path().join("x.log");

    Real.append(&path, "first").expect("append");
    Real.append(&path, "second").expect("append");

    assert_eq!(
        std::fs::read_to_string(&path).expect("the file was written"),
        "first\nsecond\n"
    );
}

#[skuld::test]
fn append_fails_and_writes_nothing_when_the_path_cannot_be_opened() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let file = dir.path().join("a-file");
    std::fs::write(&file, "x").expect("a regular file to use as a parent");

    // A directory cannot be created under a regular file.
    let result = Real.append(&file.join("x.log"), "lost");

    assert!(result.is_err());
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
