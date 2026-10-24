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
    /// What `start` answers when it does not fail.
    start_reply: StartReply,
    /// The deadline each `wait_callback` was handed, in call order.
    deadlines: Vec<Deadline>,
    /// The deadline each `arm` was handed, in call order.
    arm_deadlines: Vec<Deadline>,
    /// What SCM recorded for a `Stopped` the script delivers.
    recorded: RecordedExit,
    /// Records for the first delivered `Stopped`s, front to back; `recorded` once it runs dry.
    stopped_records: VecDeque<RecordedExit>,
    /// The failure-action configuration `recovery` reads; `None` makes the read fail.
    recovery: Option<Recovery>,
    /// What the next `query_status` calls answer, front to back; the last delivered record, stopped,
    /// once it runs dry.
    queries: VecDeque<(QueriedState, RecordedExit)>,
    /// What `query_status` answers before any `Stopped` was delivered: a stop requested without a
    /// wait finds the service in this state.
    queried_at_request: (QueriedState, RecordedExit),
    last_stopped: Option<RecordedExit>,
    recovery_reads: usize,
}

impl FakeScm {
    fn new<const N: usize>(waits: [Option<Observed>; N]) -> Self {
        Self {
            log: vec![],
            waits: waits.into(),
            what_arm_reports: Waited::Confirmed,
            start_fails: false,
            start_reply: StartReply::Accepted,
            deadlines: vec![],
            arm_deadlines: vec![],
            recorded: shim_failed(),
            stopped_records: VecDeque::new(),
            recovery: Some(Recovery::default()),
            queries: VecDeque::new(),
            queried_at_request: (QueriedState::Running, RecordedExit { win32: 0, specific: 0 }),
            last_stopped: None,
            recovery_reads: 0,
        }
    }

    /// The first delivered `Stopped`s carry these records, in order.
    fn stopped_with<const N: usize>(mut self, records: [RecordedExit; N]) -> Self {
        self.stopped_records = records.into();
        self
    }

    /// The service restarts on a failure, as goetia configures `restart: on-failure`.
    fn restarting(mut self) -> Self {
        self.recovery = Some(RESTARTS);
        self
    }

    /// Reading the failure actions fails.
    fn unreadable_recovery(mut self) -> Self {
        self.recovery = None;
        self
    }

    /// `query_status` answers these, front to back.
    fn queried<const N: usize>(mut self, answers: [(QueriedState, RecordedExit); N]) -> Self {
        self.queries = answers.into();
        self
    }

    /// A stop requested with no wait finds the service in `state` with `exit` recorded.
    fn found(mut self, state: QueriedState, exit: RecordedExit) -> Self {
        self.queried_at_request = (state, exit);
        self
    }

    /// A delivered `Stopped` carries the record `reported` leaves in SCM.
    fn stopped_after(mut self, reported: Reported) -> Self {
        let (win32, specific) = reported.scm_codes();
        self.recorded = RecordedExit { win32, specific };
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
            WantState::RestartBegins => "arm_restart_begins",
            WantState::RestartResolves => "arm_restart_resolves",
        });
        Ok(self.what_arm_reports)
    }
    fn control_stop(&mut self) -> std::io::Result<()> {
        self.log.push("control_stop");
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
        if observed == Some(Observed::Stopped) {
            self.last_stopped = Some(self.stopped_records.pop_front().unwrap_or(self.recorded));
        }
        self.log.push(match observed {
            Some(Observed::Stopped) => "got_stopped",
            Some(Observed::Running) => "got_running",
            Some(Observed::Pending) => "got_pending",
            None => "got_nothing",
        });
        Ok(observed)
    }
    fn stopped_exit(&self) -> RecordedExit {
        self.last_stopped.unwrap_or(self.recorded)
    }
    fn query_status(&mut self) -> std::io::Result<(QueriedState, RecordedExit)> {
        self.log.push("query");
        Ok(self.queries.pop_front().unwrap_or(match self.last_stopped {
            // Unchanged since the last `Stopped` the script delivered.
            Some(exit) => (QueriedState::Stopped, exit),
            None => self.queried_at_request,
        }))
    }
    fn recovery(&mut self) -> std::io::Result<Recovery> {
        self.recovery_reads += 1;
        self.recovery
            .ok_or_else(|| std::io::Error::other("scripted recovery read failure"))
    }
}

