//! Pure orchestration tests for [`super::stop_via_notify`]/
//! [`super::start_via_notify`] against a scripted [`super::ScmActor`] — no
//! real SCM involved, so these run on every platform and need no elevation.
//! Expiry is driven by the script, never by the clock: nothing here waits.

use std::collections::VecDeque;

use super::*;
use crate::backend::scm::exit_status::{RecordedExit, Reported};
use crate::manager::budget::{Budget, Deadline};
use crate::spec::Kind;

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
    stop_fails: bool,
    /// What `start` answers when it does not fail.
    start_reply: StartReply,
    /// The deadline each `wait_callback` was handed, in call order.
    deadlines: Vec<Deadline>,
    /// The deadline each `arm` was handed, in call order.
    arm_deadlines: Vec<Deadline>,
    /// What SCM recorded for a `Stopped` the script delivers.
    recorded: RecordedExit,
    /// The `dwWaitHint` of a `Stopped` the script delivers.
    wait_hint: u32,
}

impl FakeScm {
    fn new<const N: usize>(waits: [Option<Observed>; N]) -> Self {
        Self {
            log: vec![],
            waits: waits.into(),
            what_arm_reports: Waited::Confirmed,
            start_fails: false,
            stop_fails: false,
            start_reply: StartReply::Accepted,
            deadlines: vec![],
            arm_deadlines: vec![],
            recorded: shim_failed(),
            wait_hint: 0,
        }
    }

    /// A delivered `Stopped` carries the record `reported` leaves in SCM.
    fn stopped_after(mut self, reported: Reported) -> Self {
        let (win32, specific) = reported.scm_codes();
        self.recorded = RecordedExit { win32, specific };
        self
    }

    /// A delivered `Stopped` is SCM's own: it records `(0, 0)` and leaves `wait_hint` behind.
    fn stopped_by_scm_with_wait_hint(mut self, wait_hint: u32) -> Self {
        self.recorded = RecordedExit { win32: 0, specific: 0 };
        self.wait_hint = wait_hint;
        self
    }

    /// Every `arm` reports `reports` rather than `Waited::Confirmed`.
    fn arming(mut self, reports: Waited) -> Self {
        self.what_arm_reports = reports;
        self
    }

    /// `start` reports failure.
    fn rejecting_start(mut self) -> Self {
        self.start_fails = true;
        self
    }

    /// `control_stop` reports failure.
    fn rejecting_stop(mut self) -> Self {
        self.stop_fails = true;
        self
    }

    /// `start` answers `ERROR_SERVICE_ALREADY_RUNNING`, with `queried` as the
    /// follow-up query's state.
    fn already_running(mut self, queried: QueriedState) -> Self {
        self.start_reply = StartReply::AlreadyRunning(queried);
        self
    }

    fn arm_count(&self) -> usize {
        self.log.iter().filter(|step| step.starts_with("arm_")).count()
    }
}

