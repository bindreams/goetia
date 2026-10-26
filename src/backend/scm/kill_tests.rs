//! Orchestration tests for [`super::kill_service`] against a scripted [`super::KillTarget`]: no
//! SCM and no process involved, so these run on every platform.

use std::cell::RefCell;
use std::collections::VecDeque;
use std::rc::Rc;

use super::*;

type Log = Rc<RefCell<Vec<String>>>;

/// What opening a pid finds.
enum Open {
    Found,
    Gone,
    Fails,
}

/// What terminating an opened process does.
enum Terminate {
    Done,
    AlreadyExited,
    Fails,
}

/// What SCM answers each `service_pid`, and what each `open` and `terminate` do.
struct Scripted {
    /// Front to back; the last answer repeats.
    pids: VecDeque<Option<u32>>,
    opens: VecDeque<Open>,
    terminates: VecDeque<Terminate>,
    /// Which `service_pid` calls, counted from 1, fail instead of answering.
    failing_reads: Vec<usize>,
    reads: usize,
    /// How many times the kill has blocked for SCM to let go.
    releases: usize,
    log: Log,
}

impl Scripted {
    fn new<const P: usize, const O: usize, const T: usize>(
        pids: [Option<u32>; P],
        opens: [Open; O],
        terminates: [Terminate; T],
    ) -> Self {
        Self {
            pids: pids.into(),
            opens: opens.into(),
            terminates: terminates.into(),
            failing_reads: Vec::new(),
            reads: 0,
            releases: 0,
            log: Log::default(),
        }
    }

    /// The `n`th `service_pid` call (from 1) fails.
    fn failing_read(mut self, n: usize) -> Self {
        self.failing_reads.push(n);
        self
    }

    fn log(&self) -> Vec<String> {
        self.log.borrow().clone()
    }
}

/// A held process: logs when it is closed, so a test sees how long the handle was held.
struct Held {
    pid: u32,
    log: Log,
}

impl Drop for Held {
    fn drop(&mut self) {
        self.log.borrow_mut().push(format!("close {}", self.pid));
    }
}

impl KillTarget for Scripted {
    type Process = Held;

    fn service_pid(&mut self) -> io::Result<Option<u32>> {
        self.log.borrow_mut().push("service_pid".to_string());
        self.reads += 1;
        if self.failing_reads.contains(&self.reads) {
            return Err(io::Error::other("scm read failed"));
        }
        Ok(if self.pids.len() > 1 {
            self.pids.pop_front().unwrap()
        } else {
            *self.pids.front().expect("a scripted pid")
        })
    }

    fn open(&mut self, pid: u32) -> io::Result<Option<Held>> {
        self.log.borrow_mut().push(format!("open {pid}"));
        match self.opens.pop_front().expect("a scripted open") {
            Open::Found => Ok(Some(Held {
                pid,
                log: self.log.clone(),
            })),
            Open::Gone => Ok(None),
            Open::Fails => Err(io::Error::other("access denied")),
        }
    }

    fn terminate(&mut self, process: &Held) -> io::Result<Termination> {
        self.log.borrow_mut().push(format!("terminate {}", process.pid));
        match self.terminates.pop_front().expect("a scripted terminate") {
            Terminate::Done => Ok(Termination::Terminated),
            Terminate::AlreadyExited => Ok(Termination::AlreadyExited),
            Terminate::Fails => Err(io::Error::other("terminate failed")),
        }
    }

    fn await_exit(&mut self, process: &Held) -> io::Result<()> {
        self.log.borrow_mut().push(format!("await_exit {}", process.pid));
        Ok(())
    }

    fn arm_release(&mut self) -> io::Result<()> {
        self.log.borrow_mut().push("arm_release".to_string());
        Ok(())
    }

    fn await_release(&mut self) -> io::Result<()> {
        self.log.borrow_mut().push("await_release".to_string());
        self.releases += 1;
        assert!(
            self.releases <= 5,
            "the script never released the pid; the wait would not end"
        );
        Ok(())
    }
}

fn steps<const N: usize>(steps: [&str; N]) -> Vec<String> {
    steps.iter().map(|s| s.to_string()).collect()
}

