//! The identity read of a manager's report, over real children: a zombie is the held process, a
//! reaped one is gone, and a live process the manager reports under another pid's hold is another.

use std::process::{Child, Command, Stdio};

use super::*;
use crate::manager::budget::Budget;

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

/// The wait for a held process ends at the deadline it is given, with the process running, and never
/// before it has exited: the process is neither reaped nor recorded by it.
#[skuld::test]
fn a_wait_for_a_running_process_expires_at_its_deadline_and_confirms_once_it_has_exited() {
    let run = Sleeper::spawn();
    let held = run.held();

    assert_eq!(await_exit(&held, Budget::Immediate.start()).unwrap(), Waited::Expired);
    assert_eq!(
        await_exit(&held, Budget::Bounded(std::time::Duration::from_millis(50)).start()).unwrap(),
        Waited::Expired
    );
    terminate(&held).expect("signal the child");
    assert_eq!(await_exit(&held, Budget::Unbounded.start()).unwrap(), Waited::Confirmed);
    assert_eq!(await_exit(&held, Budget::Immediate.start()).unwrap(), Waited::Confirmed);
}

/// A process that has exited and not been reaped has no process left to be reported, but the manager
/// has not recorded that: its pid is still the held process's.
#[skuld::test]
fn a_report_of_an_exited_process_not_yet_reaped_is_the_held_one_and_stale() {
    let mut run = Sleeper::spawn();
    let held = run.held();
    terminate(&held).expect("signal the child");
    assert_eq!(await_exit(&held, Budget::Unbounded.start()).unwrap(), Waited::Confirmed);

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

// The recorded rule ---------------------------------------------------------------------------------------------------

/// The manager has recorded the end of `pid`'s process when it reports another pid or none, or `pid`
/// on a process that is not the one held; it has not while it reports the held process's own.
#[skuld::test]
fn the_end_is_recorded_when_the_manager_reports_another_pid_none_or_another_process() {
    let run = Sleeper::spawn();
    let held = run.held();
    let other = Sleeper::spawn();

    assert!(!recorded(Some(run.pid()), Some(&held), run.pid()).unwrap());
    assert!(recorded(None, Some(&held), run.pid()).unwrap());
    assert!(recorded(Some(run.pid() + 1), Some(&held), run.pid()).unwrap());
    assert!(recorded(Some(other.pid()), Some(&held), other.pid()).unwrap());
}

// The confirmation ----------------------------------------------------------------------------------------------------

/// A manager that shows `pid`, and answers `names` to its own lookup of it; either read panics when
/// the test says it is not made.
struct Shows {
    pid: Option<u32>,
    names: Option<bool>,
}

impl Reports for Shows {
    fn service_pid(&mut self) -> io::Result<Option<u32>> {
        Ok(self.pid)
    }

    fn names(&mut self, _pid: u32) -> io::Result<bool> {
        Ok(self.names.expect("the lookup was not to be made"))
    }

    fn await_recorded(&mut self, _pid: u32, _held: Option<&Process>, _deadline: Deadline) -> io::Result<Waited> {
        unreachable!("the confirmation waits for nothing")
    }
}

#[skuld::test]
fn a_pid_the_manager_shows_and_names_is_confirmed() {
    let run = Sleeper::spawn();
    let held = run.held();
    let mut target = Signalling(Shows {
        pid: Some(run.pid()),
        names: Some(true),
    });

    assert!(target.confirm(run.pid(), &held).unwrap());
}

#[skuld::test]
fn another_pid_is_not_confirmed_and_the_lookup_is_not_made() {
    let run = Sleeper::spawn();
    let held = run.held();
    let mut target = Signalling(Shows {
        pid: Some(run.pid() + 1),
        names: None,
    });

    assert!(!target.confirm(run.pid(), &held).unwrap());
}

/// The manager shows the pid, the lookup does not name the job, and the process held is running: it
/// is a live process the kill cannot tie to the job, so it is an error and nothing is claimed ended.
#[skuld::test]
fn a_running_process_the_manager_shows_and_does_not_name_is_an_error() {
    let run = Sleeper::spawn();
    let held = run.held();
    let mut target = Signalling(Shows {
        pid: Some(run.pid()),
        names: Some(false),
    });

    let e = target.confirm(run.pid(), &held).expect_err("a process not confirmed");

    let text = e.to_string();
    assert!(text.contains(&format!("process {}", run.pid())), "{text}");
    assert!(text.contains("could not be confirmed as the service's"), "{text}");
}

/// The process held has exited and the manager still shows its pid: not a live process to be wrong
/// about, so the confirmation is "no", and the record is what the kill waits for.
#[skuld::test]
fn an_exited_process_the_manager_shows_and_does_not_name_is_not_confirmed() {
    let mut run = Sleeper::spawn();
    let held = run.held();
    terminate(&held).expect("signal the child");
    assert_eq!(await_exit(&held, Budget::Unbounded.start()).unwrap(), Waited::Confirmed);
    let mut target = Signalling(Shows {
        pid: Some(run.pid()),
        names: Some(false),
    });

    assert!(!target.confirm(run.pid(), &held).unwrap());
    run.0.wait().expect("reap the child");
}
