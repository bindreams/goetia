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
    /// Fails with an OS error of this code.
    FailsWithOsError(i32),
    /// Fails with this goetia error, passed through an `io::Error` as a manager's target does.
    FailsCarrying(Error),
}

/// What the manager answers each `service_pid`, and what each `open` does. The signal and the waits
/// are scripted by flags: the last answer of `pids` repeats.
struct Scripted {
    pids: VecDeque<Option<u32>>,
    opens: VecDeque<Open>,
    /// Which `service_pid` calls, counted from 1, fail instead of answering.
    failing_reads: Vec<usize>,
    /// The OS error code the failed reads carry, if they carry one.
    read_os_error: Option<i32>,
    /// The goetia error the first failed read carries, passed through an `io::Error` as a manager's
    /// target does.
    read_carrying: Option<Error>,
    reads: usize,
    failing_terminate: bool,
    failing_exit: bool,
    failing_recorded: bool,
    /// Whether a held process outlasts the wait before the notice: only an unbounded wait for it ends.
    running: bool,
    /// Whether `await_recorded` is cut short by its deadline.
    expiring_recorded: bool,
    /// Whether the manager has not recorded the exit within the wait before the notice, so the wait
    /// for the record blocks: only an unbounded wait for it ends.
    lagging_record: bool,
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
            read_os_error: None,
            read_carrying: None,
            reads: 0,
            failing_terminate: false,
            failing_exit: false,
            failing_recorded: false,
            running: false,
            expiring_recorded: false,
            lagging_record: false,
            disowning: false,
            log: Log::default(),
        }
    }

    /// The `n`th `service_pid` call (from 1) fails.
    fn failing_read(mut self, n: usize) -> Self {
        self.failing_reads.push(n);
        self
    }

    /// The failed reads carry an OS error of this code.
    fn reading_fails_with_os_error(mut self, code: i32) -> Self {
        self.read_os_error = Some(code);
        self
    }

    /// The failed read carries this goetia error.
    fn reading_fails_carrying(mut self, error: Error) -> Self {
        self.read_carrying = Some(error);
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

    fn lagging_record(mut self) -> Self {
        self.lagging_record = true;
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
            if let Some(error) = self.read_carrying.take() {
                return Err(io::Error::other(error));
            }
            return Err(match self.read_os_error {
                Some(code) => io::Error::from_raw_os_error(code),
                None => io::Error::new(READ_FAILED_KIND, "manager read failed"),
            });
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
            Open::FailsWithOsError(code) => Err(io::Error::from_raw_os_error(code)),
            Open::FailsCarrying(e) => Err(io::Error::other(e)),
        }
    }

    fn force_end(&mut self, process: &Held) -> io::Result<()> {
        self.log.borrow_mut().push(format!("force_end {}", process.pid));
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
        if self.lagging_record && deadline.remaining().is_some() {
            return Ok(Waited::Expired);
        }
        if self.failing_recorded {
            return Err(io::Error::new(READ_FAILED_KIND, "manager read failed"));
        }
        // The record, and with it every scripted state up to the last, which the manager now shows: a
        // restart that happened under the kill is a state the machine could read, and does not.
        while self.pids.len() > 1 {
            self.pids.pop_front();
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
            "force_end 100",
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
            "force_end 100",
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

/// The manager has not recorded the exit by the time of the notice: the verb says why it blocks, once,
/// before the wait for the record that has no bound, after a signal and for a process that ended on its
/// own alike.
#[skuld::test]
fn a_wait_for_the_manager_to_record_the_exit_says_why_before_it_blocks() {
    let notice = "notice frpc: waiting for the manager to stop reporting process 100";
    let signalled = Scripted::new([Some(100), Some(100)], [Open::Found]).lagging_record();
    let ended = Scripted::new([Some(100), Some(100)], [Open::Found])
        .disowning()
        .lagging_record();

    for (what, mut target, before) in [
        (
            "after a signal",
            signalled,
            vec!["force_end 100", "await_exit 100 for the notice"],
        ),
        ("for an ended process", ended, vec![]),
    ] {
        assert_eq!(kill_service(&mut target, ID).unwrap(), KillOutcome::Ended, "{what}");
        let log = target.log();
        let at = log.iter().position(|call| call == notice).expect(what);
        assert_eq!(
            log[at - 1],
            "await_recorded 100",
            "{what}: after the wait that was bounded"
        );
        assert_eq!(log[at + 1], "await_recorded 100", "{what}: before the one that is not");
        assert!(log[..at].iter().all(|call| !call.starts_with("notice")), "{what}");
        for call in before {
            assert!(log[..at].contains(&call.to_string()), "{what}: {call}");
        }
    }
}

/// The same for the manager's record of a process that was gone before it could be opened, which the hold
/// waits for.
#[skuld::test]
fn a_wait_for_the_record_of_a_process_gone_before_it_was_opened_says_why_before_it_blocks() {
    let mut target = Scripted::new([Some(100)], [Open::Gone]).lagging_record();

    assert_eq!(kill_service(&mut target, ID).unwrap(), KillOutcome::Ended);
    assert_eq!(
        target.log(),
        steps([
            "service_pid",
            "open 100",
            "await_recorded 100 unheld",
            "notice frpc: waiting for the manager to stop reporting process 100",
            "await_recorded 100 unheld",
        ])
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

/// A service that restarts under the kill is not chased: one process is held, and once the manager has
/// recorded its end nothing more is read of the service, whatever it runs now.
#[skuld::test]
fn a_restart_during_the_kill_is_not_followed() {
    // `opens` holds one entry: a second `open` would panic.
    let mut target = Scripted::new([Some(100), Some(100), Some(200), Some(300)], [Open::Found]);

    assert_eq!(kill_service(&mut target, ID).unwrap(), KillOutcome::Ended);
    assert_eq!(
        target.pids,
        [Some(300)],
        "the restarts were scripted to happen under the record wait"
    );
    assert_eq!(
        target.log(),
        steps([
            "service_pid",
            "open 100",
            "service_pid",
            "force_end 100",
            "await_exit 100 for the notice",
            "await_recorded 100",
            "close 100",
        ]),
        "one signal, and no read of the service after the record"
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

    assert_eq!(e.kind(), io::ErrorKind::Other);
    assert_eq!(
        kill_failed(ID, e).report().to_string(),
        "failed to kill `frpc`: opening process 100: access denied"
    );
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

/// The read that decides whether an open refusal is the service's own: both errors reach the caller,
/// behind what the kill was for, with the read's kind.
#[skuld::test]
fn a_failed_read_after_a_refused_open_reports_both_errors() {
    let mut target = Scripted::new([Some(100)], [Open::Fails]).failing_read(2);

    let e = kill_service(&mut target, ID).unwrap_err();

    assert_eq!(e.kind(), READ_FAILED_KIND);
    assert_eq!(
        kill_failed(ID, e).report().to_string(),
        "failed to kill `frpc`: the process (pid 100) could not be opened (access denied), and the re-read of \
         what runs the daemon failed: manager read failed"
    );
    assert_eq!(target.log(), steps(["service_pid", "open 100", "service_pid"]));
}

/// The code of the failure is the read's, the primary error; the refused open is named after it. The
/// kill's own context leads, as for any OS failure of a kill.
#[skuld::test]
fn the_os_code_of_a_failed_read_is_found_after_a_refused_open() {
    const READ_CODE: i32 = 1722;
    const OPEN_CODE: i32 = 5;
    let mut target = Scripted::new([Some(100)], [Open::FailsWithOsError(OPEN_CODE)])
        .failing_read(2)
        .reading_fails_with_os_error(READ_CODE);

    let e = kill_service(&mut target, ID).unwrap_err();
    let failed = kill_failed(ID, e);

    assert_eq!(failed.raw_os_error(), Some(READ_CODE));
    let read = io::Error::from_raw_os_error(READ_CODE).to_string();
    let open = io::Error::from_raw_os_error(OPEN_CODE).to_string();
    assert_eq!(
        failed.report().to_string(),
        format!(
            "failed to kill `{ID}`: the process (pid 100) could not be opened ({open}), and the re-read of what \
             runs the daemon failed: {read}"
        )
    );
}

/// The class of the read is the failure's, and the refused open and the kill are told with it: none is
/// dropped.
#[skuld::test]
fn a_failed_read_after_a_refused_open_is_the_reads_class_and_tells_the_open() {
    let read = Error::Undetermined {
        id: ID.to_string(),
        reason: Box::new(Error::os("read", io::Error::other("manager read failed"))),
        recovery: "re-run".to_string(),
    };
    let mut target = Scripted::new([Some(100)], [Open::Fails])
        .failing_read(2)
        .reading_fails_carrying(read);

    let failed = kill_failed(ID, kill_service(&mut target, ID).unwrap_err());

    assert!(failed.is_indeterminate(), "{failed:?}");
    assert_eq!(
        failed.report().to_string(),
        "failed to kill `frpc`: the process (pid 100) could not be opened (access denied), and the re-read of \
         what runs the daemon failed: cannot determine whether daemon `frpc` is installed: failed to read: \
         manager read failed. re-run"
    );
}

/// An error of goetia's that the refused open carries is written in full, as the report writes any:
/// its child's stderr and its remedy are not lost to the parenthesis it sits in.
#[skuld::test]
fn a_carried_error_inside_the_refused_open_keeps_its_stderr_and_remedy() {
    let open = Error::Undetermined {
        id: ID.to_string(),
        reason: Box::new(Error::Command {
            command: "launchctl print".to_string(),
            status: crate::error::error_tests::exit_status(1),
            stderr: "no such
service"
                .to_string(),
        }),
        recovery: "re-run as root".to_string(),
    };
    let mut target = Scripted::new([Some(100)], [Open::FailsCarrying(open)]).failing_read(2);

    let failed = kill_failed(ID, kill_service(&mut target, ID).unwrap_err());

    let report = failed.report().to_string();
    assert!(
        report.starts_with(&format!(
            "failed to kill `{ID}`: the process (pid 100) could not be opened ("
        )),
        "{report}"
    );
    assert!(
        report.contains(
            ": no such
service), and the re-read of what runs the daemon failed: manager read failed"
        ),
        "{report}"
    );
    assert!(report.ends_with(". re-run as root"), "{report}");
}

/// The confirming read, with the process held: it is let go and nothing is signalled.
#[skuld::test]
fn a_failed_confirming_read_closes_the_process_and_signals_nothing() {
    let mut target = Scripted::new([Some(100)], [Open::Found]).failing_read(2);

    let e = kill_service(&mut target, ID).unwrap_err();

    assert_eq!(e.kind(), READ_FAILED_KIND);
    assert_eq!(
        kill_failed(ID, e).report().to_string(),
        "failed to kill `frpc`: confirming that process 100 is the daemon's: manager read failed"
    );
    assert_eq!(
        target.log(),
        steps(["service_pid", "open 100", "service_pid", "close 100"])
    );
}

/// What a failed wait for the exit says of process 100, which every test here signals: the signal was
/// delivered and the exit is not established.
const EXIT_UNCONFIRMED: &str = "process 100 was signalled, but goetia could not establish that it exited";

/// What a failed wait for the manager's record says: the exit was confirmed.
const RECORD_UNREAD: &str =
    "process 100 was signalled and has exited, but goetia could not confirm that the manager recorded it";

/// What a failure after the signal reports: it was delivered, so the error must not read as a failed
/// kill. The cause and its kind are kept.
fn assert_signalled_but_unconfirmed(e: &io::Error, lead: &str, cause: &str, kind: io::ErrorKind) {
    let text = crate::error::Chain(e).to_string();
    assert!(text.starts_with(lead), "{text}");
    assert!(text.ends_with(cause), "{text}");
    assert!(!text.contains("kill failed"), "{text}");
    assert_eq!(e.kind(), kind);
}

/// The read after the signal: the kill cannot say the manager has let go, but the signal stands.
#[skuld::test]
fn a_failed_record_wait_after_the_signal_says_the_signal_was_delivered() {
    let mut target = Scripted::new([Some(100), Some(100)], [Open::Found]).failing_recorded();

    let e = kill_service(&mut target, ID).unwrap_err();

    assert_signalled_but_unconfirmed(&e, RECORD_UNREAD, "manager read failed", READ_FAILED_KIND);
    assert_eq!(
        target.log(),
        steps([
            "service_pid",
            "open 100",
            "service_pid",
            "force_end 100",
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

    assert_signalled_but_unconfirmed(&e, EXIT_UNCONFIRMED, "wait on the process failed", io::ErrorKind::Other);
    assert!(!target.log().contains(&"await_recorded 100".to_string()));
}

/// The signal stands and the process may well have exited, so a failed wait after it leaves the outcome
/// open, whichever wait failed. The CLI exits `4`, not `1`.
#[skuld::test]
fn a_failed_wait_after_the_signal_leaves_the_outcome_open() {
    for (what, target, exited) in [
        (
            "the exit wait",
            Scripted::new([Some(100), Some(100)], [Open::Found]).failing_exit(),
            false,
        ),
        (
            "the record wait",
            Scripted::new([Some(100), Some(100)], [Open::Found]).failing_recorded(),
            true,
        ),
    ] {
        let mut target = target;

        let failed = kill_failed(ID, kill_service(&mut target, ID).unwrap_err());

        assert!(failed.is_indeterminate(), "{what}: {failed:?}");
        assert!(
            matches!(failed.cause(), Error::KillUnconfirmed { pid: 100, signalled: true, exited: e, .. } if *e == exited),
            "{what}: {failed:?}"
        );
        #[cfg(feature = "cli")]
        assert_eq!(
            crate::cli::ExitCode::of(&failed),
            crate::cli::ExitCode::Indeterminate,
            "{what}"
        );
    }
}

/// A goetia error after the signal is open too, though alone it is an established failure: `cause()` is the
/// kill's, and the report prints the carried error under it.
#[skuld::test]
fn a_determinate_error_after_the_signal_is_open_and_is_carried_under_the_cause() {
    let alone = || Error::UnitNotLoaded { unit: "x".to_string() };
    assert!(!alone().is_indeterminate());

    for exited in [false, true] {
        let failed = kill_failed("x", kill_unconfirmed(7, true, exited, io::Error::other(alone())));

        assert!(failed.is_indeterminate(), "{failed:?}");
        let Error::KillUnconfirmed { source, .. } = failed.cause() else {
            panic!("the cause is the kill's: {failed:?}");
        };
        assert!(matches!(**source, Error::UnitNotLoaded { .. }), "{source:?}");
        assert!(
            failed.report().to_string().contains("LoadState=not-found"),
            "{}",
            failed.report()
        );
    }
}

/// What a person reads, for each wait: the signal was delivered, and either the exit is not established, or
/// the process exited and only the manager's record of it could not be confirmed. The OS code of the failed
/// wait is kept.
#[skuld::test]
fn a_failed_wait_after_the_signal_says_which_wait_failed() {
    const CODE: i32 = 1722;
    let os = io::Error::from_raw_os_error(CODE);
    for (exited, lead) in [
        (
            false,
            "process 100 was signalled, but goetia could not establish that it exited",
        ),
        (
            true,
            "process 100 was signalled and has exited, but goetia could not confirm that the manager recorded it",
        ),
    ] {
        let failed = kill_failed(
            ID,
            kill_unconfirmed(100, true, exited, io::Error::from_raw_os_error(CODE)),
        );

        assert_eq!(failed.raw_os_error(), Some(CODE));
        assert_eq!(failed.report().to_string(), format!("kill `{ID}`: {lead}: {os}"));
    }
}

/// A process that ended on its own before any signal is gone, and only the manager's record of it is
/// unconfirmed, so the kill is open (exit `4`) as one that signalled is, and its text claims no signal. The
/// three ways the record wait is reached: the process gone before it could be opened, the manager's pid
/// lagging past the first wait, and the process held not being the service's.
#[skuld::test]
fn a_failed_record_wait_for_a_process_found_gone_is_open_and_claims_no_signal() {
    let gone = Scripted::new([Some(100)], [Open::Gone]).failing_recorded();
    let lagging = Scripted::new([Some(100)], [Open::Gone])
        .lagging_record()
        .failing_recorded();
    let disowned = Scripted::new([Some(100)], [Open::Found]).disowning().failing_recorded();
    for (what, mut target) in [("gone", gone), ("lagging", lagging), ("disowned", disowned)] {
        let e = kill_service(&mut target, ID).unwrap_err();

        assert_eq!(e.kind(), READ_FAILED_KIND, "{what}");
        let failed = kill_failed(ID, e);
        assert!(failed.is_indeterminate(), "{what}: {failed:?}");
        assert!(
            matches!(
                failed.cause(),
                Error::KillUnconfirmed {
                    pid: 100,
                    signalled: false,
                    exited: true,
                    ..
                }
            ),
            "{what}: {failed:?}"
        );
        assert_eq!(
            failed.report().to_string(),
            "kill `frpc`: process 100 had already exited, but goetia could not confirm that the manager recorded it: manager read failed",
            "{what}"
        );
        assert!(!target.log().iter().any(|step| step.starts_with("force_end")), "{what}");
    }
}

/// A kill that fails after the manager named the process says which process, whichever step failed and
/// whenever: in the hold, or in the record wait after the hold found the process gone or not the service's.
#[skuld::test]
fn every_failure_of_a_kill_after_the_pid_is_read_says_which_process() {
    let refused = Scripted::new([Some(100)], [Open::Fails]);
    let confirming = Scripted::new([Some(100)], [Open::Found]).failing_read(2);
    let recording = Scripted::new([Some(100)], [Open::Gone]).failing_recorded();
    let recording_after_expiry = Scripted::new([Some(100)], [Open::Gone])
        .lagging_record()
        .failing_recorded();
    let recording_after_ended = Scripted::new([Some(100)], [Open::Found]).disowning().failing_recorded();
    for (what, mut target) in [
        ("refused open", refused),
        ("confirming read", confirming),
        ("record wait in the hold", recording),
        ("record wait after Expired", recording_after_expiry),
        ("record wait after Ended", recording_after_ended),
    ] {
        let e = kill_service(&mut target, ID).unwrap_err();

        let report = kill_failed(ID, e).report().to_string();

        assert!(report.contains("process 100"), "{what}: {report}");
    }
}

/// The hold itself says which process, for every caller of it: the one that waits on the process and the
/// one that kills it.
#[skuld::test]
fn every_failure_of_a_hold_after_the_pid_is_read_says_which_process() {
    let refused = Scripted::new([Some(100)], [Open::Fails]);
    let confirming = Scripted::new([Some(100)], [Open::Found]).failing_read(2);
    let recording = Scripted::new([Some(100)], [Open::Gone]).failing_recorded();
    for (what, mut target) in [
        ("refused open", refused),
        ("confirming read", confirming),
        ("record wait", recording),
    ] {
        let failed = hold_once(&mut target, Budget::Unbounded.start()).expect_err("a failed hold");

        let report = target_failed("hold", io::Error::from(failed)).report().to_string();

        assert!(report.contains("process 100"), "{what}: {report}");
    }
}

#[skuld::test]
fn a_failed_signal_is_an_error_and_waits_for_nothing() {
    let mut target = Scripted::new([Some(100), Some(100)], [Open::Found]).failing_terminate();

    let e = kill_service(&mut target, ID).unwrap_err();

    let text = crate::error::Chain(&e).to_string();
    assert!(
        text.contains("process 100 was held and confirmed as the daemon's, but signalling it failed"),
        "{text}"
    );
    assert!(text.ends_with("terminate failed"), "{text}");
    assert_eq!(
        target.log(),
        steps(["service_pid", "open 100", "service_pid", "force_end 100", "close 100"]),
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

    let Ok(Some(Hold::Held { pid, process })) = hold_once(&mut target, Budget::Unbounded.start()) else {
        panic!("a held process");
    };

    assert_eq!(pid, 100);
    assert_eq!(target.log(), steps(["service_pid", "open 100", "service_pid"]));
    drop(process);
    assert_eq!(target.log().last().map(String::as_str), Some("close 100"));
}

/// A service the manager reports with no process is not held: there is nothing to hold, and it is read once.
#[skuld::test]
fn a_service_with_no_process_is_not_held() {
    let mut target = Scripted::new([None], []);

    let hold = hold_once(&mut target, Budget::Unbounded.start()).expect("a read");

    assert!(hold.is_none(), "{hold:?}");
    assert_eq!(target.log(), steps(["service_pid"]));
}

/// A process gone before it could be opened is waited for in the manager, until the deadline: once the
/// manager has recorded it, it is [`Hold::Ended`].
#[skuld::test]
fn a_process_gone_before_the_open_is_ended_once_the_manager_records_it() {
    let mut target = Scripted::new([Some(100)], [Open::Gone]);

    let hold = hold_once(&mut target, Budget::Unbounded.start()).expect("a record");

    assert!(matches!(hold, Some(Hold::Ended { pid: 100, held: None })), "{hold:?}");
    assert_eq!(
        target.log(),
        steps(["service_pid", "open 100", "await_recorded 100 unheld"])
    );
}

// A pid already read --------------------------------------------------------------------------------------------------

/// A caller that has read the manager's pid itself (a launchd stop, which watches it before it holds it)
/// holds that pid: the manager is not asked for it again, only whether it still vouches for it.
#[skuld::test]
fn a_pid_the_caller_read_is_held_without_reading_the_manager_for_it() {
    let mut target = Scripted::new([Some(100)], [Open::Found]);

    let Ok(Hold::Held { pid, process }) = hold_pid(&mut target, 100) else {
        panic!("a held process");
    };

    assert_eq!(pid, 100);
    assert_eq!(target.log(), steps(["open 100", "service_pid"]));
    drop(process);
}

/// A pid whose process is gone is [`Hold::Ended`] with no read of the manager made for it: a caller that
/// paces its own next read (a launchd stop, whose deadline is spent) does not read twice, and a gone
/// process is never an error of that read.
#[skuld::test]
fn a_pid_the_caller_read_whose_process_is_gone_is_ended_with_no_read_of_the_manager() {
    let mut target = Scripted::new([Some(100)], [Open::Gone])
        .failing_recorded()
        .failing_read(1);

    let hold = hold_pid(&mut target, 100).expect("an ended process, not a read error");

    assert!(matches!(hold, Hold::Ended { pid: 100, held: None }), "{hold:?}");
    assert_eq!(target.log(), steps(["open 100"]), "the manager is not read");
}

/// The failed-open rule is the one `hold_once` has: a refusal only while the manager still names the pid.
#[skuld::test]
fn an_open_that_failed_on_a_pid_the_caller_read_is_a_refusal_only_while_the_manager_names_it() {
    let mut named = Scripted::new([Some(100)], [Open::Fails]);
    let error = hold_pid(&mut named, 100).expect_err("a refusal");
    assert!(matches!(error, HoldError::Refused(_)), "{error:?}");
    assert_eq!(
        kill_failed(ID, error.into()).report().to_string(),
        "failed to kill `frpc`: opening process 100: access denied"
    );
    assert_eq!(named.log(), steps(["open 100", "service_pid"]));

    for moved_off in [Some(200), None] {
        let mut gone = Scripted::new([moved_off], [Open::Fails]);
        let hold = hold_pid(&mut gone, 100).expect("an ended process, not a refusal");
        assert!(
            matches!(hold, Hold::Ended { pid: 100, held: None }),
            "{moved_off:?}: {hold:?}"
        );
    }
}

// The error of a kill -------------------------------------------------------------------------------------------------

/// An error of goetia's own that a target passed through an `io::Error` is the cause of the kill's failure,
/// so a caller matching `cause()` on `NoManager` or `Command` still finds it, and what was being killed is
/// told before it.
#[skuld::test]
fn a_wrapped_error_is_the_cause_of_the_kills_failure_told_under_the_kill() {
    let e = io::Error::other(Error::NoManager {
        evidence: "x".to_string(),
    });

    let failed = kill_failed("x", e);

    assert!(matches!(failed.cause(), Error::NoManager { .. }), "{failed:?}");
    assert!(
        failed.report().to_string().starts_with("failed to kill `x`: "),
        "{}",
        failed.report()
    );
}

/// Any other failure is an OS failure of the kill, naming it.
#[skuld::test]
fn any_other_error_is_an_os_failure_of_the_kill() {
    let failed = kill_failed("x", io::Error::other("access denied"));

    assert!(
        matches!(&failed, Error::Os { what, .. } if what == "kill `x`"),
        "{failed:?}"
    );
}

/// A failure after the signal was delivered, plain or of goetia's own, is told under the kill without
/// saying the kill failed: the signal stands. A failure to signal is a failed kill, whose context says
/// how far it got.
#[skuld::test]
fn only_a_delivered_signal_keeps_the_report_from_reading_as_a_failed_kill() {
    let wrapped = || io::Error::other(Error::UnitNotLoaded { unit: "x".to_string() });
    for (e, lead) in [
        (
            kill_unconfirmed(7, true, false, io::Error::other("wait failed")),
            "kill `x`: process 7 was signalled, but goetia could not establish",
        ),
        (
            kill_unconfirmed(7, true, true, wrapped()),
            "kill `x`: process 7 was signalled and has exited",
        ),
        (
            not_signalled(7, io::Error::other("terminate failed")),
            "failed to kill `x`: process 7 was held and confirmed",
        ),
        (
            not_signalled(7, wrapped()),
            "failed to kill `x`: process 7 was held and confirmed",
        ),
    ] {
        let report = kill_failed("x", e).report().to_string();

        assert!(report.starts_with(lead), "{report}");
        assert_eq!(
            report.contains("failed to kill"),
            lead.starts_with("failed"),
            "{report}"
        );
    }
}

/// A failure of goetia's own that came before the signal is the step's source itself, so a caller matching
/// on `cause()` finds it.
#[skuld::test]
fn a_wrapped_error_before_the_signal_is_the_cause_of_the_failed_kill() {
    let wrapped = io::Error::other(Error::UnitNotLoaded { unit: "x".to_string() });

    let failed = kill_failed("x", not_signalled(7, wrapped));

    let Error::Step { what: kill, source } = &failed else {
        panic!("a step: {failed:?}");
    };
    let Error::Step { what, source } = &**source else {
        panic!("a step under the kill: {failed:?}");
    };
    assert_eq!(kill, "failed to kill `x`");
    assert!(what.contains("signalling it failed"), "{what}");
    assert!(matches!(**source, Error::UnitNotLoaded { .. }), "{source:?}");
    assert!(matches!(failed.cause(), Error::UnitNotLoaded { .. }), "{failed:?}");
}
