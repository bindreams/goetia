//! Tests of what SCM's side of `kill` adds to the state machine (`backend::kill`): the hold `stop`
//! and `uninstall` take, and the wait for SCM to record an exit, against a scripted
//! [`KillTarget`]: no SCM and no process involved, so these run on every platform.

use std::cell::RefCell;
use std::collections::VecDeque;
use std::rc::Rc;

use super::*;
use crate::backend::kill::{KillOutcome, kill_service};
use crate::backend::scm::test_support::{spent, unbounded};
use crate::backend::scm::wait::Waited;
use crate::error::Error;
use crate::manager::budget::Deadline;

type Log = Rc<RefCell<Vec<String>>>;

/// The kind of a failed `service_pid`: not `Other`, so a test sees whether a kind is kept.
const READ_FAILED_KIND: io::ErrorKind = io::ErrorKind::TimedOut;

/// The OS code of an open that fails: `ERROR_ACCESS_DENIED`.
const OPEN_DENIED: i32 = 5;

/// What opening a pid finds.
enum Open {
    Found,
    Gone,
    Fails,
}

/// What SCM answers each `service_pid`, and what each `open` does.
struct Scripted {
    /// Front to back; the last answer repeats.
    pids: VecDeque<Option<u32>>,
    opens: VecDeque<Open>,
    /// Which `service_pid` calls, counted from 1, fail instead of answering.
    failing_reads: Vec<usize>,
    reads: usize,
    /// Whether `arm_release` fails.
    failing_arm: bool,
    /// Whether the deadline passes during `arm_release`, or during `await_release`.
    expiring_arm: bool,
    expiring_release: bool,
    /// How many times the kill has blocked for SCM to let go.
    releases: usize,
    /// How many `arm_release` calls have been made.
    arms: usize,
    /// From which `arm_release` call, counted from 0, SCM refuses to watch the service.
    unwatchable_from: Option<usize>,
    /// Whether an `arm_release` is outstanding: made, and neither woken by `await_release` nor
    /// cancelled. SCM allows one per service handle and buffer.
    armed: bool,
    log: Log,
}

impl Scripted {
    fn new<const P: usize, const O: usize>(pids: [Option<u32>; P], opens: [Open; O]) -> Self {
        Self {
            pids: pids.into(),
            opens: opens.into(),
            failing_reads: Vec::new(),
            reads: 0,
            failing_arm: false,
            expiring_arm: false,
            expiring_release: false,
            releases: 0,
            arms: 0,
            unwatchable_from: None,
            armed: false,
            log: Log::default(),
        }
    }

    /// The `n`th `service_pid` call (from 1) fails.
    fn failing_read(mut self, n: usize) -> Self {
        self.failing_reads.push(n);
        self
    }

    /// `arm_release` fails.
    fn failing_arm(mut self) -> Self {
        self.failing_arm = true;
        self
    }

    /// The deadline passes while `arm_release` is trying to register.
    fn expiring_arm(mut self) -> Self {
        self.expiring_arm = true;
        self
    }

    /// The deadline passes while `await_release` blocks: the registration is still outstanding.
    fn expiring_release(mut self) -> Self {
        self.expiring_release = true;
        self
    }

    /// SCM refuses every `arm_release` from the `n`th (from 0) with 1072, the service being marked for
    /// deletion and `STOPPED`.
    fn unwatchable_from(mut self, n: usize) -> Self {
        self.unwatchable_from = Some(n);
        self
    }

    fn log(&self) -> Vec<String> {
        self.log.borrow().clone()
    }
}

/// A held process: logs when it is closed, so a test sees how long the handle was held.
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
            return Err(io::Error::new(READ_FAILED_KIND, "scm read failed"));
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
            Open::Fails => Err(io::Error::from_raw_os_error(OPEN_DENIED)),
        }
    }

    fn force_end(&mut self, process: &Held) -> io::Result<()> {
        self.log.borrow_mut().push(format!("force_end {}", process.pid));
        Ok(())
    }

    fn await_exit(&mut self, process: &Held, _deadline: Deadline) -> io::Result<Waited> {
        self.log.borrow_mut().push(format!("await_exit {}", process.pid));
        Ok(Waited::Confirmed)
    }

    fn await_recorded(&mut self, pid: u32, _held: Option<&Held>, deadline: Deadline) -> io::Result<Waited> {
        await_scm_released(self, pid, deadline)
    }
}

