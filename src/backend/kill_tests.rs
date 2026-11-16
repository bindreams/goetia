//! The state machine of [`super::kill_service`] against a scripted [`super::KillTarget`]: no manager
//! and no process involved, so these run on every platform.

use std::cell::RefCell;
use std::collections::VecDeque;
use std::rc::Rc;

use super::*;

type Log = Rc<RefCell<Vec<String>>>;

/// The kind of a failed `service_pid`: not `Other`, so a test sees whether a kind is kept.
const READ_FAILED_KIND: io::ErrorKind = io::ErrorKind::TimedOut;

/// The service every test kills.
const ID: &str = "frpc";

/// What opening a pid finds.
enum Open {
    Found,
    Gone,
    Fails,
}

/// What the manager answers each `service_pid`, and what each `open` does. The signal and the waits
/// are scripted by flags: the last answer of `pids` repeats.
struct Scripted {
    pids: VecDeque<Option<u32>>,
    opens: VecDeque<Open>,
    /// Which `service_pid` calls, counted from 1, fail instead of answering.
    failing_reads: Vec<usize>,
    reads: usize,
    failing_terminate: bool,
    failing_exit: bool,
    failing_recorded: bool,
    /// Whether a held process outlasts the wait before the notice: only an unbounded wait for it ends.
    running: bool,
    /// Whether `await_recorded` is cut short by its deadline.
    expiring_recorded: bool,
    /// Whether the manager's own lookup of the held process, in `confirm`, does not name it as the
    /// service's, whatever pid `service_pid` still shows.
    disowning: bool,
    log: Log,
}

impl Scripted {
    fn new<const P: usize, const O: usize>(pids: [Option<u32>; P], opens: [Open; O]) -> Self {
        Self {
            pids: pids.into(),
            opens: opens.into(),
            failing_reads: Vec::new(),
            reads: 0,
            failing_terminate: false,
            failing_exit: false,
            failing_recorded: false,
            running: false,
            expiring_recorded: false,
            disowning: false,
            log: Log::default(),
        }
    }

    /// The `n`th `service_pid` call (from 1) fails.
    fn failing_read(mut self, n: usize) -> Self {
        self.failing_reads.push(n);
        self
    }

    fn failing_terminate(mut self) -> Self {
        self.failing_terminate = true;
        self
    }

    fn failing_exit(mut self) -> Self {
        self.failing_exit = true;
        self
    }

    fn failing_recorded(mut self) -> Self {
        self.failing_recorded = true;
        self
    }

    /// The held process has not exited within the wait before the notice, so the wait for it blocks.
    fn running(mut self) -> Self {
        self.running = true;
        self
    }

    fn expiring_recorded(mut self) -> Self {
        self.expiring_recorded = true;
        self
    }

    /// The manager still shows the pid, and does not name the process held as the service's.
    fn disowning(mut self) -> Self {
        self.disowning = true;
        self
    }

    fn log(&self) -> Vec<String> {
        self.log.borrow().clone()
    }
}

/// A held process: logs when it is closed, so a test sees how long it was held.
#[derive(Debug)]
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
            return Err(io::Error::new(READ_FAILED_KIND, "manager read failed"));
        }
        Ok(if self.pids.len() > 1 {
            self.pids.pop_front().unwrap()
        } else {
            *self.pids.front().expect("a scripted pid")
        })
    }

    fn confirm(&mut self, pid: u32, _process: &Held) -> io::Result<bool> {
        Ok(self.service_pid()? == Some(pid) && !self.disowning)
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

    fn terminate(&mut self, process: &Held) -> io::Result<()> {
        self.log.borrow_mut().push(format!("terminate {}", process.pid));
        if self.failing_terminate {
            return Err(io::Error::other("terminate failed"));
        }
        Ok(())
    }

    fn notice(&mut self, line: &str) {
        self.log.borrow_mut().push(format!("notice {line}"));
    }

    fn await_exit(&mut self, process: &Held, deadline: Deadline) -> io::Result<Waited> {
        let bounded = deadline.remaining().is_some();
        self.log.borrow_mut().push(format!(
            "await_exit {}{}",
            process.pid,
            if bounded { " for the notice" } else { "" }
        ));
        if self.failing_exit {
            return Err(io::Error::other("wait on the process failed"));
        }
        Ok(if bounded && self.running {
            Waited::Expired
        } else {
            Waited::Confirmed
        })
    }

    fn await_recorded(&mut self, pid: u32, held: Option<&Held>, deadline: Deadline) -> io::Result<Waited> {
        self.log.borrow_mut().push(match held {
            Some(_) => format!("await_recorded {pid}"),
            None => format!("await_recorded {pid} unheld"),
        });
        if self.failing_recorded {
            return Err(io::Error::new(READ_FAILED_KIND, "manager read failed"));
        }
        if self.expiring_recorded {
            assert!(
                deadline.expired(),
                "Scripted: await_recorded expires only once its deadline has passed; an unbounded or unspent deadline never expires"
            );
            return Ok(Waited::Expired);
        }
        Ok(Waited::Confirmed)
    }
}

