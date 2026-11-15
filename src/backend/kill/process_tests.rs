//! The identity read of a manager's report, over real children: a zombie is the held process, a
//! reaped one is gone, and a live process the manager reports under another pid's hold is another.

use std::process::{Child, Command, Stdio};

use super::*;

/// A child that runs until it is signalled, killed and reaped on drop so a failing assert leaves
/// nothing running.
pub(crate) struct Sleeper(pub(crate) Child);

impl Sleeper {
    pub(crate) fn spawn() -> Self {
        let child = Command::new("sleep")
            .arg("3600")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn sleep");
        Sleeper(child)
    }

    pub(crate) fn pid(&self) -> u32 {
        self.0.id()
    }

    /// The process held, as `kill` holds it.
    fn held(&self) -> Process {
        open(self.pid()).expect("open the child").expect("the child is running")
    }
}

impl Drop for Sleeper {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// A process that has exited and not been reaped has no process left to be reported, but the manager
/// has not recorded that: its pid is still the held process's.
#[skuld::test]
fn a_report_of_an_exited_process_not_yet_reaped_is_the_held_one_and_stale() {
    let mut run = Sleeper::spawn();
    let held = run.held();
    terminate(&held).expect("signal the child");
    await_exit(&held).expect("wait for it to exit");

    assert!(has_exited(&held).unwrap());
    assert!(report_is_stale(Some(&held), run.pid()).unwrap());
    run.0.wait().expect("reap the child");
}

/// A reaped process is gone, and a manager still showing its pid has not caught up: the wait goes on
/// until it does.
#[skuld::test]
fn a_report_of_a_reaped_process_is_stale() {
    let mut run = Sleeper::spawn();
    let held = run.held();
    terminate(&held).expect("signal the child");
    run.0.wait().expect("reap the child");

    assert!(report_is_stale(Some(&held), run.pid()).unwrap());
    assert!(report_is_stale(None, run.pid()).unwrap(), "nothing held, nothing there");
}

#[skuld::test]
fn a_report_of_a_running_held_process_is_stale() {
    let run = Sleeper::spawn();
    let held = run.held();

    assert!(!has_exited(&held).unwrap());
    assert!(report_is_stale(Some(&held), run.pid()).unwrap());
}

/// The pid that the manager reports names a live process other than the held one: the number was
/// given to a new process, and the held one's end is recorded.
#[skuld::test]
fn a_report_of_a_pid_another_process_has_is_not_stale() {
    let held_run = Sleeper::spawn();
    let held = held_run.held();
    let other = Sleeper::spawn();

    assert!(!report_is_stale(Some(&held), other.pid()).unwrap());
    assert!(
        !report_is_stale(None, other.pid()).unwrap(),
        "a process gone before it could be opened is no process that has the pid now"
    );
}

/// An OS that refuses to say what holds the pid leaves the question open, and the wait says so rather
/// than going on forever or deciding for one side.
#[skuld::test]
fn an_identity_the_os_will_not_read_is_an_error() {
    let e = stale(None, Resolved::Unknown, 7).expect_err("an unreadable identity");

    assert!(e.to_string().contains("process 7"), "{e}");
}
