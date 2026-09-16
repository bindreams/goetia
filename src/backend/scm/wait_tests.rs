//! Pure orchestration tests for [`super::stop_via_notify`]/
//! [`super::start_via_notify`] against a scripted [`super::ScmActor`] — no
//! real SCM involved, so these run on every platform and need no elevation.
//! Expiry is driven by the script, never by the clock: nothing here waits.
//! Ported from `~/src/hole/crates/bridge/src/cutover/scm_wait_tests.rs`.

use std::collections::VecDeque;

use super::*;
use crate::manager::budget::{Budget, Deadline};

/// Records the granular SCM steps in call order and replays a scripted
/// outcome for each `wait_callback` — `None` being "the deadline expired
/// with no callback delivered". [`Self::arming`] reaches the *other* expiry,
/// the one a bounded [`ScmActor::arm`] reports when the SCM keeps answering
/// `ERROR_SERVICE_NOTIFY_CLIENT_LAGGING` past the deadline.
struct FakeScm {
    log: Vec<&'static str>,
    /// What the next `wait_callback` calls return, front to back.
    waits: VecDeque<Option<Observed>>,
    what_arm_reports: Waited,
    start_fails: bool,
    /// The deadline each `wait_callback` was handed, in call order.
    deadlines: Vec<Deadline>,
}

impl FakeScm {
    fn new<const N: usize>(waits: [Option<Observed>; N]) -> Self {
        Self {
            log: vec![],
            waits: waits.into(),
            what_arm_reports: Waited::Confirmed,
            start_fails: false,
            deadlines: vec![],
        }
    }

    /// Every `arm` reports `reports` rather than `Waited::Confirmed`.
    fn arming(mut self, reports: Waited) -> Self {
        self.what_arm_reports = reports;
        self
    }

    /// `start` reports failure — the shape the *shipped*
    /// `ERROR_SERVICE_ALREADY_RUNNING` arm produced for a queried
    /// `StartPending`.
    fn rejecting_start(mut self) -> Self {
        self.start_fails = true;
        self
    }

    fn arm_count(&self) -> usize {
        self.log.iter().filter(|step| step.starts_with("arm_")).count()
    }
}

impl ScmActor for FakeScm {
    fn arm(&mut self, want: WantState, _deadline: Deadline) -> std::io::Result<Waited> {
        self.log.push(match want {
            WantState::Stopped => "arm_stopped",
            WantState::Running => "arm_running",
        });
        Ok(self.what_arm_reports)
    }
    fn control_stop(&mut self) -> std::io::Result<()> {
        self.log.push("control_stop");
        Ok(())
    }
    fn start(&mut self) -> std::io::Result<()> {
        self.log.push("start");
        if self.start_fails {
            return Err(std::io::Error::other("scripted start failure"));
        }
        Ok(())
    }
    fn wait_callback(&mut self, deadline: Deadline) -> std::io::Result<Option<Observed>> {
        self.log.push("wait");
        self.deadlines.push(deadline);
        let observed = self.waits.pop_front().expect("script ran dry");
        self.log.push(match observed {
            Some(Observed::Stopped) => "got_stopped",
            Some(Observed::Running) => "got_running",
            Some(Observed::Pending) => "got_pending",
            None => "got_nothing",
        });
        Ok(observed)
    }
}

fn unbounded() -> Deadline {
    Budget::Unbounded.start()
}

/// An already-spent deadline. The fake's script is what makes a wait report
/// expiry; this is only what the state machine is *told* it has left.
fn spent() -> Deadline {
    Budget::Immediate.start()
}

#[skuld::test]
fn stop_via_notify_arms_stopped_then_gates_on_stopped() {
    let mut fake = FakeScm::new([Some(Observed::Stopped)]);
    assert_eq!(stop_via_notify(&mut fake, unbounded()).unwrap(), Waited::Confirmed);
    assert_eq!(fake.log, vec!["arm_stopped", "control_stop", "wait", "got_stopped"]);
}