/// `restart: on-failure` as goetia writes it: a restart action, and the non-crash flag.
const RESTARTS: Recovery = Recovery {
    restarts: true,
    on_non_crash_failures: true,
};

/// A daemon that exited 7: what SCM records and restarts on.
fn daemon_failed() -> RecordedExit {
    let (win32, specific) = Reported::from_daemon_code(7).scm_codes();
    RecordedExit { win32, specific }
}

/// A commanded stop, or a clean exit: nothing follows it.
fn stopped_clean() -> RecordedExit {
    RecordedExit { win32: 0, specific: 0 }
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
    assert_eq!(stop_via_notify(&mut fake, unbounded()).unwrap(), StopWaited::Confirmed);
    assert_eq!(fake.log, vec!["arm_stopped", "control_stop", "wait", "got_stopped"]);
}

#[skuld::test]
fn stop_re_arms_after_a_non_terminal_callback() {
    // A pending (intermediate) callback fires while waiting for STOPPED, then
    // STOPPED; the non-terminal callback must trigger a re-arm.
    let mut fake = FakeScm::new([Some(Observed::Pending), Some(Observed::Stopped)]);
    assert_eq!(stop_via_notify(&mut fake, unbounded()).unwrap(), StopWaited::Confirmed);
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
    for reported in [
        Reported::from_daemon_code(7),
        Reported::Clean,
        Reported::incomplete_stop(),
    ] {
        let mut fake = FakeScm::new([Some(Observed::Stopped)]).stopped_after(reported);
        assert_eq!(
            start_via_notify(&mut fake, unbounded(), Kind::Simple).unwrap(),
            Waited::Confirmed,
            "{reported:?}"
        );
    }
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
    assert_eq!(stop_via_notify(&mut fake, spent()).unwrap(), StopWaited::Expired);
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
    assert_eq!(stop_via_notify(&mut fake, deadline).unwrap(), StopWaited::Confirmed);
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
    assert_eq!(request_stop(&mut fake).unwrap(), RequestedStop::Issued);
    assert_eq!(fake.log, vec!["query", "control_stop"]);
}

// a restart SCM has queued --------------------------------------------------------------------------------------------

/// A service that stopped on a failure while recovery actions are configured has a restart queued
/// that nothing cancels, so the stop waits for it, stops what it starts, and ends on a `STOPPED`
/// that queues nothing.
#[skuld::test]
fn a_stop_of_a_service_awaiting_its_restart_waits_for_the_restart_and_stops_what_it_starts() {
    let deadline = an_hour_out();
    let mut fake = FakeScm::new([
        Some(Observed::Stopped),
        Some(Observed::Running),
        Some(Observed::Stopped),
    ])
    .restarting()
    .stopped_with([daemon_failed(), stopped_clean()]);
    assert_eq!(stop_via_notify(&mut fake, deadline).unwrap(), StopWaited::Confirmed);
    assert_eq!(
        fake.log,
        vec![
            "arm_stopped",
            "control_stop",
            "wait",
            "got_stopped",
            // The restart is queued: wait for it to begin.
            "arm_restart_begins",
            "query",
            "wait",
            "got_running",
            // The restarted daemon is running: an ordinary stop, armed before it is issued.
            "arm_stopped",
            "control_stop",
            "wait",
            "got_stopped",
        ]
    );
    assert_eq!(fake.arm_deadlines, vec![deadline; 3]);
    assert_eq!(fake.deadlines, vec![deadline; 3]);
}

#[skuld::test]
fn a_restart_passing_through_start_pending_is_watched_to_its_outcome() {
    let mut fake = FakeScm::new([
        Some(Observed::Stopped),
        Some(Observed::Pending),
        Some(Observed::Running),
        Some(Observed::Stopped),
    ])
    .restarting()
    .stopped_with([daemon_failed(), stopped_clean()]);
    assert_eq!(stop_via_notify(&mut fake, unbounded()).unwrap(), StopWaited::Confirmed);
    assert_eq!(
        fake.log,
        vec![
            "arm_stopped",
            "control_stop",
            "wait",
            "got_stopped",
            "arm_restart_begins",
            "query",
            "wait",
            "got_pending",
            "arm_restart_resolves",
            "wait",
            "got_running",
            "arm_stopped",
            "control_stop",
            "wait",
            "got_stopped",
        ]
    );
}