#[skuld::test]
fn a_service_with_no_process_has_nothing_to_kill() {
    let mut target = Scripted::new([None], [], []);

    assert_eq!(kill_service(&mut target).unwrap(), Killed::NotRunning);
    assert_eq!(
        target.log(),
        steps(["service_pid"]),
        "nothing was opened, nothing terminated"
    );
}

/// The order that makes the kill safe: the process is opened and held, SCM is asked again whether it
/// is still the service's, and only then is it terminated, through the handle still held.
#[skuld::test]
fn the_process_is_held_from_the_open_through_the_termination() {
    let mut target = Scripted::new([Some(100), Some(100), None], [Open::Found], [Terminate::Done]);

    assert_eq!(kill_service(&mut target).unwrap(), Killed::Terminated);
    assert_eq!(
        target.log(),
        steps([
            "service_pid",
            "open 100",
            "service_pid",
            "terminate 100",
            "await_exit 100",
            "arm_release",
            "service_pid",
            "close 100",
        ])
    );
}

/// The service restarted between the read and the confirmation: the process opened is not the
/// service's, so it is let go before SCM is asked again, and the new process is the one killed.
#[skuld::test]
fn a_restart_between_the_open_and_the_confirmation_lets_go_of_the_stale_process() {
    let mut target = Scripted::new(
        [Some(100), Some(200), Some(200), Some(200), None],
        [Open::Found, Open::Found],
        [Terminate::Done],
    );

    assert_eq!(kill_service(&mut target).unwrap(), Killed::Terminated);
    assert_eq!(
        target.log(),
        steps([
            "service_pid",
            "open 100",
            "service_pid",
            "close 100",
            "service_pid",
            "open 200",
            "service_pid",
            "terminate 200",
            "await_exit 200",
            "arm_release",
            "service_pid",
            "close 200",
        ])
    );
}

/// The process exited between the read and the open, so there is nothing to open: SCM is asked
/// again, and reports the service stopped.
#[skuld::test]
fn a_process_gone_before_it_was_opened_sends_the_question_back_to_scm() {
    let mut target = Scripted::new([Some(100), None], [Open::Gone], []);

    assert_eq!(kill_service(&mut target).unwrap(), Killed::NotRunning);
    assert_eq!(target.log(), steps(["service_pid", "open 100", "service_pid"]));
}

/// The same, with the service restarted: the process that vanished is not the answer, the service's
/// new one is.
#[skuld::test]
fn a_process_gone_before_it_was_opened_is_followed_to_the_services_new_process() {
    let mut target = Scripted::new(
        [Some(100), Some(200), Some(200), None],
        [Open::Gone, Open::Found],
        [Terminate::Done],
    );

    assert_eq!(kill_service(&mut target).unwrap(), Killed::Terminated);
    assert_eq!(
        target.log(),
        steps([
            "service_pid",
            "open 100",
            "service_pid",
            "open 200",
            "service_pid",
            "terminate 200",
            "await_exit 200",
            "arm_release",
            "service_pid",
            "close 200",
        ])
    );
}

/// The service stopped between the open and the confirmation: the process held is no longer one to
/// terminate.
#[skuld::test]
fn a_service_stopped_after_the_open_is_not_terminated_through_the_handle() {
    let mut target = Scripted::new([Some(100), None], [Open::Found], []);

    assert_eq!(kill_service(&mut target).unwrap(), Killed::NotRunning);
    assert_eq!(
        target.log(),
        steps(["service_pid", "open 100", "service_pid", "close 100", "service_pid"])
    );
}

/// The process exited between the confirmation and the termination, on its own: nothing was
/// terminated, so the kill is not reported delivered. SCM is asked again, and has stopped the service.
#[skuld::test]
fn a_process_that_exited_before_the_termination_is_not_a_kill() {
    let mut target = Scripted::new([Some(100), Some(100), None], [Open::Found], [Terminate::AlreadyExited]);

    assert_eq!(kill_service(&mut target).unwrap(), Killed::NotRunning);
    assert_eq!(
        target.log(),
        steps([
            "service_pid",
            "open 100",
            "service_pid",
            "terminate 100",
            "close 100",
            "service_pid",
        ])
    );
}