impl ScmRelease for Scripted {
    fn arm_release(&mut self, deadline: Deadline) -> io::Result<ReleaseWatch> {
        self.log.borrow_mut().push("arm_release".to_string());
        if self.failing_arm {
            return Err(io::Error::other("notifier could not be opened"));
        }
        if self.expiring_arm {
            assert!(
                deadline.expired(),
                "Scripted: arm_release expires only once its deadline has passed; an unbounded or unspent deadline never expires"
            );
            return Ok(ReleaseWatch::Expired);
        }
        assert!(
            !self.armed,
            "arm_release over an outstanding registration: SCM allows one per service and buffer"
        );
        // A marked service that SCM recorded `STOPPED` refuses the registration: nothing is outstanding.
        if self.unwatchable_from.is_some_and(|n| self.arms >= n) {
            self.arms += 1;
            return Ok(ReleaseWatch::Stopped);
        }
        self.arms += 1;
        self.armed = true;
        Ok(ReleaseWatch::Armed)
    }

    fn cancel_release(&mut self) {
        self.log.borrow_mut().push("cancel_release".to_string());
        assert!(self.armed, "cancel_release with no registration outstanding");
        self.armed = false;
    }

    fn await_release(&mut self, deadline: Deadline) -> io::Result<Waited> {
        self.log.borrow_mut().push("await_release".to_string());
        assert!(self.armed, "await_release with no registration to wake");
        if self.expiring_release {
            assert!(
                deadline.expired(),
                "Scripted: await_release expires only once its deadline has passed; an unbounded or unspent deadline never expires"
            );
            // Still registered: the caller cancels it.
            return Ok(Waited::Expired);
        }
        self.armed = false;
        self.releases += 1;
        assert!(
            self.releases <= 5,
            "the script never released the pid; the wait would not end"
        );
        Ok(Waited::Confirmed)
    }
}

fn steps<const N: usize>(steps: [&str; N]) -> Vec<String> {
    steps.iter().map(|s| s.to_string()).collect()
}

/// What `uninstall` holds before it stops a service: the same open, the same confirmation, and no
/// termination.
#[skuld::test]
fn holding_a_service_with_no_process_holds_nothing() {
    let mut target = Scripted::new([None], []);

    assert!(hold_service_process(&mut target, unbounded()).unwrap().is_none());
    assert_eq!(target.log(), steps(["service_pid"]));
}

#[skuld::test]
fn holding_keeps_the_process_once_scm_confirms_its_pid() {
    let mut target = Scripted::new([Some(100), Some(100)], [Open::Found]);

    let (pid, process) = hold_service_process(&mut target, unbounded())
        .unwrap()
        .expect("a held process");

    assert_eq!(pid, 100);
    assert_eq!(target.log(), steps(["service_pid", "open 100", "service_pid"]));
    drop(process);
    assert_eq!(target.log().last().map(String::as_str), Some("close 100"));
}

#[skuld::test]
fn holding_follows_the_service_when_it_restarts_between_the_open_and_the_confirmation() {
    let mut target = Scripted::new([Some(100), Some(200), Some(200)], [Open::Found, Open::Found]);

    let (pid, _process) = hold_service_process(&mut target, unbounded())
        .unwrap()
        .expect("a held process");

    assert_eq!(pid, 200);
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
        ])
    );
}

