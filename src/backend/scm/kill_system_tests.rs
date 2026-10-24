//! The Win32 half of the SCM kill against real child processes the test spawns itself: no service,
//! no SCM and no elevation. These are the classifications that decide between "follow SCM" and
//! "error", which the scripted tests in `kill_tests.rs` take as given.

use std::os::windows::io::AsRawHandle;
use std::process::{Child, Command, Stdio};

use windows_sys::Win32::Foundation::FILETIME;
use windows_sys::Win32::System::Threading::GetProcessTimes;

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

/// When the process started, from its handle.
fn creation_time(process: &impl AsRawHandle) -> u64 {
    let zero = FILETIME {
        dwLowDateTime: 0,
        dwHighDateTime: 0,
    };
    let (mut created, mut exited, mut kernel, mut user) = (zero, zero, zero, zero);
    // SAFETY: a live handle and four valid out-pointers.
    let ok = unsafe {
        GetProcessTimes(
            process.as_raw_handle(),
            &mut created,
            &mut exited,
            &mut kernel,
            &mut user,
        )
    };
    assert_ne!(ok, 0, "GetProcessTimes: {}", std::io::Error::last_os_error());
    (u64::from(created.dwHighDateTime) << 32) | u64::from(created.dwLowDateTime)
}

/// Windows hands a freed pid to the next process created, and the other tests of this crate create
/// processes concurrently, so the pid may already name a stranger. What must never happen is the
/// exited process still being openable.
#[skuld::test]
fn opening_a_pid_whose_process_is_gone_finds_none() {
    let mut child = spawn_exiting_with(0);
    let pid = child.0.id();
    let started = creation_time(&child.0);
    child.0.wait().unwrap();
    // Every handle to the process is closed, so its object is gone and the pid names nothing.
    drop(child);

    if let Some(found) = open_process(pid).unwrap() {
        assert_ne!(creation_time(&found), started, "the exited process is still open");
    }
}

#[skuld::test]
fn terminating_a_running_process_delivers() {
    let mut child = spawn_long_running();
    let process = open_process(child.0.id()).unwrap().expect("the child is running");

    assert_eq!(terminate_process(&process).unwrap(), Termination::Terminated);
    assert_eq!(child.0.wait().unwrap().code(), Some(1));
}

/// A second terminate lands while the first is still taking effect: `TerminateProcess` is
/// asynchronous and refuses before the process handle is signalled.
#[skuld::test]
fn terminating_right_after_a_terminate_is_already_exited() {
    let child = spawn_long_running();
    let process = open_process(child.0.id()).unwrap().expect("the child is running");
    assert_eq!(terminate_process(&process).unwrap(), Termination::Terminated);

    assert_eq!(terminate_process(&process).unwrap(), Termination::AlreadyExited);
}

/// 259 is `STILL_ACTIVE`, the code a check of the exit code would take for a live process.
#[skuld::test]
fn terminating_a_process_that_exited_with_259_is_already_exited() {
    let mut child = spawn_exiting_with(259);
    let process = open_process(child.0.id())
        .unwrap()
        .expect("the child's object is held by its handle");
    assert_eq!(child.0.wait().unwrap().code(), Some(259));

    assert_eq!(terminate_process(&process).unwrap(), Termination::AlreadyExited);
}