fn steps<const N: usize>(steps: [&str; N]) -> Vec<String> {
    steps.iter().map(|s| s.to_string()).collect()
}

// No process ----------------------------------------------------------------------------------------------------------

#[skuld::test]
fn a_service_with_no_process_has_nothing_to_kill() {
    let mut target = Scripted::new([None], []);

    assert_eq!(kill_service(&mut target, ID).unwrap(), KillOutcome::NotRunning);
    assert_eq!(
        target.log(),
        steps(["service_pid"]),
        "nothing was opened, nothing terminated"
    );
}

// The sequence --------------------------------------------------------------------------------------------------------

/// The order that makes the kill safe: the process is opened and held, the manager is asked again
/// whether it is still the service's, the signal is sent through the held process, and only then are
/// the waits made: for the process's exit, then for the manager to stop reporting it.
#[skuld::test]
fn the_process_is_held_from_the_open_through_the_waits_and_signalled_before_any_wait() {
    let mut target = Scripted::new([Some(100), Some(100)], [Open::Found]);

    assert_eq!(kill_service(&mut target, ID).unwrap(), KillOutcome::Ended);
    assert_eq!(
        target.log(),
        steps([
            "service_pid",
            "open 100",
            "service_pid",
            "terminate 100",
            "await_exit 100 for the notice",
            "await_recorded 100",
            "close 100",
        ])
    );
}

/// A process that outlasts the short wait is waited for with no bound, and the verb says why it
/// blocks, once, before that wait: a process that never exits would otherwise look like a hang.
#[skuld::test]
fn a_wait_for_a_process_that_has_not_exited_says_why_before_it_blocks() {
    let mut target = Scripted::new([Some(100), Some(100)], [Open::Found]).running();

    assert_eq!(kill_service(&mut target, ID).unwrap(), KillOutcome::Ended);
    assert_eq!(
        target.log(),
        steps([
            "service_pid",
            "open 100",
            "service_pid",
            "terminate 100",
            "await_exit 100 for the notice",
            "notice frpc: waiting for process 100 to exit",
            "await_exit 100",
            "await_recorded 100",
            "close 100",
        ])
    );
}

/// A process that exits within the short wait is not announced: the notice is for a wait that lasts.
#[skuld::test]
fn a_process_that_has_exited_by_the_time_of_the_wait_is_not_announced() {
    let mut target = Scripted::new([Some(100), Some(100)], [Open::Found]);

    assert_eq!(kill_service(&mut target, ID).unwrap(), KillOutcome::Ended);
    assert!(
        target.log().iter().all(|call| !call.starts_with("notice")),
        "{:?}",
        target.log()
    );
}

/// What the notice says: the service, the process, and what it waits for.
#[skuld::test]
fn the_waiting_notice_names_the_service_and_the_process() {
    assert_eq!(
        waiting_notice("frpc", 4242, None),
        "frpc: waiting for process 4242 to exit"
    );
}

#[skuld::test]
fn the_waiting_notice_names_a_failed_terminate() {
    assert_eq!(
        waiting_notice(
            "frpc",
            4242,
            Some(&io::Error::other("The handle is invalid. (os error 6)"))
        ),
        "frpc: waiting for process 4242 to exit, and terminating it failed: The handle is invalid. (os error 6)"
    );
}