impl ScmActor for FakeScm {
    fn arm(&mut self, want: WantState, deadline: Deadline) -> std::io::Result<Waited> {
        self.arm_deadlines.push(deadline);
        self.log.push(match want {
            WantState::Stopped => "arm_stopped",
            WantState::Running => "arm_running",
            WantState::RunningOrStopped => "arm_running_or_stopped",
        });
        Ok(self.what_arm_reports)
    }
    fn control_stop(&mut self) -> std::io::Result<()> {
        self.log.push("control_stop");
        if self.stop_fails {
            return Err(std::io::Error::other("scripted stop failure"));
        }
        Ok(())
    }
    fn start(&mut self) -> std::io::Result<StartReply> {
        self.log.push("start");
        if self.start_fails {
            return Err(std::io::Error::other("scripted start failure"));
        }
        Ok(self.start_reply)
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
    fn stopped_exit(&self) -> RecordedExit {
        self.recorded
    }
    fn stopped_wait_hint(&self) -> u32 {
        self.wait_hint
    }
}

/// The record of a service whose shim failed before launching the daemon.
fn shim_failed() -> RecordedExit {
    let (win32, specific) = Reported::shim_failure(None).scm_codes();
    RecordedExit { win32, specific }
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

// settle_to_stopped ----

#[skuld::test]
fn settle_a_running_service_is_stopped_and_its_stop_awaited() {
    let mut fake = FakeScm::new([Some(Observed::Running), Some(Observed::Stopped)]);
    assert_eq!(settle_to_stopped(&mut fake, unbounded()).unwrap(), Waited::Confirmed);
    assert_eq!(
        fake.log,
        vec![
            "arm_running_or_stopped",
            "wait",
            "got_running",
            "arm_stopped",
            "control_stop",
            "wait",
            "got_stopped",
        ]
    );
}

/// STOP sent in `START_PENDING` is rejected with 1052 and has no later effect, so nothing is sent
/// until the service has reached `RUNNING`.
#[skuld::test]
fn settle_a_start_pending_service_is_waited_out_before_any_stop() {
    let mut fake = FakeScm::new([
        Some(Observed::Pending),
        Some(Observed::Running),
        Some(Observed::Stopped),
    ]);
    assert_eq!(settle_to_stopped(&mut fake, unbounded()).unwrap(), Waited::Confirmed);
    assert_eq!(
        fake.log,
        vec![
            "arm_running_or_stopped",
            "wait",
            "got_pending",
            "arm_running_or_stopped",
            "wait",
            "got_running",
            "arm_stopped",
            "control_stop",
            "wait",
            "got_stopped",
        ]
    );
}

/// A service that is stopping (or starting and then ending) is waited out, never sent a STOP.
#[skuld::test]
fn settle_a_pending_service_that_stops_is_never_sent_a_stop() {
    let mut fake = FakeScm::new([Some(Observed::Pending), Some(Observed::Stopped)]);
    assert_eq!(settle_to_stopped(&mut fake, unbounded()).unwrap(), Waited::Confirmed);
    assert!(!fake.log.contains(&"control_stop"), "{:?}", fake.log);
}

#[skuld::test]
fn settle_a_stopped_service_is_left_alone() {
    let mut fake = FakeScm::new([Some(Observed::Stopped)]);
    assert_eq!(settle_to_stopped(&mut fake, unbounded()).unwrap(), Waited::Confirmed);
    assert_eq!(fake.log, vec!["arm_running_or_stopped", "wait", "got_stopped"]);
}

/// The daemon can end between `RUNNING` and the stop, where `control_stop` answers
/// `ERROR_SERVICE_NOT_ACTIVE` (absorbed by the actor as `Ok`). The `STOPPED` it left must still reach
/// the wait, which holds only if the stop wait is armed before the stop is sent.
#[skuld::test]
fn settle_a_service_that_ends_before_the_stop_lands_still_confirms() {
    let mut fake = FakeScm::new([Some(Observed::Running), Some(Observed::Stopped)]);
    assert_eq!(settle_to_stopped(&mut fake, unbounded()).unwrap(), Waited::Confirmed);
    let arm = fake.log.iter().position(|s| *s == "arm_stopped").unwrap();
    let stop = fake.log.iter().position(|s| *s == "control_stop").unwrap();
    assert!(arm < stop, "{:?}", fake.log);
}

#[skuld::test]
fn settle_a_failed_stop_is_an_error() {
    let mut fake = FakeScm::new([Some(Observed::Running)]).rejecting_stop();
    assert!(settle_to_stopped(&mut fake, unbounded()).is_err());
}

#[skuld::test]
fn start_via_notify_arms_running_before_start_then_gates_on_running() {
    let mut fake = FakeScm::new([Some(Observed::Running)]);
    assert_eq!(
        start_via_notify(&mut fake, unbounded(), Kind::Simple).unwrap(),
        Waited::Confirmed
    );
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
    assert_eq!(
        start_via_notify(&mut fake, unbounded(), Kind::Simple).unwrap(),
        Waited::Confirmed
    );
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
    assert!(start_via_notify(&mut fake, unbounded(), Kind::Simple).is_err());
}

#[skuld::test]
fn a_daemon_that_exited_right_after_launch_is_a_started_service() {
    // `start` confirms the shim launched the daemon, as `systemctl start` confirms the exec: what
    // the daemon does next is not the start's to judge, and racing it would make the answer vary.
    for reported in [Reported::from_daemon_code(7), Reported::Clean] {
        let mut fake = FakeScm::new([Some(Observed::Stopped)]).stopped_after(reported);
        assert_eq!(
            start_via_notify(&mut fake, unbounded(), Kind::Simple).unwrap(),
            Waited::Confirmed,
            "{reported:?}"
        );
    }
}

/// The shim's `(0, 1003)` says it could not show the daemon's tree gone: not a launch to confirm,
/// and the daemon may still be running. A `Stopped` notification can follow a `RUNNING` the wait
/// never saw, so the message claims neither when the record was made nor why.
#[skuld::test]
fn an_incomplete_stop_record_is_a_failed_start() {
    for joined in [None, Some(QueriedState::StartPending)] {
        let mut fake = FakeScm::new([Some(Observed::Stopped)]).stopped_after(Reported::incomplete_stop());
        if let Some(queried) = joined {
            fake = fake.already_running(queried);
        }

        let err = start_via_notify(&mut fake, unbounded(), Kind::Simple).unwrap_err();

        let message = err.to_string();
        assert!(message.contains("failed start"), "{joined:?}: {message}");
        assert!(message.contains("may still be running"), "{joined:?}: {message}");
        assert!(!message.contains("before reaching Running"), "{joined:?}: {message}");
        assert!(!message.contains("contained"), "{joined:?}: {message}");
    }
}

/// A daemon that exits with code 1003 is `(1066, 1003)`, not the incomplete-stop record.
#[skuld::test]
fn a_daemon_exiting_1003_is_still_a_started_service() {
    let mut fake = FakeScm::new([Some(Observed::Stopped)]).stopped_after(Reported::from_daemon_code(1003));

    assert_eq!(
        start_via_notify(&mut fake, unbounded(), Kind::Simple).unwrap(),
        Waited::Confirmed
    );
}

/// A `start` that lands as SCM's queued recovery start fires gets 1056 over `START_PENDING`. If
/// that attempt fails (missing ImagePath, broken account) SCM reports `STOPPED (0, 0)` with a
/// wait hint of 2000, and no shim ever ran.
#[skuld::test]
fn a_failed_scm_recovery_start_that_a_start_joined_is_a_failed_start() {
    let mut fake = FakeScm::new([Some(Observed::Stopped)])
        .already_running(QueriedState::StartPending)
        .stopped_by_scm_with_wait_hint(2000);

    let err = start_via_notify(&mut fake, unbounded(), Kind::Simple).unwrap_err();

    assert!(err.to_string().contains("failed start"), "{err}");
}

/// A start SCM accepted is ours, so `(0, 0)` with a wait hint is not SCM's failed attempt.
#[skuld::test]
fn a_stopped_with_a_wait_hint_after_an_accepted_start_is_a_started_service() {
    let mut fake = FakeScm::new([Some(Observed::Stopped)]).stopped_by_scm_with_wait_hint(2000);

    assert_eq!(
        start_via_notify(&mut fake, unbounded(), Kind::Simple).unwrap(),
        Waited::Confirmed
    );
}

/// The shim's own report of a daemon's exit is not `(0, 0)`, so a leftover wait hint does not make
/// it a failed start.
#[skuld::test]
fn a_daemon_exit_record_with_a_wait_hint_that_a_start_joined_is_a_started_service() {
    let mut fake = FakeScm::new([Some(Observed::Stopped)])
        .already_running(QueriedState::StartPending)
        .stopped_after(Reported::from_daemon_code(7));
    fake.wait_hint = 2000;

    assert_eq!(
        start_via_notify(&mut fake, unbounded(), Kind::Simple).unwrap(),
        Waited::Confirmed
    );
}

/// `(0, 0)` with no wait hint is the shim's clean report: the daemon ran and ended.
#[skuld::test]
fn a_clean_daemon_exit_that_a_start_joined_is_still_a_started_service() {
    let mut fake = FakeScm::new([Some(Observed::Stopped)])
        .already_running(QueriedState::StartPending)
        .stopped_after(Reported::Clean);

    assert_eq!(
        start_via_notify(&mut fake, unbounded(), Kind::Simple).unwrap(),
        Waited::Confirmed
    );
}

#[skuld::test]
fn a_shim_failure_before_launch_is_a_failed_start() {
    for reported in [Reported::shim_failure(None), Reported::shim_failure(Some(2))] {
        let mut fake = FakeScm::new([Some(Observed::Stopped)]).stopped_after(reported);
        let err = start_via_notify(&mut fake, unbounded(), Kind::Simple).unwrap_err();
        assert!(
            err.to_string().contains("failed start"),
            "{reported:?} gave an error that does not say so: {err}"
        );
    }
}

#[skuld::test]
fn a_managed_service_that_stops_before_running_is_a_failed_start_whatever_it_recorded() {
    // A managed service speaks SCM itself: STOPPED with a service-specific code before RUNNING is
    // how it reports that it could not start. The shim's reading of a record (a daemon exit means
    // the daemon was launched) does not apply to it.
    for reported in [
        Reported::from_daemon_code(7),
        Reported::Clean,
        Reported::incomplete_stop(),
        Reported::shim_failure(Some(2)),
    ] {
        let mut fake = FakeScm::new([Some(Observed::Stopped)]).stopped_after(reported);
        let err = start_via_notify(&mut fake, unbounded(), Kind::Managed).unwrap_err();
        assert!(
            err.to_string().contains("failed start"),
            "{reported:?} gave an error that does not say so: {err}"
        );
    }
}

#[skuld::test]
fn a_managed_service_that_reaches_running_is_a_started_service() {
    let mut fake = FakeScm::new([Some(Observed::Running)]);
    assert_eq!(
        start_via_notify(&mut fake, unbounded(), Kind::Managed).unwrap(),
        Waited::Confirmed
    );
}

#[skuld::test]
fn a_failed_start_names_the_recorded_code() {
    let mut fake = FakeScm::new([Some(Observed::Stopped)]).stopped_after(Reported::shim_failure(Some(2)));
    let err = start_via_notify(&mut fake, unbounded(), Kind::Simple).unwrap_err();
    assert!(err.to_string().contains("exit code 2"), "{err}");
}

#[skuld::test]
fn start_via_notify_rearms_on_pending_not_errs() {
    // A StartPending/StopPending intermediate (Pending) must RE-ARM, not fail.
    let mut fake = FakeScm::new([
        Some(Observed::Pending),
        Some(Observed::Pending),
        Some(Observed::Running),
    ]);
    assert_eq!(
        start_via_notify(&mut fake, unbounded(), Kind::Simple).unwrap(),
        Waited::Confirmed
    );
    assert_eq!(fake.arm_count(), 3); // initial + 2 re-arms
}

#[skuld::test]
fn a_start_wait_whose_deadline_expires_reports_expired() {
    let mut fake = FakeScm::new([Some(Observed::Pending), None]);
    assert_eq!(
        start_via_notify(&mut fake, spent(), Kind::Simple).unwrap(),
        Waited::Expired
    );
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
    assert_eq!(
        start_via_notify(&mut fake, spent(), Kind::Simple).unwrap(),
        Waited::Expired
    );
    assert_eq!(fake.log, vec!["arm_running", "start", "wait", "got_nothing"]);
}

/// A deadline no re-derivation inside the wait can reproduce: an hour out,
/// where a fresh `Budget::DEFAULT.start()` (or any other budget the code
/// could reach for) lands somewhere else.
fn an_hour_out() -> Deadline {
    Budget::Bounded(std::time::Duration::from_secs(3600)).start()
}

/// Re-arming must not restart the clock: a service that chatters
/// START_PENDING forever has to terminate at the caller's budget. Every arm
/// — `arm`'s own deadline bounds the lagging-remedy loop — and every wait
/// gets the caller's deadline, not a fresh one.
#[skuld::test]
fn a_pending_callback_rearms_under_the_same_deadline() {
    let deadline = an_hour_out();
    let mut fake = FakeScm::new([
        Some(Observed::Pending),
        Some(Observed::Pending),
        Some(Observed::Running),
    ]);
    assert_eq!(
        start_via_notify(&mut fake, deadline, Kind::Simple).unwrap(),
        Waited::Confirmed
    );
    assert_eq!(fake.arm_count(), 3);
    assert_eq!(fake.arm_deadlines, vec![deadline; 3]);
    assert_eq!(fake.deadlines, vec![deadline; 3]);
}

/// [`a_pending_callback_rearms_under_the_same_deadline`]'s stop mirror.
#[skuld::test]
fn a_pending_stop_callback_rearms_under_the_same_deadline() {
    let deadline = an_hour_out();
    let mut fake = FakeScm::new([
        Some(Observed::Pending),
        Some(Observed::Running),
        Some(Observed::Stopped),
    ]);
    assert_eq!(stop_via_notify(&mut fake, deadline).unwrap(), Waited::Confirmed);
    assert_eq!(fake.arm_count(), 3);
    assert_eq!(fake.arm_deadlines, vec![deadline; 3]);
    assert_eq!(fake.deadlines, vec![deadline; 3]);
}

#[skuld::test]
fn an_arm_that_keeps_lagging_returns_expired_instead_of_looping() {
    // `Waited::Expired` from `arm` is what a bounded arm reports when the
    // SCM answers ERROR_SERVICE_NOTIFY_CLIENT_LAGGING past the deadline. The
    // empty script proves no wait was entered; the log proves the request
    // still went out.
    let mut fake = FakeScm::new([]).arming(Waited::Expired);
    assert_eq!(
        start_via_notify(&mut fake, spent(), Kind::Simple).unwrap(),
        Waited::Expired
    );
    assert_eq!(fake.log, vec!["arm_running", "start"]);
}

#[skuld::test]
fn a_start_the_scm_rejects_is_reported_as_a_failed_start() {
    // A start the SCM rejects is a failed start, not a wait that ran out of
    // budget. See `already_running_is_recoverable` and
    // `start_re_arms_after_a_non_terminal_callback` for the related cases
    // this doesn't duplicate.
    let mut rejecting = FakeScm::new([]).rejecting_start();
    assert!(start_via_notify(&mut rejecting, unbounded(), Kind::Simple).is_err());
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

/// The plain verb is idempotent: a 1056 over a service the query reports
/// `Running` is a start, on both paths.
#[skuld::test]
fn a_plain_start_over_a_running_service_is_a_start() {
    let mut fake = FakeScm::new([]).already_running(QueriedState::Running);
    request_start(&mut fake).unwrap();

    let mut fake = FakeScm::new([Some(Observed::Running)]).already_running(QueriedState::Running);
    assert_eq!(
        start_via_notify(&mut fake, unbounded(), Kind::Simple).unwrap(),
        Waited::Confirmed
    );
}

/// A 1056 over a service the query reports `Running` is the SCM saying the goal is reached, so the
/// wait confirms on it with nothing more to watch — even with the deadline already spent, where the
/// other two backends agree: launchd reads a running job as started under any budget.
#[skuld::test]
fn a_start_the_scm_answers_already_running_is_confirmed_whatever_the_deadline() {
    for (arm, deadline) in [(Waited::Expired, spent()), (Waited::Confirmed, unbounded())] {
        let mut fake = FakeScm::new([]).already_running(QueriedState::Running).arming(arm);
        assert_eq!(
            start_via_notify(&mut fake, deadline, Kind::Simple).unwrap(),
            Waited::Confirmed
        );
        assert_eq!(fake.log, vec!["arm_running", "start"], "nothing left to wait for");
    }
}

/// `StartPending` is on its way, not there: under a spent deadline that is still an expiry.
#[skuld::test]
fn a_start_the_scm_answers_start_pending_still_expires_on_a_spent_deadline() {
    let mut fake = FakeScm::new([])
        .already_running(QueriedState::StartPending)
        .arming(Waited::Expired);
    assert_eq!(
        start_via_notify(&mut fake, spent(), Kind::Simple).unwrap(),
        Waited::Expired
    );
}

#[skuld::test]
fn a_plain_start_over_a_stopping_service_is_refused() {
    let mut fake = FakeScm::new([]).already_running(QueriedState::StopPending);
    assert!(request_start(&mut fake).is_err());
}

/// `restart --timeout 0`'s start, over the service the real `goetia-shim`
/// presents after an unconfirmed stop: `RUNNING` for its whole teardown,
/// never `STOP_PENDING`. That 1056 is a refusal — reading it as a start
/// reports a restart that leaves the daemon down.
#[skuld::test]
fn a_start_after_an_unconfirmed_stop_refuses_a_service_still_reading_running() {
    let mut fake = FakeScm::new([]).already_running(QueriedState::Running);
    assert!(request_start_after_stop(&mut fake).is_err());
    assert_eq!(fake.log, vec!["start"], "it arms nothing and waits for nothing");
}

/// Every 1056, whatever the query says: none of them establishes that the
/// stop just issued has taken effect.
#[skuld::test]
fn a_start_after_an_unconfirmed_stop_refuses_every_already_running_answer() {
    for state in [
        QueriedState::Stopped,
        QueriedState::StartPending,
        QueriedState::StopPending,
        QueriedState::Running,
        QueriedState::ContinuePending,
        QueriedState::PausePending,
        QueriedState::Paused,
    ] {
        let mut fake = FakeScm::new([]).already_running(state);
        assert!(request_start_after_stop(&mut fake).is_err(), "{state:?}");
    }
}

#[skuld::test]
fn a_start_after_an_unconfirmed_stop_that_the_scm_accepts_is_ok() {
    let mut fake = FakeScm::new([]);
    request_start_after_stop(&mut fake).unwrap();
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