#[skuld::test]
fn stop_re_arms_after_a_non_terminal_callback() {
    // A pending (intermediate) callback fires while waiting for STOPPED, then
    // STOPPED; the non-terminal callback must trigger a re-arm.
    let mut fake = FakeScm::new([Some(Observed::Pending), Some(Observed::Stopped)]);
    assert_eq!(stop_via_notify(&mut fake, unbounded()).unwrap(), Waited::Confirmed);
    assert_eq!(
        fake.log,
        vec![
            "arm_stopped",
            "control_stop",
            "wait",
            "got_pending",
            "arm_stopped", // re-arm: still waiting for STOPPED
            "wait",
            "got_stopped",
        ]
    );
}

#[skuld::test]
fn start_via_notify_arms_running_before_start_then_gates_on_running() {
    let mut fake = FakeScm::new([Some(Observed::Running)]);
    assert_eq!(start_via_notify(&mut fake, unbounded()).unwrap(), Waited::Confirmed);
    // The critical ordering: arm RUNNING strictly BEFORE start, else a
    // RUNNING reached before the arm fires only on the NEXT entry — a hang.
    assert_eq!(fake.log, vec!["arm_running", "start", "wait", "got_running"]);
}

#[skuld::test]
fn start_re_arms_after_a_non_terminal_callback() {
    // A StartPending/StopPending intermediate (Pending) fires while waiting
    // for RUNNING, then RUNNING; the non-terminal callback must trigger a
    // re-arm.
    let mut fake = FakeScm::new([Some(Observed::Pending), Some(Observed::Running)]);
    assert_eq!(start_via_notify(&mut fake, unbounded()).unwrap(), Waited::Confirmed);
    assert_eq!(
        fake.log,
        vec![
            "arm_running",
            "start",
            "wait",
            "got_pending",
            "arm_running", // re-arm: still waiting for RUNNING
            "wait",
            "got_running",
        ]
    );
}

#[skuld::test]
fn a_terminal_stopped_after_start_is_still_an_error_not_an_expiry() {
    // A service that stopped instead of reaching Running is a *failed
    // start*, not a wait that ran out of budget — the caller's remedy
    // differs, so the two must not collapse into one another.
    let mut fake = FakeScm::new([Some(Observed::Pending), Some(Observed::Stopped)]);
    assert!(start_via_notify(&mut fake, unbounded()).is_err());
}

#[skuld::test]
fn start_via_notify_ok_when_service_runs() {
    let mut fake = FakeScm::new([Some(Observed::Pending), Some(Observed::Running)]);
    assert_eq!(start_via_notify(&mut fake, unbounded()).unwrap(), Waited::Confirmed);
}

#[skuld::test]
fn start_via_notify_rearms_on_pending_not_errs() {
    // A StartPending/StopPending intermediate (Pending) must RE-ARM, not fail.
    let mut fake = FakeScm::new([
        Some(Observed::Pending),
        Some(Observed::Pending),
        Some(Observed::Running),
    ]);
    assert_eq!(start_via_notify(&mut fake, unbounded()).unwrap(), Waited::Confirmed);
    assert_eq!(fake.arm_count(), 3); // initial + 2 re-arms
}

#[skuld::test]
fn stop_via_notify_ok_when_already_stopped_on_first_callback() {
    let mut fake = FakeScm::new([Some(Observed::Stopped)]);
    assert_eq!(stop_via_notify(&mut fake, unbounded()).unwrap(), Waited::Confirmed);
    assert_eq!(fake.arm_count(), 1);
}

#[skuld::test]
fn a_start_wait_whose_deadline_expires_reports_expired() {
    let mut fake = FakeScm::new([Some(Observed::Pending), None]);
    assert_eq!(start_via_notify(&mut fake, spent()).unwrap(), Waited::Expired);
}

#[skuld::test]
fn a_stop_wait_whose_deadline_expires_reports_expired() {
    let mut fake = FakeScm::new([Some(Observed::Pending), None]);
    assert_eq!(stop_via_notify(&mut fake, spent()).unwrap(), Waited::Expired);
}

#[skuld::test]
fn an_expired_deadline_still_issues_the_start_request() {
    // Only the *waiting* is bounded. A service that failed to start because
    // goetia ran out of budget while watching for it would be the worst of
    // both answers.
    let mut fake = FakeScm::new([None]);
    assert_eq!(start_via_notify(&mut fake, spent()).unwrap(), Waited::Expired);
    assert_eq!(fake.log, vec!["arm_running", "start", "wait", "got_nothing"]);
}