/// The process was gone when the open came: it has ended, so the kill waits for the manager to stop
/// reporting it and returns. It does not ask what the service runs now.
#[skuld::test]
fn a_process_gone_before_it_was_opened_is_waited_for_in_the_manager_and_not_followed() {
    let mut target = Scripted::new([Some(100), Some(200)], [Open::Gone]);

    assert_eq!(kill_service(&mut target, ID).unwrap(), KillOutcome::Ended);
    assert_eq!(
        target.log(),
        steps([
            "service_pid",
            "open 100",
            "await_recorded 100 unheld",
            "await_recorded 100 unheld"
        ]),
        "the hold's own wait, then the kill's: the second is answered by the first read"
    );
}

/// The manager still shows the pid by the confirmation, but the process held is not the service's:
/// nothing is signalled and the held process is not waited for. The manager may still report the pid
/// for a moment, so the kill waits for its record before it returns.
#[skuld::test]
fn a_held_process_the_manager_does_not_name_is_not_signalled_and_the_record_is_waited_for() {
    let mut target = Scripted::new([Some(100), Some(100)], [Open::Found]).disowning();

    assert_eq!(kill_service(&mut target, ID).unwrap(), KillOutcome::Ended);
    assert_eq!(
        target.log(),
        steps([
            "service_pid",
            "open 100",
            "service_pid",
            "await_recorded 100",
            "close 100"
        ])
    );
}

/// The wait for the record of an ended process fails: that is the kill's error, plain, for nothing
/// was signalled.
#[skuld::test]
fn a_failed_record_wait_for_a_process_that_ended_is_not_called_a_signal() {
    let mut target = Scripted::new([Some(100), Some(100)], [Open::Found])
        .disowning()
        .failing_recorded();

    let e = kill_service(&mut target, ID).unwrap_err();

    assert_eq!(e.to_string(), "manager read failed");
    assert!(!target.log().iter().any(|call| call.starts_with("terminate")));
}

/// The manager reports another process by the confirmation: it has moved off the one held, which is
/// either gone or a stranger that took its pid, so it is not signalled. The hold is let go once the
/// manager's record of the pid has been read, the new process is not opened and nothing is signalled.
#[skuld::test]
fn another_pid_at_the_confirmation_ends_the_kill_without_a_signal() {
    let mut target = Scripted::new([Some(100), Some(200)], [Open::Found]);

    assert_eq!(kill_service(&mut target, ID).unwrap(), KillOutcome::Ended);
    assert_eq!(
        target.log(),
        steps([
            "service_pid",
            "open 100",
            "service_pid",
            "await_recorded 100",
            "close 100"
        ])
    );
}

/// The service stopped between the open and the confirmation: the same, with the manager reporting
/// none. This is not [`KillOutcome::NotRunning`], which only the first read says.
#[skuld::test]
fn no_pid_at_the_confirmation_ends_the_kill_and_is_not_a_service_with_no_process() {
    let mut target = Scripted::new([Some(100), None], [Open::Found]);

    assert_eq!(kill_service(&mut target, ID).unwrap(), KillOutcome::Ended);
    assert_eq!(
        target.log(),
        steps([
            "service_pid",
            "open 100",
            "service_pid",
            "await_recorded 100",
            "close 100"
        ])
    );
}

/// A service that restarts under the kill is not chased: one process is held, whatever follows.
#[skuld::test]
fn a_restart_during_the_kill_is_not_followed() {
    // `opens` holds one entry: a second `open` would panic.
    let mut target = Scripted::new([Some(100), Some(100), Some(200), Some(300)], [Open::Found]);

    assert_eq!(kill_service(&mut target, ID).unwrap(), KillOutcome::Ended);
    assert_eq!(
        target.log().iter().filter(|call| call.starts_with("terminate")).count(),
        1
    );
}

