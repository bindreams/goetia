//! Orchestration tests for [`super::kill_service`] against a scripted [`super::KillTarget`]: no
//! SCM and no process involved, so these run on every platform. Every change of the service's
//! state between two of its steps is scripted, never raced for.

use std::collections::VecDeque;

use super::*;

/// What SCM answers the next `service_pid`, and what opening a pid finds.
struct Scripted {
    /// Front to back; the last answer repeats.
    pids: VecDeque<Option<u32>>,
    /// Per `open`, front to back: the process found, or `None` for gone.
    opens: VecDeque<Option<u32>>,
    log: Vec<String>,
    terminate_fails: bool,
}

impl Scripted {
    fn new<const P: usize, const O: usize>(pids: [Option<u32>; P], opens: [Option<u32>; O]) -> Self {
        Self {
            pids: pids.into(),
            opens: opens.into(),
            log: vec![],
            terminate_fails: false,
        }
    }
}

impl KillTarget for Scripted {
    /// The pid the process was opened under: what a handle to it would say.
    type Process = u32;

    fn service_pid(&mut self) -> io::Result<Option<u32>> {
        self.log.push("service_pid".to_string());
        Ok(if self.pids.len() > 1 {
            self.pids.pop_front().unwrap()
        } else {
            *self.pids.front().expect("a scripted pid")
        })
    }

    fn open(&mut self, pid: u32) -> io::Result<Option<u32>> {
        self.log.push(format!("open {pid}"));
        Ok(self.opens.pop_front().expect("a scripted open"))
    }

    fn terminate(&mut self, process: u32) -> io::Result<()> {
        self.log.push(format!("terminate {process}"));
        if self.terminate_fails {
            return Err(io::Error::other("access denied"));
        }
        Ok(())
    }
}

#[skuld::test]
fn a_service_with_no_process_has_nothing_to_kill() {
    let mut target = Scripted::new([None], []);

    assert_eq!(kill_service(&mut target).unwrap(), Killed::NotRunning);
    assert_eq!(target.log, ["service_pid"], "nothing was opened, nothing terminated");
}

/// The order that makes the kill safe: the process is opened first, then SCM is asked again whether
/// it is still the service's, and only then is it terminated.
#[skuld::test]
fn the_process_is_opened_then_confirmed_through_scm_then_terminated() {
    let mut target = Scripted::new([Some(100)], [Some(100)]);

    assert_eq!(kill_service(&mut target).unwrap(), Killed::Terminated);
    assert_eq!(target.log, ["service_pid", "open 100", "service_pid", "terminate 100"]);
}

/// The service restarted between the read and the confirmation: the pid read first may belong to
/// anyone now, so nothing is terminated through it, and the service's new process is the one killed.
#[skuld::test]
fn a_restart_between_the_read_and_the_confirmation_terminates_nothing_stale() {
    let mut target = Scripted::new([Some(100), Some(200), Some(200)], [Some(100), Some(200)]);

    assert_eq!(kill_service(&mut target).unwrap(), Killed::Terminated);
    assert_eq!(
        target.log,
        [
            "service_pid",
            "open 100",
            "service_pid",
            "service_pid",
            "open 200",
            "service_pid",
            "terminate 200"
        ]
    );
}

/// The process exited between the read and the open, so there is nothing to open: SCM is asked
/// again, and reports the service stopped.
#[skuld::test]
fn a_process_gone_before_it_was_opened_sends_the_question_back_to_scm() {
    let mut target = Scripted::new([Some(100), None], [None]);

    assert_eq!(kill_service(&mut target).unwrap(), Killed::NotRunning);
    assert_eq!(target.log, ["service_pid", "open 100", "service_pid"]);
}

/// The service stopped between the open and the confirmation: the process opened is no longer one
/// to terminate.
#[skuld::test]
fn a_service_stopped_after_the_open_is_not_terminated_through_the_handle() {
    let mut target = Scripted::new([Some(100), None], [Some(100)]);

    assert_eq!(kill_service(&mut target).unwrap(), Killed::NotRunning);
    assert!(
        !target.log.iter().any(|step| step.starts_with("terminate")),
        "{:?}",
        target.log
    );
}

#[skuld::test]
fn a_failed_termination_is_an_error_not_a_kill() {
    let mut target = Scripted::new([Some(100)], [Some(100)]);
    target.terminate_fails = true;

    let e = kill_service(&mut target).unwrap_err();

    assert_eq!(e.to_string(), "access denied");
}