/// SCM's own attempt to start the service failed (P10): `START_PENDING`, then `STOPPED` with Win32
/// 0, and nothing follows. Waiting on for a `RUNNING` would only run out the budget.
#[skuld::test]
fn a_restart_attempt_scm_could_not_make_leaves_nothing_pending() {
    let mut fake = FakeScm::new([
        Some(Observed::Stopped),
        Some(Observed::Pending),
        Some(Observed::Stopped),
    ])
    .restarting()
    .stopped_with([daemon_failed(), stopped_clean()]);
    assert_eq!(stop_via_notify(&mut fake, unbounded()).unwrap(), StopWaited::Confirmed);
    assert_eq!(
        fake.log,
        vec![
            "arm_stopped",
            "control_stop",
            "wait",
            "got_stopped",
            "arm_restart_begins",
            "query",
            "wait",
            "got_pending",
            "arm_restart_resolves",
            "wait",
            "got_stopped",
        ],
        "no stop is issued: there is nothing running"
    );
}

/// A restarted process that fails before it is running is an ordinary failure (P9), and the action
/// repeats.
#[skuld::test]
fn a_restarted_daemon_that_fails_again_keeps_the_wait_going() {
    let shim_failed = RecordedExit { win32: 5, specific: 0 };
    let mut fake = FakeScm::new([
        Some(Observed::Stopped),
        Some(Observed::Pending),
        Some(Observed::Stopped),
        Some(Observed::Running),
        Some(Observed::Stopped),
    ])
    .restarting()
    .stopped_with([daemon_failed(), shim_failed, stopped_clean()]);
    assert_eq!(stop_via_notify(&mut fake, unbounded()).unwrap(), StopWaited::Confirmed);
    assert_eq!(
        fake.log,
        vec![
            "arm_stopped",
            "control_stop",
            "wait",
            "got_stopped",
            "arm_restart_begins",
            "query",
            "wait",
            "got_pending",
            "arm_restart_resolves",
            "wait",
            "got_stopped",
            // Another restart is queued.
            "arm_restart_begins",
            "query",
            "wait",
            "got_running",
            "arm_stopped",
            "control_stop",
            "wait",
            "got_stopped",
        ]
    );
}

/// The attempt can be made, and fail, between the `STOPPED` that queued the restart and the arm
/// that watches for it: the arm is on `START_PENDING` and `RUNNING`, so it would never hear of it.
/// The status read right after the arm is what catches it.
#[skuld::test]
fn a_restart_attempt_that_failed_before_the_arm_is_not_waited_for() {
    let mut fake = FakeScm::new([Some(Observed::Stopped)])
        .restarting()
        .stopped_with([daemon_failed()])
        .queried([(QueriedState::Stopped, stopped_clean())]);
    assert_eq!(stop_via_notify(&mut fake, unbounded()).unwrap(), StopWaited::Confirmed);
    assert_eq!(
        fake.log,
        vec![
            "arm_stopped",
            "control_stop",
            "wait",
            "got_stopped",
            "arm_restart_begins",
            "query"
        ]
    );
}

/// A stop that arrives as the daemon fails: the `STOPPED` the stop waits for is the failure's, and
/// SCM queues its restart. The stop then meets the pending state.
#[skuld::test]
fn a_stop_that_meets_a_failing_daemon_follows_the_restart_that_failure_queues() {
    let mut fake = FakeScm::new([
        Some(Observed::Pending),
        Some(Observed::Stopped),
        Some(Observed::Running),
        Some(Observed::Stopped),
    ])
    .restarting()
    .stopped_with([daemon_failed(), stopped_clean()]);
    assert_eq!(stop_via_notify(&mut fake, unbounded()).unwrap(), StopWaited::Confirmed);
    assert_eq!(fake.log.iter().filter(|step| **step == "control_stop").count(), 2);
}