/// The same, with the service restarted by the time SCM is asked again: the new process is killed.
#[skuld::test]
fn a_process_that_exited_before_the_termination_is_followed_to_the_services_new_process() {
    let mut target = Scripted::new(
        [Some(100), Some(100), Some(200), Some(200), None],
        [Open::Found, Open::Found],
        [Terminate::AlreadyExited, Terminate::Done],
    );

    assert_eq!(kill_service(&mut target).unwrap(), Killed::Terminated);
    assert_eq!(
        target.log(),
        steps([
            "service_pid",
            "open 100",
            "service_pid",
            "terminate 100",
            "close 100",
            "service_pid",
            "open 200",
            "service_pid",
            "terminate 200",
            "await_exit 200",
            "arm_release",
            "service_pid",
            "close 200",
        ])
    );
}

/// An open that failed for a pid SCM has since moved off is the pid having been somebody else's or
/// gone, not a refusal about the service: SCM is asked again.
#[skuld::test]
fn an_open_that_failed_on_a_pid_scm_moved_off_follows_the_service() {
    let mut target = Scripted::new(
        [Some(100), Some(200), Some(200), Some(200), None],
        [Open::Fails, Open::Found],
        [Terminate::Done],
    );

    assert_eq!(kill_service(&mut target).unwrap(), Killed::Terminated);
    assert_eq!(
        target.log(),
        steps([
            "service_pid",
            "open 100",
            "service_pid",
            "service_pid",
            "open 200",
            "service_pid",
            "terminate 200",
            "await_exit 200",
            "arm_release",
            "service_pid",
            "close 200",
        ])
    );
}

#[skuld::test]
fn an_open_that_failed_on_a_service_that_stopped_is_not_running() {
    let mut target = Scripted::new([Some(100), None], [Open::Fails], []);

    assert_eq!(kill_service(&mut target).unwrap(), Killed::NotRunning);
}

/// SCM still vouches for the pid and it cannot be opened: that is a refusal, and it is reported.
#[skuld::test]
fn an_open_that_failed_on_the_services_own_pid_is_an_error() {
    let mut target = Scripted::new([Some(100)], [Open::Fails], []);

    let e = kill_service(&mut target).unwrap_err();

    assert_eq!(e.to_string(), "access denied");
    assert_eq!(target.log(), steps(["service_pid", "open 100", "service_pid"]));
}

/// SCM records the exit after the termination is delivered, so a read straight after it still shows
/// the old pid and the old state (CI run 40262088279: `uninstall` straight after `kill` saw RUNNING).
/// The kill is not done until SCM has let go: it blocks for the release and reads again.
#[skuld::test]
fn a_kill_waits_while_scm_still_reports_the_killed_pid() {
    let mut target = Scripted::new(
        [Some(100), Some(100), Some(100), Some(100), None],
        [Open::Found],
        [Terminate::Done],
    );

    assert_eq!(kill_service(&mut target).unwrap(), Killed::Terminated);
    assert_eq!(
        target.log(),
        steps([
            "service_pid",
            "open 100",
            "service_pid",
            "terminate 100",
            "await_exit 100",
            "arm_release",
            "service_pid",
            "await_release",
            "arm_release",
            "service_pid",
            "await_release",
            "arm_release",
            "service_pid",
            "close 100",
        ]),
        "the read comes after each arm, so a change between a read and an arm is not missed"
    );
}

/// A recovery action restarted the service: SCM has moved off the killed pid, so there is nothing
/// to wait for, and the new process is not the kill's to touch.
#[skuld::test]
fn a_kill_is_done_when_scm_reports_a_new_pid() {
    let mut target = Scripted::new([Some(100), Some(100), Some(200)], [Open::Found], [Terminate::Done]);

    assert_eq!(kill_service(&mut target).unwrap(), Killed::Terminated);
    assert_eq!(
        target.log(),
        steps([
            "service_pid",
            "open 100",
            "service_pid",
            "terminate 100",
            "await_exit 100",
            "arm_release",
            "service_pid",
            "close 100",
        ])
    );
}