/// An open that failed for a pid the manager has since moved off is the pid having been somebody
/// else's or gone, not a refusal about the service.
#[skuld::test]
fn an_open_that_failed_on_a_pid_the_manager_moved_off_is_an_ended_process() {
    let mut target = Scripted::new([Some(100), Some(200)], [Open::Fails]);

    assert_eq!(kill_service(&mut target, ID).unwrap(), KillOutcome::Ended);
    assert_eq!(
        target.log(),
        steps(["service_pid", "open 100", "service_pid", "await_recorded 100 unheld"])
    );
}

#[skuld::test]
fn an_open_that_failed_on_a_service_that_stopped_is_an_ended_process() {
    let mut target = Scripted::new([Some(100), None], [Open::Fails]);

    assert_eq!(kill_service(&mut target, ID).unwrap(), KillOutcome::Ended);
}

/// The manager still vouches for the pid and it cannot be opened: that is a refusal, and it is reported.
#[skuld::test]
fn an_open_that_failed_on_the_services_own_pid_is_an_error() {
    let mut target = Scripted::new([Some(100)], [Open::Fails]);

    let e = kill_service(&mut target, ID).unwrap_err();

    assert_eq!(e.to_string(), "access denied");
    assert_eq!(target.log(), steps(["service_pid", "open 100", "service_pid"]));
}

// A failed read -------------------------------------------------------------------------------------------------------

/// The first read: nothing is opened, nothing terminated.
#[skuld::test]
fn a_failed_first_read_is_an_error_and_touches_nothing() {
    let mut target = Scripted::new([Some(100)], []).failing_read(1);

    let e = kill_service(&mut target, ID).unwrap_err();

    assert_eq!(e.to_string(), "manager read failed");
    assert_eq!(target.log(), steps(["service_pid"]));
}

/// The read that decides whether an open refusal is the service's own: both errors reach the caller.
#[skuld::test]
fn a_failed_read_after_a_refused_open_reports_both_errors() {
    let mut target = Scripted::new([Some(100)], [Open::Fails]).failing_read(2);

    let e = kill_service(&mut target, ID).unwrap_err();

    assert_eq!(
        e.to_string(),
        "manager read failed (after the service's process could not be opened: access denied)"
    );
    assert_eq!(e.kind(), READ_FAILED_KIND);
    let report = crate::error::Error::os("kill", e).report().to_string();
    assert_eq!(report.matches("manager read failed").count(), 1, "{report}");
    assert_eq!(report.matches("access denied").count(), 1, "{report}");
    assert_eq!(target.log(), steps(["service_pid", "open 100", "service_pid"]));
}

/// The confirming read, with the process held: it is let go and nothing is signalled.
#[skuld::test]
fn a_failed_confirming_read_closes_the_process_and_signals_nothing() {
    let mut target = Scripted::new([Some(100)], [Open::Found]).failing_read(2);

    let e = kill_service(&mut target, ID).unwrap_err();

    assert_eq!(e.to_string(), "manager read failed");
    assert_eq!(
        target.log(),
        steps(["service_pid", "open 100", "service_pid", "close 100"])
    );
}

/// What a failure after the signal reports: it was delivered, so the error must not read as a failed
/// kill. The cause and its kind are kept.
fn assert_signalled_but_unconfirmed(e: &io::Error, cause: &str, kind: io::ErrorKind) {
    let text = crate::error::Chain(e).to_string();
    assert!(text.contains("was signalled"), "{text}");
    assert!(
        text.contains("waiting for it to exit and for the manager to record that failed"),
        "{text}"
    );
    assert!(text.ends_with(cause), "{text}");
    assert!(!text.contains("kill failed"), "{text}");
    assert_eq!(e.kind(), kind);
}

/// The read after the signal: the kill cannot say the manager has let go, but the signal stands.
#[skuld::test]
fn a_failed_record_wait_after_the_signal_says_the_signal_was_delivered() {
    let mut target = Scripted::new([Some(100), Some(100)], [Open::Found]).failing_recorded();

    let e = kill_service(&mut target, ID).unwrap_err();

    assert_signalled_but_unconfirmed(&e, "manager read failed", READ_FAILED_KIND);
    assert_eq!(
        target.log(),
        steps([
            "service_pid",
            "open 100",
            "service_pid",
            "terminate 100",
            "await_exit 100 for the notice",
            "await_recorded 100",
            "close 100",
        ])
    );
}