#[skuld::test]
fn a_wait_for_a_restart_that_runs_out_reports_the_restart_pending() {
    for waits in [
        vec![Some(Observed::Stopped), None],
        vec![Some(Observed::Stopped), Some(Observed::Pending), None],
    ] {
        let mut fake = FakeScm::new([]).restarting().stopped_with([daemon_failed()]);
        fake.waits = waits.into();
        assert_eq!(stop_via_notify(&mut fake, spent()).unwrap(), StopWaited::RestartPending);
        assert_eq!(
            fake.log.iter().filter(|step| **step == "control_stop").count(),
            1,
            "nothing is running to stop while the restart is pending"
        );
    }
}

#[skuld::test]
fn a_failure_with_no_recovery_actions_queues_nothing() {
    let mut fake = FakeScm::new([Some(Observed::Stopped)]).stopped_with([daemon_failed()]);
    assert_eq!(stop_via_notify(&mut fake, unbounded()).unwrap(), StopWaited::Confirmed);
    assert_eq!(fake.log, vec!["arm_stopped", "control_stop", "wait", "got_stopped"]);
}

/// A clean stop is the common case and needs no read of the configuration.
#[skuld::test]
fn a_clean_stop_reads_no_recovery_configuration() {
    let mut fake = FakeScm::new([Some(Observed::Stopped)])
        .unreadable_recovery()
        .stopped_with([stopped_clean()]);
    assert_eq!(stop_via_notify(&mut fake, unbounded()).unwrap(), StopWaited::Confirmed);
}

#[skuld::test]
fn a_recovery_configuration_that_cannot_be_read_fails_the_stop() {
    let mut fake = FakeScm::new([Some(Observed::Stopped)])
        .unreadable_recovery()
        .stopped_with([daemon_failed()]);
    assert!(stop_via_notify(&mut fake, unbounded()).is_err());
}

#[skuld::test]
fn only_a_non_zero_win32_code_with_a_restart_action_queues_a_restart() {
    let exit = |win32| RecordedExit { win32, specific: 7 };
    let crashes_only = Recovery {
        restarts: true,
        on_non_crash_failures: false,
    };
    let no_restart = Recovery {
        restarts: false,
        on_non_crash_failures: true,
    };

    // Win32 0 is never restarted, whatever the service-specific code (P7b).
    assert!(!restart_queued(exit(0), RESTARTS));
    assert!(restart_queued(exit(1066), RESTARTS));
    assert!(restart_queued(exit(31), RESTARTS));
    assert!(!restart_queued(exit(1066), no_restart));
    // Without the non-crash flag only a process that ended without reporting (1067) is restarted.
    assert!(!restart_queued(exit(1066), crashes_only));
    assert!(!restart_queued(exit(31), crashes_only));
    assert!(restart_queued(exit(1067), crashes_only));
}

/// `--timeout 0` waits for nothing, so it cannot stop what the restart will start. It says so
/// instead of issuing a stop that SCM answers with a success the pending restart then undoes.
#[skuld::test]
fn a_stop_request_over_a_pending_restart_issues_nothing() {
    let mut fake = FakeScm::new([])
        .restarting()
        .found(QueriedState::Stopped, daemon_failed());
    assert_eq!(request_stop(&mut fake).unwrap(), RequestedStop::RestartPending);
    assert_eq!(fake.log, vec!["query"]);
}

#[skuld::test]
fn a_stop_request_is_issued_unless_a_restart_is_pending() {
    for (recovering, state, exit) in [
        (true, QueriedState::Stopped, stopped_clean()),
        (false, QueriedState::Stopped, daemon_failed()),
        (true, QueriedState::Running, daemon_failed()),
        (true, QueriedState::StartPending, daemon_failed()),
    ] {
        let mut fake = FakeScm::new([]).found(state, exit);
        if recovering {
            fake = fake.restarting();
        }
        assert_eq!(
            request_stop(&mut fake).unwrap(),
            RequestedStop::Issued,
            "{state:?} {exit:?}"
        );
        assert_eq!(fake.log, vec!["query", "control_stop"], "{state:?} {exit:?}");
    }
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