/// A pid that cannot be opened is the caller's to see only while SCM still says it is the service's.
#[skuld::test]
fn holding_refuses_a_pid_it_cannot_open_while_scm_vouches_for_it() {
    let mut target = Scripted::new([Some(100), Some(100)], [Open::Fails]);

    let error = hold_service_process(&mut target, unbounded()).expect_err("a refused open");

    assert!(matches!(error, HoldError::Refused(_)), "{error:?}");
    let refused = Error::os("hold", io::Error::from(error));
    assert_eq!(refused.raw_os_error(), Some(OPEN_DENIED));
    assert!(
        refused.report().to_string().contains("opening process 100"),
        "{refused:?}"
    );
    assert_eq!(target.log(), steps(["service_pid", "open 100", "service_pid"]));
}

/// A stop under a budget holds the process first, and the hold waits on SCM's release only until
/// the stop's deadline: a pid SCM keeps reporting past it is [`HoldError::Expired`], with nothing
/// held and no registration left outstanding.
#[skuld::test]
fn a_release_scm_does_not_make_by_the_deadline_expires_the_hold() {
    let mut target = Scripted::new([Some(100), Some(100)], [Open::Gone]).expiring_release();

    let error = hold_service_process(&mut target, spent()).expect_err("an expired hold");

    assert!(matches!(error, HoldError::Expired { pid: 100 }), "{error:?}");
    assert_eq!(
        target.log(),
        steps([
            "service_pid",
            "open 100",
            "arm_release",
            "service_pid",
            "await_release",
            "cancel_release"
        ])
    );
}

#[skuld::test]
fn an_arm_that_expires_expires_the_hold_with_nothing_to_cancel() {
    let mut target = Scripted::new([Some(100)], [Open::Gone]).expiring_arm();

    let error = hold_service_process(&mut target, spent()).expect_err("an expired hold");

    assert!(matches!(error, HoldError::Expired { pid: 100 }), "{error:?}");
    assert_eq!(target.log(), steps(["service_pid", "open 100", "arm_release"]));
}

/// An expiry is not a process that could not be opened: `hold_if_possible` lets go of the refusal
/// only, and what an expiry means is its caller's to say.
#[skuld::test]
fn an_expired_hold_is_not_let_go_by_hold_if_possible() {
    let mut release = Scripted::new([Some(100), Some(100)], [Open::Gone]).expiring_release();
    let mut arm = Scripted::new([Some(100)], [Open::Gone]).expiring_arm();

    for target in [&mut release, &mut arm] {
        let outcome = hold_if_possible(target, spent()).expect("an expiry is an outcome, not a failure");
        assert!(matches!(outcome, HoldOutcome::Expired { pid: 100 }), "{outcome:?}");
    }
}

/// `uninstall` waits for the process only if it can hold it: a process it cannot open (a right
/// removed by policy) must not stop the service from being uninstalled, and is left to the sweep,
/// which finds it by its image. The outcome carries why it is not held, so the caller can say that no
/// wait for it was possible.
#[skuld::test]
fn a_process_that_cannot_be_opened_is_not_held_and_not_an_error() {
    let mut target = Scripted::new([Some(100), Some(100)], [Open::Fails]);

    let outcome = hold_if_possible(&mut target, unbounded()).unwrap();

    let HoldOutcome::Refused(reason) = outcome else {
        panic!("expected the refusal, got {outcome:?}");
    };
    let refused = Error::os("hold", reason);
    assert_eq!(refused.raw_os_error(), Some(OPEN_DENIED));
    assert!(
        refused.report().to_string().contains("opening process 100"),
        "{refused:?}"
    );
    assert_eq!(target.log(), steps(["service_pid", "open 100", "service_pid"]));
}

#[skuld::test]
fn a_service_with_no_process_is_no_process_to_hold_if_possible() {
    let mut target = Scripted::new([None], []);

    let outcome = hold_if_possible(&mut target, unbounded()).unwrap();

    assert!(matches!(outcome, HoldOutcome::NoProcess), "{outcome:?}");
}

#[skuld::test]
fn a_process_that_can_be_opened_is_held_by_hold_if_possible() {
    let mut target = Scripted::new([Some(100), Some(100)], [Open::Found]);

    let outcome = hold_if_possible(&mut target, unbounded()).unwrap();

    assert!(matches!(outcome, HoldOutcome::Held(_)), "{outcome:?}");
}