#[skuld::test]
fn a_failed_wait_for_the_exit_says_the_signal_was_delivered_and_waits_for_no_record() {
    let mut target = Scripted::new([Some(100), Some(100)], [Open::Found]).failing_exit();

    let e = kill_service(&mut target, ID).unwrap_err();

    assert_signalled_but_unconfirmed(&e, "wait on the process failed", io::ErrorKind::Other);
    assert!(!target.log().contains(&"await_recorded 100".to_string()));
}

/// A process that was never signalled keeps the plain error: it is not a signalled one.
#[skuld::test]
fn a_failed_record_wait_for_a_process_found_gone_is_not_called_a_signal() {
    let mut target = Scripted::new([Some(100)], [Open::Gone]).failing_recorded();

    let e = kill_service(&mut target, ID).unwrap_err();

    assert_eq!(e.to_string(), "manager read failed");
}

#[skuld::test]
fn a_failed_signal_is_an_error_and_waits_for_nothing() {
    let mut target = Scripted::new([Some(100), Some(100)], [Open::Found]).failing_terminate();

    let e = kill_service(&mut target, ID).unwrap_err();

    let text = crate::error::Chain(&e).to_string();
    assert!(
        text.contains("process 100 was held and confirmed as the service's, but signalling it failed"),
        "{text}"
    );
    assert!(text.ends_with("terminate failed"), "{text}");
    assert_eq!(
        target.log(),
        steps(["service_pid", "open 100", "service_pid", "terminate 100", "close 100"]),
        "the process is let go on the error path too"
    );
}

// The hold, under a deadline ------------------------------------------------------------------------------------------

/// A stop under a budget holds the process first, and the hold waits for the manager to let go only
/// until the stop's deadline: a pid it keeps reporting past it is [`HoldError::Expired`], with nothing
/// held.
#[skuld::test]
fn a_record_the_manager_does_not_make_by_the_deadline_expires_the_hold() {
    let mut target = Scripted::new([Some(100)], [Open::Gone]).expiring_recorded();

    let error = hold_once(&mut target, Budget::Immediate.start()).expect_err("an expired hold");

    assert!(matches!(error, HoldError::Expired { pid: 100 }), "{error:?}");
    assert_eq!(
        target.log(),
        steps(["service_pid", "open 100", "await_recorded 100 unheld"])
    );
}

#[skuld::test]
fn holding_keeps_the_process_once_the_manager_confirms_its_pid() {
    let mut target = Scripted::new([Some(100), Some(100)], [Open::Found]);

    let Ok(Hold::Held { pid, process }) = hold_once(&mut target, Budget::Unbounded.start()) else {
        panic!("a held process");
    };

    assert_eq!(pid, 100);
    assert_eq!(target.log(), steps(["service_pid", "open 100", "service_pid"]));
    drop(process);
    assert_eq!(target.log().last().map(String::as_str), Some("close 100"));
}

// The error of a kill -------------------------------------------------------------------------------------------------

/// An error of goetia's own that a target passed through an `io::Error` is itself again, so a caller
/// matching on `NoManager` or `Command` still finds it.
#[skuld::test]
fn a_wrapped_error_comes_back_as_itself() {
    let e = io::Error::other(Error::NoManager {
        evidence: "x".to_string(),
    });

    assert!(matches!(kill_failed("x", e), Error::NoManager { .. }));
}

/// Any other failure is an OS failure of the kill, naming it; one after the signal keeps its context.
#[skuld::test]
fn any_other_error_is_an_os_failure_of_the_kill() {
    let after_the_signal = signalled_but(
        7,
        io::Error::other(Error::NoManager {
            evidence: "x".to_string(),
        }),
    );
    for e in [io::Error::other("access denied"), after_the_signal] {
        let failed = kill_failed("x", e);

        assert!(
            matches!(&failed, Error::Os { what, .. } if what == "kill `x`"),
            "{failed:?}"
        );
    }
}
