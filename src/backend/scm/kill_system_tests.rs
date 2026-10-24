//! The Win32 half of the SCM kill against real child processes the test spawns itself: no service,
//! no SCM and no elevation. These are the classifications that decide between "follow SCM" and
//! "error", which the scripted tests in `kill_tests.rs` take as given.

use std::cell::Cell;
use std::process::{Child, Command, Stdio};

use super::Termination;
use super::system::{open_process, terminate_process};

/// A child that is killed and reaped on drop, so a failing assert leaves nothing running.
struct Spawned(Child);

impl Drop for Spawned {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn spawn(program: &str, args: &[&str]) -> Spawned {
    let child = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap_or_else(|e| panic!("spawn {program}: {e}"));
    Spawned(child)
}

/// A child that runs until it is terminated.
fn spawn_long_running() -> Spawned {
    spawn("ping", &["-n", "3600", "127.0.0.1"])
}

/// A child that exits by itself with `code`.
fn spawn_exiting_with(code: u32) -> Spawned {
    spawn("cmd", &["/c", &format!("exit {code}")])
}

// `ERROR_INVALID_PARAMETER` -> gone is covered by `kill_tests.rs` only: when the OS frees a real process object is not the test's to control.

/// The kill loop meets a process that exited but is still held: the held object keeps its pid
/// openable. `child` keeps its own handle open until it is dropped.
#[skuld::test]
fn opening_a_pid_whose_exited_process_is_held_finds_it() {
    let mut child = spawn_exiting_with(0);
    child.0.wait().unwrap();

    let found = open_process(child.0.id()).unwrap();

    assert!(found.is_some(), "the exited process is held by the child's handle");
}

/// What the loop does with that handle: terminating it finds nothing left to do.
#[skuld::test]
fn terminating_a_held_exited_process_is_already_exited() {
    let mut child = spawn_exiting_with(0);
    child.0.wait().unwrap();
    let process = open_process(child.0.id())
        .unwrap()
        .expect("the exited process is held by the child's handle");

    let result = terminate_process(&process, || unreachable!("an exited process is not waited for"));

    assert_eq!(result.unwrap(), Termination::AlreadyExited);
}

#[skuld::test]
fn terminating_a_running_process_delivers() {
    let mut child = spawn_long_running();
    let process = open_process(child.0.id()).unwrap().expect("the child is running");

    let result = terminate_process(&process, || unreachable!("a live process is not waited for"));

    assert_eq!(result.unwrap(), Termination::Terminated);
    assert_eq!(child.0.wait().unwrap().code(), Some(1));
}

/// A second terminate lands while the first is still taking effect: `TerminateProcess` is
/// asynchronous and refuses before the process handle is signalled. Whether the handle is signalled
/// by the time the second call looks is the OS's to say, so the notice may or may not be given, but
/// never more than once.
#[skuld::test]
fn terminating_right_after_a_terminate_is_already_exited() {
    let child = spawn_long_running();
    let process = open_process(child.0.id()).unwrap().expect("the child is running");
    assert_eq!(
        terminate_process(&process, || unreachable!("a live process is not waited for")).unwrap(),
        Termination::Terminated
    );

    let notices = Cell::new(0);
    let result = terminate_process(&process, || notices.set(notices.get() + 1));

    assert_eq!(result.unwrap(), Termination::AlreadyExited);
    assert!(notices.get() <= 1, "noticed {} times", notices.get());
}

/// 259 is `STILL_ACTIVE`, the code a check of the exit code would take for a live process. The
/// process is already gone, so there is nothing to wait for and nothing to announce.
#[skuld::test]
fn terminating_a_process_that_exited_with_259_is_already_exited() {
    let mut child = spawn_exiting_with(259);
    let process = open_process(child.0.id())
        .unwrap()
        .expect("the child's object is held by its handle");
    assert_eq!(child.0.wait().unwrap().code(), Some(259));

    let result = terminate_process(&process, || unreachable!("an exited process is not waited for"));

    assert_eq!(result.unwrap(), Termination::AlreadyExited);
}