/// Only the refusal is let go. A failed read of SCM is not a process that cannot be opened: the
/// uninstall fails with nothing changed, whether the read was the first or the one after a refusal.
#[skuld::test]
fn a_failed_read_of_scm_is_not_let_go_by_hold_if_possible() {
    let mut first = Scripted::new([Some(100)], []).failing_read(1);
    let mut after_refusal = Scripted::new([Some(100), Some(100)], [Open::Fails]).failing_read(2);

    for target in [&mut first, &mut after_refusal] {
        let error = hold_if_possible(target, unbounded()).expect_err("an SCM read failure");
        assert_eq!(error.kind(), READ_FAILED_KIND);
    }
}

/// SCM has not yet recorded the exit of a process that is already gone: the kill blocks for SCM's
/// release rather than re-reading in a loop. That wait is the record's, so the kill makes no second one.
#[skuld::test]
fn a_process_gone_before_it_was_opened_waits_for_scm_to_let_go_of_the_pid() {
    let mut target = Scripted::new([Some(100), Some(100), None], [Open::Gone]);

    assert_eq!(kill_service(&mut target, "frpc").unwrap(), KillOutcome::Ended);
    assert_eq!(
        target.log(),
        steps([
            "service_pid",
            "open 100",
            "arm_release",
            "service_pid",
            "await_release",
            "service_pid",
        ])
    );
}

/// A wake that shows SCM let go of the pid ends the wait: nothing arms again first. SCM answers an arm
/// on a delete-marked, `STOPPED` service with 1072 (round 8), so an arm after that wake would fail a
/// kill that worked.
#[skuld::test]
fn a_wake_that_shows_scm_let_go_of_the_pid_arms_nothing_again() {
    let mut target = Scripted::new([Some(100), Some(100), None], [Open::Gone]).unwatchable_from(1);

    assert_eq!(kill_service(&mut target, "frpc").unwrap(), KillOutcome::Ended);
    assert_eq!(
        target.log(),
        steps([
            "service_pid",
            "open 100",
            "arm_release",
            "service_pid",
            "await_release",
            "service_pid",
        ])
    );
}

/// SCM refuses to watch a marked service it recorded `STOPPED`, and that status is the release: the
/// pid is not read again, and nothing waits on a registration that does not exist.
#[skuld::test]
fn a_service_scm_will_not_watch_and_shows_stopped_is_released() {
    let mut target = Scripted::new([Some(100), None], [Open::Gone]).unwatchable_from(0);

    assert_eq!(kill_service(&mut target, "frpc").unwrap(), KillOutcome::Ended);
    assert_eq!(
        target.log(),
        steps(["service_pid", "open 100", "arm_release"]),
        "the arm is refused, and that is the release"
    );
}

/// The same for the arm after a wake that still shows the pid.
#[skuld::test]
fn a_re_arm_scm_refuses_on_a_stopped_service_is_the_release() {
    let mut target =
        Scripted::new([Some(100), Some(100), Some(100), Some(100), None], [Open::Found]).unwatchable_from(1);

    assert_eq!(kill_service(&mut target, "frpc").unwrap(), KillOutcome::Ended);
    assert_eq!(
        target.log(),
        steps([
            "service_pid",
            "open 100",
            "service_pid",
            "force_end 100",
            "await_exit 100",
            "arm_release",
            "service_pid",
            "await_release",
            "service_pid",
            "arm_release",
            "close 100",
        ])
    );
}

/// SCM records the exit after the termination is delivered, so a read straight after it still shows
/// the old pid and the old state (CI run 40262088279: `uninstall` straight after `kill` saw RUNNING).
/// The kill is not done until SCM has let go: it blocks for the release and reads again.
#[skuld::test]
fn a_kill_waits_while_scm_still_reports_the_killed_pid() {
    let mut target = Scripted::new(
        [Some(100), Some(100), Some(100), Some(100), Some(100), None],
        [Open::Found],
    );

    assert_eq!(kill_service(&mut target, "frpc").unwrap(), KillOutcome::Ended);
    assert_eq!(
        target.log(),
        steps([
            "service_pid",
            "open 100",
            "service_pid",
            "force_end 100",
            "await_exit 100",
            "arm_release",
            "service_pid",
            "await_release",
            "service_pid",
            "arm_release",
            "service_pid",
            "await_release",
            "service_pid",
            "close 100",
        ]),
        "the read comes after each arm, so a change between a read and an arm is not missed, and after \
         each wake, before any arm"
    );
}