#[skuld::test]
fn a_pending_callback_rearms_under_the_same_deadline() {
    // Re-arming must not restart the clock: a service that chatters
    // START_PENDING forever has to terminate at the caller's budget.
    let mut fake = FakeScm::new([Some(Observed::Pending), Some(Observed::Running)]);
    assert_eq!(start_via_notify(&mut fake, unbounded()).unwrap(), Waited::Confirmed);
    assert_eq!(fake.arm_count(), 2);
    assert_eq!(fake.deadlines[0], fake.deadlines[1]);
}

#[skuld::test]
fn an_arm_that_keeps_lagging_returns_expired_instead_of_looping() {
    // `Waited::Expired` from `arm` is what a bounded arm reports when the
    // SCM answers ERROR_SERVICE_NOTIFY_CLIENT_LAGGING past the deadline. The
    // empty script proves no wait was entered; the log proves the request
    // still went out.
    let mut fake = FakeScm::new([]).arming(Waited::Expired);
    assert_eq!(start_via_notify(&mut fake, spent()).unwrap(), Waited::Expired);
    assert_eq!(fake.log, vec!["arm_running", "start"]);
}

#[skuld::test]
fn a_start_issued_while_start_pending_resolves_through_the_wait() {
    // The shape the corrected ERROR_SERVICE_ALREADY_RUNNING arm produces for
    // a queried `StartPending`: `start` reports success, and the wait —
    // which `want_to_mask` armed SERVICE_NOTIFY_START_PENDING for — resolves
    // through the immediate-fire and a re-arm.
    let mut fake = FakeScm::new([Some(Observed::Pending), Some(Observed::Running)]);
    assert_eq!(start_via_notify(&mut fake, unbounded()).unwrap(), Waited::Confirmed);
    assert_eq!(
        fake.log,
        vec![
            "arm_running",
            "start",
            "wait",
            "got_pending",
            "arm_running",
            "wait",
            "got_running"
        ]
    );

    // The shipped arm rejected that state instead, failing a start that was
    // going to succeed.
    let mut rejecting = FakeScm::new([]).rejecting_start();
    assert!(start_via_notify(&mut rejecting, unbounded()).is_err());
}

#[skuld::test]
fn request_start_arms_nothing_and_waits_for_nothing() {
    let mut fake = FakeScm::new([]);
    request_start(&mut fake).unwrap();
    assert_eq!(fake.log, vec!["start"]);
}

#[skuld::test]
fn request_stop_arms_nothing_and_waits_for_nothing() {
    let mut fake = FakeScm::new([]);
    request_stop(&mut fake).unwrap();
    assert_eq!(fake.log, vec!["control_stop"]);
}

// handle teardown -----------------------------------------------------------------------------------------------------

/// Records which of [`ScmHandles`]' steps ran, in call order.
struct FakeHandles {
    log: Vec<&'static str>,
    outstanding: bool,
}

impl FakeHandles {
    fn new(outstanding: bool) -> Self {
        Self {
            log: vec![],
            outstanding,
        }
    }
}

impl ScmHandles for FakeHandles {
    fn close_handles(&mut self) {
        self.log.push("close");
    }
    fn registration_outstanding(&self) -> bool {
        self.outstanding
    }
    fn drain_queued_apc(&mut self) {
        self.log.push("drain");
    }
}

#[skuld::test]
fn the_handles_close_before_a_queued_apc_is_drained() {
    // Draining first leaves a window in which the SCM can queue a fresh
    // notification against boxes that are about to be freed. Reversing the
    // two lines in `close_then_drain` fails here.
    let mut fake = FakeHandles::new(true);
    close_then_drain(&mut fake);
    assert_eq!(fake.log, vec!["close", "drain"]);
}

#[skuld::test]
fn nothing_is_drained_when_no_registration_is_outstanding() {
    // No arm succeeded, so no APC can be queued and the alertable wait would
    // only be able to run some *other* registration's callback.
    let mut fake = FakeHandles::new(false);
    close_then_drain(&mut fake);
    assert_eq!(fake.log, vec!["close"]);
}

// the already-running (1056) decision ---------------------------------------------------------------------------------

