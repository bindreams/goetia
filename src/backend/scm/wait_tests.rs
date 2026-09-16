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