/// A recovery action restarted the service: SCM has moved off the killed pid, so there is nothing
/// to wait for, and the new process is not the kill's to touch.
#[skuld::test]
fn a_kill_is_done_when_scm_reports_a_new_pid() {
    let mut target = Scripted::new([Some(100), Some(100), Some(200)], [Open::Found]);

    assert_eq!(kill_service(&mut target, "frpc").unwrap(), KillOutcome::Ended);
    assert_eq!(
        target.log(),
        steps([
            "service_pid",
            "open 100",
            "service_pid",
            "force_end 100",
            "await_exit 100",
            "arm_release",
            "service_pid",
            "cancel_release",
            "close 100",
        ])
    );
}

// A failed read of SCM ------------------------------------------------------------------------------------------------

/// What a failure after a delivered termination reports: the process is dead, so the error must not
/// read as a failed kill. The cause and its kind are kept, and the kill is open (exit `4`): its cause is
/// `KillUnconfirmed`, the manager's record unconfirmed.
fn assert_terminated_but_unconfirmed(e: io::Error, cause: &str, kind: io::ErrorKind) {
    let text = crate::error::Chain(&e).to_string();
    assert!(
        text.starts_with(
            "process 100 was signalled and has exited, but goetia could not confirm that the manager recorded it"
        ),
        "{text}"
    );
    assert!(text.ends_with(cause), "{text}");
    assert!(!text.contains("kill failed"), "{text}");
    assert_eq!(e.kind(), kind);
    let failed = crate::backend::kill::kill_failed("frpc", e);
    assert!(failed.is_indeterminate(), "{failed:?}");
    assert!(
        matches!(
            failed.cause(),
            crate::Error::KillUnconfirmed {
                progress: crate::KillProgress::Exited,
                ..
            }
        ),
        "{failed:?}"
    );
}

/// The read after the termination: the kill cannot say SCM has let go, but the process is gone.
#[skuld::test]
fn a_failed_read_after_the_termination_says_the_signal_was_delivered() {
    let mut target = Scripted::new([Some(100)], [Open::Found]).failing_read(3);

    let e = kill_service(&mut target, "frpc").unwrap_err();

    assert_terminated_but_unconfirmed(e, "scm read failed", READ_FAILED_KIND);
    assert_eq!(
        target.log(),
        steps([
            "service_pid",
            "open 100",
            "service_pid",
            "force_end 100",
            "await_exit 100",
            "arm_release",
            "service_pid",
            "close 100",
        ])
    );
}

#[skuld::test]
fn a_failed_registration_after_the_termination_says_the_signal_was_delivered() {
    let mut target = Scripted::new([Some(100)], [Open::Found]).failing_arm();

    let e = kill_service(&mut target, "frpc").unwrap_err();

    assert_terminated_but_unconfirmed(e, "notifier could not be opened", io::ErrorKind::Other);
}

/// A process that ended before any termination is gone, and only SCM's record of it is unconfirmed: the
/// kill is open, and its text claims no termination.
#[skuld::test]
fn a_failed_registration_for_a_process_found_gone_is_open_and_claims_no_termination() {
    let mut target = Scripted::new([Some(100)], [Open::Gone]).failing_arm();

    let e = kill_service(&mut target, "frpc").unwrap_err();

    let failed = crate::backend::kill::kill_failed("frpc", e);
    assert!(failed.is_indeterminate(), "{failed:?}");
    assert_eq!(
        failed.report().to_string(),
        "kill `frpc`: process 100 had already exited, but goetia could not confirm that the manager recorded it: notifier could not be opened"
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