#[skuld::test]
fn a_queried_running_recovers_an_already_running_start() {
    // The service is up and the wait, armed for SERVICE_NOTIFY_RUNNING,
    // immediate-fires on it.
    assert!(already_running_is_recoverable(QueriedState::Running));
}

#[skuld::test]
fn a_queried_start_pending_recovers_an_already_running_start() {
    // `want_to_mask` arms SERVICE_NOTIFY_START_PENDING in BOTH of its
    // `WantState::Running` branches, so a service something else started a
    // moment ago immediate-fires and resolves through the wait. Rejecting it
    // failed a start that was going to succeed — narrowing this predicate
    // back to `Running`-only fails here.
    assert!(already_running_is_recoverable(QueriedState::StartPending));
}

#[skuld::test]
fn no_other_queried_state_recovers_an_already_running_start() {
    // None of these is a state the RUNNING wait is armed for, so treating
    // 1056 as "the wait will complete" would block until the deadline
    // instead of reporting what the service is actually doing.
    for state in [
        QueriedState::Stopped,
        QueriedState::StopPending,
        QueriedState::ContinuePending,
        QueriedState::PausePending,
        QueriedState::Paused,
    ] {
        assert!(
            !already_running_is_recoverable(state),
            "{state:?} must not be recoverable"
        );
    }
}

// the lagging-registration (1294) retry loop --------------------------------------------------------------------------

/// Replays `NotifyServiceStatusChangeW` return codes for
/// [`register_until_deadline`], recording each step in call order.
struct FakeRegistrar {
    log: Vec<&'static str>,
    /// Codes for the first `register` calls, front to back.
    codes: VecDeque<u32>,
    /// What every `register` past `codes` returns. Lagging forever is how a
    /// test scripts an SCM that never stops answering 1294.
    forever: u32,
}

impl FakeRegistrar {
    fn new<const N: usize>(codes: [u32; N], forever: u32) -> Self {
        Self {
            log: vec![],
            codes: codes.into(),
            forever,
        }
    }
}

impl NotifyRegistrar for FakeRegistrar {
    fn register(&mut self) -> std::io::Result<u32> {
        self.log.push("register");
        Ok(self.codes.pop_front().unwrap_or(self.forever))
    }
    fn reopen(&mut self) -> std::io::Result<()> {
        self.log.push("reopen");
        Ok(())
    }
}

#[skuld::test]
fn a_registrar_that_keeps_lagging_returns_expired_instead_of_looping() {
    // The 1294 remedy re-enters the loop, so the deadline is the only thing
    // that can end it — there is deliberately no retry counter. Remove the
    // `deadline.expired()` check and this test does not fail, it HANGS, so
    // prove it bites under
    // `python3 .github/scripts/run-with-timeout.py 120 cargo test <name>`
    // and expect exit 124.
    let mut fake = FakeRegistrar::new([], ERROR_SERVICE_NOTIFY_CLIENT_LAGGING);
    assert_eq!(register_until_deadline(&mut fake, spent()).unwrap(), Waited::Expired);
    // The check sits at the TOP of the loop, so a deadline that is already
    // spent does not even attempt a registration.
    assert!(fake.log.is_empty());
}

#[skuld::test]
fn a_lagging_registration_reopens_the_handles_and_arms_again() {
    // The documented 1294 remedy, and the only path that re-enters the loop.
    let mut fake = FakeRegistrar::new([ERROR_SERVICE_NOTIFY_CLIENT_LAGGING], 0);
    assert_eq!(
        register_until_deadline(&mut fake, unbounded()).unwrap(),
        Waited::Confirmed
    );
    assert_eq!(fake.log, vec!["register", "reopen", "register"]);
}

#[skuld::test]
fn a_registration_error_that_is_not_lagging_is_reported_not_retried() {
    // Anything other than 0 or 1294 is the SCM's own failure: report it
    // rather than reopening handles in a loop that cannot help.
    let mut fake = FakeRegistrar::new([], 5); // ERROR_ACCESS_DENIED
    let err = register_until_deadline(&mut fake, unbounded()).unwrap_err();
    assert_eq!(err.raw_os_error(), Some(5));
    assert_eq!(fake.log, vec!["register"]);
}