// A failed read of SCM ------------------------------------------------------------------------------------------------

/// The first read: nothing is opened, nothing terminated.
#[skuld::test]
fn a_failed_first_read_is_an_error_and_touches_nothing() {
    let mut target = Scripted::new([Some(100)], [], []).failing_read(1);

    let e = kill_service(&mut target).unwrap_err();

    assert_eq!(e.to_string(), "scm read failed");
    assert_eq!(target.log(), steps(["service_pid"]));
}

/// The read that decides whether an open refusal is the service's own: both errors reach the caller.
#[skuld::test]
fn a_failed_read_after_a_refused_open_reports_both_errors() {
    let mut target = Scripted::new([Some(100)], [Open::Fails], []).failing_read(2);

    let e = kill_service(&mut target).unwrap_err();

    let msg = e.to_string();
    assert!(msg.contains("scm read failed"), "{msg}");
    assert!(msg.contains("access denied"), "{msg}");
    assert_eq!(target.log(), steps(["service_pid", "open 100", "service_pid"]));
}

/// The confirming read, with the handle held: the handle is closed and nothing is terminated.
#[skuld::test]
fn a_failed_confirming_read_closes_the_handle_and_terminates_nothing() {
    let mut target = Scripted::new([Some(100)], [Open::Found], []).failing_read(2);

    let e = kill_service(&mut target).unwrap_err();

    assert_eq!(e.to_string(), "scm read failed");
    assert_eq!(
        target.log(),
        steps(["service_pid", "open 100", "service_pid", "close 100"])
    );
}

/// The read after the termination: the kill cannot say SCM has let go, so it is an error.
#[skuld::test]
fn a_failed_read_after_the_termination_is_an_error() {
    let mut target = Scripted::new([Some(100)], [Open::Found], [Terminate::Done]).failing_read(3);

    let e = kill_service(&mut target).unwrap_err();

    assert_eq!(e.to_string(), "scm read failed");
    assert_eq!(
        target.log(),
        steps([
            "service_pid",
            "open 100",
            "service_pid",
            "terminate 100",
            "await_exit 100",
            "arm_release",
            "service_pid",
            "close 100",
        ])
    );
}

#[skuld::test]
fn a_failed_termination_is_an_error_not_a_kill() {
    let mut target = Scripted::new([Some(100)], [Open::Found], [Terminate::Fails]);

    let e = kill_service(&mut target).unwrap_err();

    assert_eq!(e.to_string(), "terminate failed");
    assert_eq!(
        target.log(),
        steps(["service_pid", "open 100", "service_pid", "terminate 100", "close 100"]),
        "the handle is closed on the error path too"
    );
}

// The raw status ------------------------------------------------------------------------------------------------------

/// `windows-service`'s `query_status` reports a pid only in `Running`; SCM reports one in every
/// state that has a process, and a service stuck starting or stopping is what `kill` is for.
#[cfg(windows)]
#[skuld::test]
fn every_state_with_a_process_yields_its_pid() {
    use windows_sys::Win32::System::Services::{
        SERVICE_CONTINUE_PENDING, SERVICE_PAUSE_PENDING, SERVICE_PAUSED, SERVICE_RUNNING, SERVICE_START_PENDING,
        SERVICE_STATUS_PROCESS, SERVICE_STOP_PENDING, SERVICE_STOPPED,
    };

    let status = |state: u32, pid: u32| SERVICE_STATUS_PROCESS {
        dwCurrentState: state,
        dwProcessId: pid,
        // SAFETY: plain integers; all-zero is a valid value of every field.
        ..unsafe { std::mem::zeroed() }
    };

    for state in [
        SERVICE_RUNNING,
        SERVICE_START_PENDING,
        SERVICE_STOP_PENDING,
        SERVICE_CONTINUE_PENDING,
        SERVICE_PAUSE_PENDING,
        SERVICE_PAUSED,
    ] {
        assert_eq!(system::process_of(&status(state, 4242)), Some(4242), "state {state}");
    }
    assert_eq!(system::process_of(&status(SERVICE_STOPPED, 0)), None);
}
