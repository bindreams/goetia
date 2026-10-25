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
    /// `delete` reports failure.
    delete_fails: bool,
    /// `control_stop` reports failure.
    stop_fails: bool,
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
    /// The process ids the next `query_status` calls report, front to back; 0 once it runs dry.
    pids: VecDeque<u32>,
    /// What `query_status` answers before any `Stopped` was delivered: a stop requested without a
    /// wait finds the service in this state.
    queried_at_request: (QueriedState, RecordedExit),
    last_stopped: Option<RecordedExit>,
    recovery_reads: usize,
    /// What the next `control_stop` calls answer, front to back; `Sent` once it runs dry.
    controls: VecDeque<ControlReply>,
    /// Whether `query_status` reports the service as accepting `STOP`.
    accepts_stop: bool,
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
            delete_fails: false,
            stop_fails: false,
            start_reply: StartReply::Accepted,
            deadlines: vec![],
            arm_deadlines: vec![],
            recorded: shim_failed(),
            stopped_records: VecDeque::new(),
            recovery: Some(Recovery::default()),
            queries: VecDeque::new(),
            pids: VecDeque::new(),
            queried_at_request: (QueriedState::Running, RecordedExit { win32: 0, specific: 0 }),
            last_stopped: None,
            recovery_reads: 0,
            controls: VecDeque::new(),
            accepts_stop: true,
            wait_hint: 0,
        }
    }

    /// The next `control_stop` calls answer these, front to back.
    fn controlled<const N: usize>(mut self, replies: [ControlReply; N]) -> Self {
        self.controls = replies.into();
        self
    }

    /// `query_status` reports a service that does not accept `STOP`.
    fn not_accepting_stop(mut self) -> Self {
        self.accepts_stop = false;
        self
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

    /// The next `query_status` calls report these process ids, front to back.
    fn running_as<const N: usize>(mut self, pids: [u32; N]) -> Self {
        self.pids = pids.into();
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

    /// `delete` reports failure.
    fn rejecting_delete(mut self) -> Self {
        self.delete_fails = true;
        self
    }

    /// `control_stop` reports failure.
    fn rejecting_stop(mut self) -> Self {
        self.stop_fails = true;
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
            WantState::RestartResolves => "arm_restart_resolves",
        });
        Ok(self.what_arm_reports)
    }
    fn control_stop(&mut self) -> std::io::Result<ControlReply> {
        self.log.push("control_stop");
        if self.stop_fails {
            return Err(std::io::Error::other("scripted stop failure"));
        }
        Ok(self.controls.pop_front().unwrap_or(ControlReply::Sent))
    }
    fn reopen_service(&mut self) -> std::io::Result<()> {
        self.log.push("reopen");
        Ok(())
    }
    fn delete(&mut self) -> std::io::Result<()> {
        self.log.push("delete");
        if self.delete_fails {
            return Err(std::io::Error::other("scripted delete failure"));
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
    fn stopped_wait_hint(&self) -> u32 {
        self.wait_hint
    }
    fn query_status(&mut self) -> std::io::Result<QueriedStatus> {
        self.log.push("query");
        let (state, exit) = self.queries.pop_front().unwrap_or(match self.last_stopped {
            // Unchanged since the last `Stopped` the script delivered.
            Some(exit) => (QueriedState::Stopped, exit),
            None => self.queried_at_request,
        });
        Ok(QueriedStatus {
            state,
            exit,
            accepts_stop: self.accepts_stop,
            pid: self.pids.pop_front().unwrap_or(0),
        })
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
    assert_eq!(
        stop_via_notify(&mut fake, unbounded(), Kind::Simple).unwrap(),
        StopWaited::Confirmed
    );
    assert_eq!(
        fake.log,
        vec!["query", "arm_stopped", "control_stop", "wait", "got_stopped"]
    );
}

#[skuld::test]
fn stop_re_arms_after_a_non_terminal_callback() {
    // A pending (intermediate) callback fires while waiting for STOPPED, then
    // STOPPED; the non-terminal callback must trigger a re-arm.
    let mut fake = FakeScm::new([Some(Observed::Pending), Some(Observed::Stopped)]).queried([
        (QueriedState::Running, stopped_clean()),
        (QueriedState::StopPending, stopped_clean()),
    ]);
    assert_eq!(
        stop_via_notify(&mut fake, unbounded(), Kind::Simple).unwrap(),
        StopWaited::Confirmed
    );
    assert_eq!(
        fake.log,
        vec![
            "query",
            "arm_stopped",
            "control_stop",
            "wait",
            "got_pending",
            "arm_stopped", // re-arm: still waiting for STOPPED
            "query",       // and the service is read again
            "wait",
            "got_stopped",
        ]
    );
}

#[skuld::test]
fn start_via_notify_arms_running_before_start_then_gates_on_running() {
    let mut fake = FakeScm::new([Some(Observed::Running)]);
    assert_eq!(
        start_via_notify(&mut fake, unbounded(), Kind::Simple, Kind::Simple).unwrap(),
        Waited::Confirmed
    );
    // The critical ordering: arm RUNNING strictly BEFORE start, else a
    // RUNNING reached before the arm fires only on the NEXT entry — a hang.
    assert_eq!(fake.log, vec!["query", "arm_running", "start", "wait", "got_running"]);
}

#[skuld::test]
fn start_re_arms_after_a_non_terminal_callback() {
    // A StartPending/StopPending intermediate (Pending) fires while waiting
    // for RUNNING, then RUNNING; the non-terminal callback must trigger a
    // re-arm.
    let mut fake = FakeScm::new([Some(Observed::Pending), Some(Observed::Running)]);
    assert_eq!(
        start_via_notify(&mut fake, unbounded(), Kind::Simple, Kind::Simple).unwrap(),
        Waited::Confirmed
    );
    assert_eq!(
        fake.log,
        vec![
            "query",
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
    assert!(start_via_notify(&mut fake, unbounded(), Kind::Simple, Kind::Simple).is_err());
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
            start_via_notify(&mut fake, unbounded(), Kind::Simple, Kind::Simple).unwrap(),
            Waited::Confirmed,
            "{reported:?}"
        );
    }
}

/// A `start` that lands as SCM's queued recovery start fires gets 1056 over `START_PENDING`. If
/// that attempt fails (missing ImagePath, broken account) SCM reports `STOPPED (0, 0)` with a
/// wait hint of 2000, and no shim ever ran.
#[skuld::test]
fn a_failed_scm_recovery_start_that_a_start_joined_is_a_failed_start() {
    let mut fake = FakeScm::new([Some(Observed::Stopped)])
        .already_running(QueriedState::StartPending)
        .stopped_by_scm_with_wait_hint(2000);

    let err = start_via_notify(&mut fake, unbounded(), Kind::Simple, Kind::Simple).unwrap_err();

    assert!(err.to_string().contains("failed start"), "{err}");
}

/// The same `(0, 0)` with no wait hint is the shim's clean report: the daemon ran and ended.
#[skuld::test]
fn a_clean_daemon_exit_that_a_start_joined_is_still_a_started_service() {
    let mut fake = FakeScm::new([Some(Observed::Stopped)])
        .already_running(QueriedState::StartPending)
        .stopped_after(Reported::Clean);

    assert_eq!(
        start_via_notify(&mut fake, unbounded(), Kind::Simple, Kind::Simple).unwrap(),
        Waited::Confirmed
    );
}

#[skuld::test]
fn a_shim_failure_before_launch_is_a_failed_start() {
    for reported in [Reported::shim_failure(None), Reported::shim_failure(Some(2))] {
        let mut fake = FakeScm::new([Some(Observed::Stopped)]).stopped_after(reported);
        let err = start_via_notify(&mut fake, unbounded(), Kind::Simple, Kind::Simple).unwrap_err();
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
        let err = start_via_notify(&mut fake, unbounded(), Kind::Managed, Kind::Managed).unwrap_err();
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
        start_via_notify(&mut fake, unbounded(), Kind::Managed, Kind::Managed).unwrap(),
        Waited::Confirmed
    );
}

#[skuld::test]
fn a_failed_start_names_the_recorded_code() {
    let mut fake = FakeScm::new([Some(Observed::Stopped)]).stopped_after(Reported::shim_failure(Some(2)));
    let err = start_via_notify(&mut fake, unbounded(), Kind::Simple, Kind::Simple).unwrap_err();
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
        start_via_notify(&mut fake, unbounded(), Kind::Simple, Kind::Simple).unwrap(),
        Waited::Confirmed
    );
    assert_eq!(fake.arm_count(), 3); // initial + 2 re-arms
}

#[skuld::test]
fn a_start_wait_whose_deadline_expires_reports_expired() {
    let mut fake = FakeScm::new([Some(Observed::Pending), None]);
    assert_eq!(
        start_via_notify(&mut fake, spent(), Kind::Simple, Kind::Simple).unwrap(),
        Waited::Expired
    );
}

#[skuld::test]
fn a_stop_wait_whose_deadline_expires_reports_expired() {
    let mut fake = FakeScm::new([Some(Observed::Pending), None]);
    assert_eq!(
        stop_via_notify(&mut fake, spent(), Kind::Simple).unwrap(),
        StopWaited::Expired
    );
}

#[skuld::test]
fn an_expired_deadline_still_issues_the_start_request() {
    // Only the *waiting* is bounded. A service that failed to start because
    // goetia ran out of budget while watching for it would be the worst of
    // both answers.
    let mut fake = FakeScm::new([None]);
    assert_eq!(
        start_via_notify(&mut fake, spent(), Kind::Simple, Kind::Simple).unwrap(),
        Waited::Expired
    );
    assert_eq!(fake.log, vec!["query", "arm_running", "start", "wait", "got_nothing"]);
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
        start_via_notify(&mut fake, deadline, Kind::Simple, Kind::Simple).unwrap(),
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
    ])
    .queried([
        (QueriedState::Running, stopped_clean()),
        (QueriedState::StopPending, stopped_clean()),
        (QueriedState::StopPending, stopped_clean()),
    ]);
    assert_eq!(
        stop_via_notify(&mut fake, deadline, Kind::Simple).unwrap(),
        StopWaited::Confirmed
    );
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
        start_via_notify(&mut fake, spent(), Kind::Simple, Kind::Simple).unwrap(),
        Waited::Expired
    );
    assert_eq!(fake.log, vec!["query", "arm_running", "start"]);
}

#[skuld::test]
fn a_start_the_scm_rejects_is_reported_as_a_failed_start() {
    // A start the SCM rejects is a failed start, not a wait that ran out of
    // budget. See `already_running_is_recoverable` and
    // `start_re_arms_after_a_non_terminal_callback` for the related cases
    // this doesn't duplicate.
    let mut rejecting = FakeScm::new([]).rejecting_start();
    assert!(start_via_notify(&mut rejecting, unbounded(), Kind::Simple, Kind::Simple).is_err());
}

#[skuld::test]
fn request_start_arms_nothing_and_waits_for_nothing() {
    let mut fake = FakeScm::new([]);
    request_start(&mut fake, Kind::Simple).unwrap();
    assert_eq!(fake.log, vec!["query", "start"]);
}

#[skuld::test]
fn request_stop_arms_nothing_and_waits_for_nothing() {
    let mut fake = FakeScm::new([]);
    assert_eq!(request_stop(&mut fake, Kind::Simple).unwrap(), RequestedStop::Issued);
    assert_eq!(fake.log, vec!["query", "control_stop"]);
}

// a restart SCM has queued --------------------------------------------------------------------------------------------

/// A stopped service whose last failure queued a restart is refused at once, whatever the
/// deadline: nothing cancels the restart, so a stop could only wait for it, and no stop waits on one.
#[skuld::test]
fn a_stopped_service_with_a_restart_pending_is_refused_without_a_wait() {
    for deadline in [unbounded(), spent()] {
        let mut fake = FakeScm::new([])
            .restarting()
            .found(QueriedState::Stopped, daemon_failed());
        assert_eq!(
            stop_via_notify(&mut fake, deadline, Kind::Simple).unwrap(),
            StopWaited::RestartPending
        );
        assert_eq!(fake.log, vec!["query"]);
    }
}

#[skuld::test]
fn a_stopped_service_with_nothing_pending_is_stopped() {
    for (restarting, exit) in [(true, stopped_clean()), (false, daemon_failed())] {
        let mut fake = FakeScm::new([]).found(QueriedState::Stopped, exit);
        if restarting {
            fake = fake.restarting();
        }
        assert_eq!(
            stop_via_notify(&mut fake, unbounded(), Kind::Simple).unwrap(),
            StopWaited::Confirmed
        );
        assert_eq!(fake.log, vec!["query"], "{exit:?}");
    }
}

/// A stop that arrives as the daemon fails: the `STOPPED` the stop waits for is the failure's, and
/// SCM queues its restart. The stop reports it and waits for nothing more.
#[skuld::test]
fn a_stop_that_meets_a_failing_daemon_reports_the_restart_pending() {
    let mut fake = FakeScm::new([Some(Observed::Pending), Some(Observed::Stopped)])
        .restarting()
        .queried([
            (QueriedState::Running, stopped_clean()),
            (QueriedState::StopPending, stopped_clean()),
        ])
        .stopped_with([daemon_failed()]);
    assert_eq!(
        stop_via_notify(&mut fake, unbounded(), Kind::Simple).unwrap(),
        StopWaited::RestartPending
    );
    assert_eq!(
        fake.log,
        vec![
            "query",
            "arm_stopped",
            "control_stop",
            "wait",
            "got_pending",
            "arm_stopped",
            "query",
            "wait",
            "got_stopped",
        ]
    );
}

#[skuld::test]
fn a_failure_with_no_recovery_actions_queues_nothing() {
    let mut fake = FakeScm::new([Some(Observed::Stopped)]).stopped_with([daemon_failed()]);
    assert_eq!(
        stop_via_notify(&mut fake, unbounded(), Kind::Simple).unwrap(),
        StopWaited::Confirmed
    );
    assert_eq!(
        fake.log,
        vec!["query", "arm_stopped", "control_stop", "wait", "got_stopped"]
    );
}

/// A clean stop is the common case and needs no read of the configuration.
#[skuld::test]
fn a_clean_stop_reads_no_recovery_configuration() {
    let mut fake = FakeScm::new([Some(Observed::Stopped)])
        .unreadable_recovery()
        .stopped_with([stopped_clean()]);
    assert_eq!(
        stop_via_notify(&mut fake, unbounded(), Kind::Simple).unwrap(),
        StopWaited::Confirmed
    );
}

#[skuld::test]
fn a_recovery_configuration_that_cannot_be_read_fails_the_stop() {
    let mut fake = FakeScm::new([Some(Observed::Stopped)])
        .unreadable_recovery()
        .stopped_with([daemon_failed()]);
    assert!(stop_via_notify(&mut fake, unbounded(), Kind::Simple).is_err());
}

#[skuld::test]
fn only_a_non_zero_win32_code_with_a_restart_action_is_a_pending_restart() {
    let exit = |win32| RecordedExit { win32, specific: 7 };
    let crashes_only = Recovery {
        restarts: true,
        on_non_crash_failures: false,
    };
    let no_restart = Recovery {
        restarts: false,
        on_non_crash_failures: true,
    };
    let pending = |recovery: Recovery, exit: RecordedExit| {
        let mut fake = FakeScm::new([]).found(QueriedState::Stopped, exit);
        fake.recovery = Some(recovery);
        stop_via_notify(&mut fake, unbounded(), Kind::Simple).unwrap() == StopWaited::RestartPending
    };

    // Win32 0 is never restarted, whatever the service-specific code (P7b).
    assert!(!pending(RESTARTS, exit(0)));
    // 1077 is SCM's own record for a service that has not run since boot, not a failure.
    assert!(!pending(RESTARTS, exit(1077)));
    assert!(pending(RESTARTS, exit(1066)));
    assert!(pending(RESTARTS, exit(31)));
    assert!(!pending(no_restart, exit(1066)));
    // Without the non-crash flag only a process that ended without reporting (1067) is restarted.
    assert!(!pending(crashes_only, exit(1066)));
    assert!(!pending(crashes_only, exit(31)));
    assert!(pending(crashes_only, exit(1067)));
}

/// `--timeout 0` issues nothing over a pending restart, as no stop does.
#[skuld::test]
fn a_stop_request_over_a_pending_restart_issues_nothing() {
    let mut fake = FakeScm::new([])
        .restarting()
        .found(QueriedState::Stopped, daemon_failed());
    assert_eq!(
        request_stop(&mut fake, Kind::Simple).unwrap(),
        RequestedStop::RestartPending
    );
    assert_eq!(fake.log, vec!["query"]);
}

#[skuld::test]
fn a_stop_request_is_issued_unless_a_restart_is_pending() {
    for (recovering, state, exit, sends) in [
        (true, QueriedState::Stopped, stopped_clean(), false),
        (false, QueriedState::Stopped, daemon_failed(), false),
        (true, QueriedState::StopPending, stopped_clean(), false),
        (true, QueriedState::Running, daemon_failed(), true),
    ] {
        let mut fake = FakeScm::new([]).found(state, exit);
        if recovering {
            fake = fake.restarting();
        }
        assert_eq!(
            request_stop(&mut fake, Kind::Simple).unwrap(),
            RequestedStop::Issued,
            "{state:?} {exit:?}"
        );
        // A service that is stopped, or already stopping, refuses a control and needs none.
        let sent = if sends {
            vec!["query", "control_stop"]
        } else {
            vec!["query"]
        };
        assert_eq!(fake.log, sent, "{state:?} {exit:?}");
    }
}

// a daemon that could not be killed -----------------------------------------------------------------------------------

/// The record of a stop that could not kill the daemon: Win32 0 beside service-specific 1003, which
/// is not a restart and not a clean stop.
fn incomplete_stop() -> RecordedExit {
    let (win32, specific) = Reported::incomplete_stop().scm_codes();
    RecordedExit { win32, specific }
}

#[skuld::test]
fn a_stop_that_could_not_kill_the_daemon_is_not_a_confirmed_stop() {
    let mut fake = FakeScm::new([Some(Observed::Stopped)]).stopped_after(Reported::incomplete_stop());
    assert_eq!(
        stop_via_notify(&mut fake, unbounded(), Kind::Simple).unwrap(),
        StopWaited::Incomplete
    );
}

/// The record outlives the stop that left it: a service found stopped with it still holds a daemon
/// that may be running.
#[skuld::test]
fn a_stopped_service_whose_record_is_an_incomplete_stop_is_not_a_confirmed_stop() {
    let mut fake = FakeScm::new([]).found(QueriedState::Stopped, incomplete_stop());
    assert_eq!(
        stop_via_notify(&mut fake, unbounded(), Kind::Simple).unwrap(),
        StopWaited::Incomplete
    );
    assert_eq!(fake.log, vec!["query"]);

    let mut fake = FakeScm::new([]).found(QueriedState::Stopped, incomplete_stop());
    assert_eq!(
        request_stop(&mut fake, Kind::Simple).unwrap(),
        RequestedStop::Incomplete
    );
    assert_eq!(fake.log, vec!["query"]);
}

// a service that is stopping ------------------------------------------------------------------------------------------

/// A registration hears only of what happens after it, so the state is read again once the arm is
/// made: a transition between the first read and the arm is never delivered.
#[skuld::test]
fn a_stopping_service_is_waited_on_after_its_state_is_read_again() {
    let mut fake = FakeScm::new([Some(Observed::Stopped)])
        .found(QueriedState::StopPending, stopped_clean())
        .stopped_with([stopped_clean()]);
    assert_eq!(
        stop_via_notify(&mut fake, unbounded(), Kind::Simple).unwrap(),
        StopWaited::Confirmed
    );
    assert_eq!(fake.log, vec!["query", "arm_stopped", "query", "wait", "got_stopped"]);
}

/// A `STOPPED` reached between the first read and the arm is judged from the second read. The
/// arm's registration is dropped with the handle, so the handle is free to arm again.
#[skuld::test]
fn a_stopping_service_that_stopped_before_the_arm_is_judged_from_the_second_read() {
    for (exit, want) in [
        (daemon_failed(), StopWaited::RestartPending),
        (stopped_clean(), StopWaited::Confirmed),
    ] {
        let mut fake = FakeScm::new([]).restarting().queried([
            (QueriedState::StopPending, stopped_clean()),
            (QueriedState::Stopped, exit),
        ]);
        assert_eq!(
            stop_via_notify(&mut fake, unbounded(), Kind::Simple).unwrap(),
            want,
            "{exit:?}"
        );
        assert_eq!(fake.log, vec!["query", "arm_stopped", "query", "reopen"], "{exit:?}");
    }
}

/// A service that stopped and was started again before the arm is on the way up, which the `STOPPED`
/// arm cannot hear of: the handle is closed and the service read from the top.
#[skuld::test]
fn a_stopping_service_that_started_again_before_the_arm_is_read_from_the_top() {
    let mut fake = FakeScm::new([Some(Observed::Running), Some(Observed::Stopped)])
        .queried([
            (QueriedState::StopPending, stopped_clean()),
            (QueriedState::StartPending, stopped_clean()),
            (QueriedState::StartPending, stopped_clean()),
        ])
        .stopped_with([stopped_clean()]);
    assert_eq!(
        stop_via_notify(&mut fake, unbounded(), Kind::Simple).unwrap(),
        StopWaited::Confirmed
    );
    assert_eq!(
        fake.log,
        vec![
            "query",
            "arm_stopped",
            "query",
            "reopen",
            "query",
            "arm_restart_resolves",
            "wait",
            "got_running",
            "query",
            "arm_stopped",
            "control_stop",
            "wait",
            "got_stopped",
        ]
    );
}

// a pending callback, and the arm that follows it ---------------------------------------------------------------------

/// The `STOPPED` arm made after a pending callback hears nothing that happened before it, as the
/// first one does not: the state is read again. A service that stopped and started again meanwhile
/// is on its way up, outside the arm's mask, and is read from the top rather than waited on for ever.
#[skuld::test]
fn the_arm_after_a_pending_callback_is_followed_by_a_read_that_finds_a_start() {
    let mut fake = FakeScm::new([
        Some(Observed::Pending),
        Some(Observed::Running),
        Some(Observed::Stopped),
    ])
    .queried([
        (QueriedState::Running, stopped_clean()),
        (QueriedState::StartPending, stopped_clean()),
        (QueriedState::StartPending, stopped_clean()),
    ])
    .stopped_with([stopped_clean()]);
    assert_eq!(
        stop_via_notify(&mut fake, unbounded(), Kind::Simple).unwrap(),
        StopWaited::Confirmed
    );
    assert_eq!(
        fake.log,
        vec![
            "query",
            "arm_stopped",
            "control_stop",
            "wait",
            "got_pending",
            "arm_stopped",
            "query",
            "reopen",
            "query",
            "arm_restart_resolves",
            "wait",
            "got_running",
            "query",
            "arm_stopped",
            "control_stop",
            "wait",
            "got_stopped",
        ]
    );
}

/// A `STOPPED` the second read finds is judged from that read, and the handle is free to arm again.
#[skuld::test]
fn the_arm_after_a_pending_callback_is_followed_by_a_read_that_finds_the_stop() {
    for (exit, want) in [
        (daemon_failed(), StopWaited::RestartPending),
        (stopped_clean(), StopWaited::Confirmed),
    ] {
        let mut fake = FakeScm::new([Some(Observed::Pending)])
            .restarting()
            .queried([(QueriedState::Running, stopped_clean()), (QueriedState::Stopped, exit)]);
        assert_eq!(
            stop_via_notify(&mut fake, unbounded(), Kind::Simple).unwrap(),
            want,
            "{exit:?}"
        );
        assert_eq!(
            fake.log,
            vec![
                "query",
                "arm_stopped",
                "control_stop",
                "wait",
                "got_pending",
                "arm_stopped",
                "query",
                "reopen"
            ],
            "{exit:?}"
        );
    }
}

/// A service that is still stopping is waited on under the arm just made.
#[skuld::test]
fn the_arm_after_a_pending_callback_keeps_waiting_for_a_service_that_is_still_stopping() {
    let mut fake = FakeScm::new([Some(Observed::Pending), Some(Observed::Stopped)])
        .queried([
            (QueriedState::Running, stopped_clean()),
            (QueriedState::StopPending, stopped_clean()),
        ])
        .stopped_with([stopped_clean()]);
    assert_eq!(
        stop_via_notify(&mut fake, unbounded(), Kind::Simple).unwrap(),
        StopWaited::Confirmed
    );
    assert_eq!(
        fake.log,
        vec![
            "query",
            "arm_stopped",
            "control_stop",
            "wait",
            "got_pending",
            "arm_stopped",
            "query",
            "wait",
            "got_stopped",
        ]
    );
}

/// `uninstall`'s stop is the same: a service that started again after the pending callback is
/// stopped, not waited on for a `STOPPED` that does not come.
#[skuld::test]
fn an_uninstall_rereads_the_service_after_the_arm_that_follows_a_pending_callback() {
    let mut fake = FakeScm::new([
        Some(Observed::Pending),
        Some(Observed::Running),
        Some(Observed::Stopped),
    ])
    .queried([
        (QueriedState::Running, stopped_clean()),
        (QueriedState::StartPending, stopped_clean()),
        (QueriedState::StartPending, stopped_clean()),
    ])
    .stopped_with([stopped_clean()]);
    assert_eq!(
        uninstall_teardown(&mut fake, unbounded(), Kind::Simple).unwrap(),
        StopWaited::Confirmed
    );
    assert_eq!(
        fake.log,
        vec![
            "query",
            "arm_stopped",
            "control_stop",
            "wait",
            "got_pending",
            "arm_stopped",
            "query",
            "reopen",
            "query",
            "arm_restart_resolves",
            "wait",
            "got_running",
            "query",
            "arm_stopped",
            "control_stop",
            "wait",
            "got_stopped",
            "delete",
            "query",
        ]
    );
}

// a refused control ---------------------------------------------------------------------------------------------------

/// `ERROR_INVALID_SERVICE_CONTROL` is what a starting service answers. By the time the state is read
/// to tell that from a service that never accepts `STOP`, it may be running and accepting it.
#[skuld::test]
fn a_refusal_from_a_service_that_moved_on_is_read_again() {
    let mut fake = FakeScm::new([Some(Observed::Stopped)])
        .controlled([ControlReply::NotAccepted])
        .queried([
            (QueriedState::Running, stopped_clean()),
            (QueriedState::StartPending, stopped_clean()),
            (QueriedState::Running, stopped_clean()),
        ])
        .stopped_with([stopped_clean()]);
    assert_eq!(
        stop_via_notify(&mut fake, unbounded(), Kind::Simple).unwrap(),
        StopWaited::Confirmed
    );
    assert_eq!(
        fake.log,
        vec![
            "query",
            "arm_stopped",
            "control_stop",
            "query",
            "reopen",
            "query",
            "arm_stopped",
            "control_stop",
            "wait",
            "got_stopped",
        ]
    );
}

#[skuld::test]
fn a_running_service_that_does_not_accept_stop_is_refused_for_good() {
    let mut fake = FakeScm::new([])
        .controlled([ControlReply::NotAccepted])
        .not_accepting_stop();
    let e = stop_via_notify(&mut fake, unbounded(), Kind::Simple).unwrap_err();
    assert!(e.to_string().contains("refused the stop control"), "{e}");
    assert_eq!(fake.log, vec!["query", "arm_stopped", "control_stop", "query"]);
}

/// Only a running service can be refusing for good: one that is starting, stopping or stopped
/// refuses a control whatever it accepts.
#[skuld::test]
fn a_refusal_from_a_service_that_is_not_running_is_read_again_whatever_it_accepts() {
    let mut fake = FakeScm::new([Some(Observed::Running), Some(Observed::Stopped)])
        .controlled([ControlReply::NotAccepted])
        .not_accepting_stop()
        .queried([
            (QueriedState::Running, stopped_clean()),
            (QueriedState::StartPending, stopped_clean()),
            (QueriedState::StartPending, stopped_clean()),
        ])
        .stopped_with([stopped_clean()]);
    assert_eq!(
        stop_via_notify(&mut fake, unbounded(), Kind::Simple).unwrap(),
        StopWaited::Confirmed
    );
    assert_eq!(
        fake.log,
        vec![
            "query",
            "arm_stopped",
            "control_stop",
            "query",
            "reopen",
            "query",
            "arm_restart_resolves",
            "wait",
            "got_running",
            "query",
            "arm_stopped",
            "control_stop",
            "wait",
            "got_stopped",
        ]
    );
}

/// A refusal the state read afterwards cannot explain is the answer: the service is `RUNNING` and
/// accepting `STOP`, as it was when the control was chosen, and sending the control again would meet
/// the same refusal for ever. No path repeats it.
#[skuld::test]
fn a_refusal_that_leaves_the_service_as_it_was_is_an_error_on_every_path() {
    for reply in [ControlReply::NotAccepted, ControlReply::StateMoved] {
        let mut fake = FakeScm::new([]).controlled([reply, reply, reply]);
        let e = stop_via_notify(&mut fake, unbounded(), Kind::Simple).unwrap_err();
        assert!(e.to_string().contains("refused the stop control"), "{e}");
        assert_eq!(
            fake.log,
            vec!["query", "arm_stopped", "control_stop", "query"],
            "{reply:?}"
        );

        let mut fake = FakeScm::new([]).controlled([reply, reply, reply]);
        let e = request_stop(&mut fake, Kind::Simple).unwrap_err();
        assert!(e.to_string().contains("refused the stop control"), "{e}");
        assert_eq!(fake.log, vec!["query", "control_stop", "query"], "{reply:?}");

        let mut fake = FakeScm::new([]).controlled([reply, reply, reply]);
        let e = uninstall_teardown(&mut fake, unbounded(), Kind::Simple).unwrap_err();
        assert!(e.to_string().contains("NOT deleted"), "{e}");
        assert!(!fake.log.contains(&"delete"), "{:?}", fake.log);
    }
}

/// A restart the stop heard the start of: the refusal after `RUNNING` is the same, wherever the
/// `RUNNING` was heard.
#[skuld::test]
fn a_refusal_after_a_start_resolved_to_running_is_an_error_when_nothing_moved() {
    let mut fake = FakeScm::new([Some(Observed::Running)])
        .queried([
            (QueriedState::StartPending, stopped_clean()),
            (QueriedState::Running, stopped_clean()),
        ])
        .controlled([ControlReply::NotAccepted]);
    let e = stop_via_notify(&mut fake, unbounded(), Kind::Simple).unwrap_err();
    assert!(e.to_string().contains("refused the stop control"), "{e}");
}

// a service whose start is in progress --------------------------------------------------------------------------------

/// A `START_PENDING` service accepts no control (the shim takes none before `RUNNING`), so a stop
/// sent now is refused. The stop waits for the start to resolve and stops what it started.
#[skuld::test]
fn a_stop_of_a_starting_service_waits_for_running_before_it_stops() {
    let deadline = an_hour_out();
    let mut fake = FakeScm::new([Some(Observed::Running), Some(Observed::Stopped)])
        .queried([(QueriedState::StartPending, stopped_clean())])
        .stopped_with([stopped_clean()]);
    assert_eq!(
        stop_via_notify(&mut fake, deadline, Kind::Simple).unwrap(),
        StopWaited::Confirmed
    );
    assert_eq!(
        fake.log,
        vec![
            "query",
            "arm_restart_resolves",
            "wait",
            "got_running",
            "query",
            "arm_stopped",
            "control_stop",
            "wait",
            "got_stopped",
        ]
    );
    assert_eq!(fake.arm_deadlines, vec![deadline; 2]);
    assert_eq!(fake.deadlines, vec![deadline; 2]);
}

#[skuld::test]
fn a_starting_service_that_reports_pending_again_is_watched_to_its_outcome() {
    let mut fake = FakeScm::new([
        Some(Observed::Pending),
        Some(Observed::Running),
        Some(Observed::Stopped),
    ])
    .queried([(QueriedState::StartPending, stopped_clean())])
    .stopped_with([stopped_clean()]);
    assert_eq!(
        stop_via_notify(&mut fake, unbounded(), Kind::Simple).unwrap(),
        StopWaited::Confirmed
    );
    assert_eq!(
        fake.log,
        vec![
            "query",
            "arm_restart_resolves",
            "wait",
            "got_pending",
            "arm_restart_resolves",
            "wait",
            "got_running",
            "query",
            "arm_stopped",
            "control_stop",
            "wait",
            "got_stopped",
        ]
    );
}

/// A start that ends in `STOPPED` with nothing queued leaves nothing to stop.
#[skuld::test]
fn a_start_that_ends_stopped_is_a_stop_without_a_control() {
    let mut fake = FakeScm::new([Some(Observed::Stopped)])
        .found(QueriedState::StartPending, stopped_clean())
        .stopped_with([stopped_clean()]);
    assert_eq!(
        stop_via_notify(&mut fake, unbounded(), Kind::Simple).unwrap(),
        StopWaited::Confirmed
    );
    assert_eq!(fake.log, vec!["query", "arm_restart_resolves", "wait", "got_stopped"]);
}

/// A start that ends in a failure with recovery configured has a restart pending, as any other
/// `STOPPED` with that record does.
#[skuld::test]
fn a_start_that_fails_into_a_queued_restart_is_refused() {
    let mut fake = FakeScm::new([Some(Observed::Stopped)])
        .restarting()
        .found(QueriedState::StartPending, stopped_clean())
        .stopped_with([daemon_failed()]);
    assert_eq!(
        stop_via_notify(&mut fake, unbounded(), Kind::Simple).unwrap(),
        StopWaited::RestartPending
    );
    assert_eq!(fake.log, vec!["query", "arm_restart_resolves", "wait", "got_stopped"]);
}

/// A wait for a start that runs out stops nothing and says a restart is pending.
#[skuld::test]
fn a_stop_that_runs_out_waiting_for_a_start_sends_no_control() {
    for (waits, arms) in [
        (vec![None], Waited::Confirmed),
        (vec![Some(Observed::Pending), None], Waited::Confirmed),
        (vec![], Waited::Expired),
    ] {
        let mut fake = FakeScm::new([])
            .found(QueriedState::StartPending, stopped_clean())
            .arming(arms);
        fake.waits = waits.into();
        assert_eq!(
            stop_via_notify(&mut fake, spent(), Kind::Simple).unwrap(),
            StopWaited::RestartPending
        );
        assert!(!fake.log.contains(&"control_stop"), "{:?}", fake.log);
    }
}

/// `--timeout 0` cannot wait for the start, so it sends nothing and says a restart is pending.
#[skuld::test]
fn a_stop_request_over_a_starting_service_issues_nothing() {
    let mut fake = FakeScm::new([]).found(QueriedState::StartPending, stopped_clean());
    assert_eq!(
        request_stop(&mut fake, Kind::Simple).unwrap(),
        RequestedStop::RestartPending
    );
    assert_eq!(fake.log, vec!["query"]);
}

// the uninstall teardown: stop, delete, settle ------------------------------------------------------------------------

/// A stopped service gets no stop, whatever is queued for it: the delete that follows is what
/// cancels a queued restart.
#[skuld::test]
fn a_stopped_service_is_deleted_and_not_stopped_whatever_is_queued() {
    for exit in [stopped_clean(), daemon_failed(), shim_failed()] {
        let mut fake = FakeScm::new([]).restarting().found(QueriedState::Stopped, exit);
        assert_eq!(
            uninstall_teardown(&mut fake, unbounded(), Kind::Simple).unwrap(),
            StopWaited::Confirmed
        );
        assert_eq!(fake.log, vec!["query", "delete", "query"], "{exit:?}");
        assert_eq!(
            fake.recovery_reads, 0,
            "{exit:?}: the configuration decides nothing here"
        );
    }
}

/// The stop comes before the delete, so a service that refuses to stop is not left marked for
/// deletion.
#[skuld::test]
fn a_running_service_is_stopped_before_the_delete() {
    let mut fake = FakeScm::new([Some(Observed::Stopped)]).stopped_with([stopped_clean()]);
    assert_eq!(
        uninstall_teardown(&mut fake, unbounded(), Kind::Simple).unwrap(),
        StopWaited::Confirmed
    );
    assert_eq!(
        fake.log,
        vec![
            "query",
            "arm_stopped",
            "control_stop",
            "wait",
            "got_stopped",
            "delete",
            "query"
        ]
    );
}

#[skuld::test]
fn every_state_that_takes_a_control_is_stopped_before_the_delete() {
    for state in [
        QueriedState::Running,
        QueriedState::ContinuePending,
        QueriedState::PausePending,
        QueriedState::Paused,
    ] {
        let mut fake = FakeScm::new([Some(Observed::Stopped)])
            .found(state, stopped_clean())
            .stopped_with([stopped_clean()]);
        assert_eq!(
            uninstall_teardown(&mut fake, unbounded(), Kind::Simple).unwrap(),
            StopWaited::Confirmed
        );
        let stop = fake.log.iter().position(|step| *step == "control_stop");
        let delete = fake.log.iter().position(|step| *step == "delete");
        assert!(
            stop.is_some() && delete.is_some() && stop < delete,
            "{state:?}: {:?}",
            fake.log
        );
        assert_eq!(
            fake.log.iter().filter(|step| **step == "control_stop").count(),
            1,
            "{state:?}"
        );
    }
}

/// A restart that fires between the stop and the delete is stopped after the delete, on the same
/// handle. Nothing can start again, so it is stopped without looking at the failure actions.
#[skuld::test]
fn a_restart_between_the_stop_and_the_delete_is_stopped_after_the_delete() {
    let mut fake = FakeScm::new([Some(Observed::Stopped)])
        .restarting()
        .queried([
            (QueriedState::Stopped, daemon_failed()),
            (QueriedState::Running, stopped_clean()),
        ])
        .stopped_with([stopped_clean()]);
    assert_eq!(
        uninstall_teardown(&mut fake, unbounded(), Kind::Simple).unwrap(),
        StopWaited::Confirmed
    );
    assert_eq!(
        fake.log,
        vec![
            "query",
            "delete",
            "query",
            "arm_stopped",
            "control_stop",
            "wait",
            "got_stopped"
        ]
    );
    assert_eq!(fake.recovery_reads, 0);
}

/// A service whose stop is refused is left intact: no delete, and the error says so.
#[skuld::test]
fn a_service_that_refuses_to_stop_is_not_deleted() {
    let mut fake = FakeScm::new([]).rejecting_stop();
    let e = uninstall_teardown(&mut fake, unbounded(), Kind::Simple).unwrap_err();
    assert_eq!(fake.log, vec!["query", "arm_stopped", "control_stop"]);
    assert!(e.to_string().contains("NOT deleted"), "{e}");
}

/// A stop that runs out before `STOPPED` deletes nothing.
#[skuld::test]
fn a_stop_that_runs_out_deletes_nothing() {
    let mut fake = FakeScm::new([None]);
    assert_eq!(
        uninstall_teardown(&mut fake, spent(), Kind::Simple).unwrap(),
        StopWaited::Expired
    );
    assert!(!fake.log.contains(&"delete"), "{:?}", fake.log);
    let mut fake = FakeScm::new([]).arming(Waited::Expired);
    assert_eq!(
        uninstall_teardown(&mut fake, spent(), Kind::Simple).unwrap(),
        StopWaited::Expired
    );
    assert!(!fake.log.contains(&"delete"), "{:?}", fake.log);
}

/// A stop that meets a failing daemon ends on the failure's `STOPPED`: it does not wait for a
/// restart, stop what it starts, or look at the failure actions. The delete that follows cancels
/// the restart.
#[skuld::test]
fn a_stop_that_meets_a_failing_daemon_does_not_wait_for_a_restart() {
    let mut fake = FakeScm::new([Some(Observed::Pending), Some(Observed::Stopped)])
        .restarting()
        .queried([
            (QueriedState::Running, stopped_clean()),
            (QueriedState::StopPending, stopped_clean()),
        ])
        .stopped_with([daemon_failed()]);
    assert_eq!(
        uninstall_teardown(&mut fake, unbounded(), Kind::Simple).unwrap(),
        StopWaited::Confirmed
    );
    assert_eq!(
        fake.log,
        vec![
            "query",
            "arm_stopped",
            "control_stop",
            "wait",
            "got_pending",
            "arm_stopped",
            "query",
            "wait",
            "got_stopped",
            "delete",
            "query",
        ]
    );
    assert_eq!(fake.recovery_reads, 0);
}

/// A delete that fails leaves the service as it was, and the error says so.
#[skuld::test]
fn a_delete_that_fails_says_the_service_was_not_deleted() {
    let mut fake = FakeScm::new([])
        .found(QueriedState::Stopped, stopped_clean())
        .rejecting_delete();
    let e = uninstall_teardown(&mut fake, unbounded(), Kind::Simple).unwrap_err();
    assert_eq!(fake.log, vec!["query", "delete"]);
    assert!(e.to_string().contains("NOT deleted"), "{e}");
}

/// A stop after the delete that fails says the delete happened: the service is marked for deletion
/// and goes when it stops.
#[skuld::test]
fn a_stop_that_fails_after_the_delete_says_the_service_is_deleted() {
    let mut fake = FakeScm::new([]).rejecting_stop().queried([
        (QueriedState::Stopped, stopped_clean()),
        (QueriedState::Running, stopped_clean()),
    ]);
    let e = uninstall_teardown(&mut fake, unbounded(), Kind::Simple).unwrap_err();
    assert_eq!(
        fake.log,
        vec!["query", "delete", "query", "arm_stopped", "control_stop"]
    );
    let text = e.to_string();
    assert!(text.contains("is deleted"), "{text}");
    assert!(!text.contains("NOT deleted"), "{text}");
}

/// A stop after the delete that runs out is reported, not taken for a stop.
#[skuld::test]
fn a_stop_after_the_delete_that_runs_out_reports_it() {
    let mut fake = FakeScm::new([None]).queried([
        (QueriedState::Stopped, stopped_clean()),
        (QueriedState::Running, stopped_clean()),
    ]);
    assert_eq!(
        uninstall_teardown(&mut fake, spent(), Kind::Simple).unwrap(),
        StopWaited::Expired
    );
    assert_eq!(fake.log.iter().filter(|step| **step == "delete").count(), 1);
}

/// Step 1 meets a starting service: it waits for `RUNNING` and stops it, rather than sending a stop
/// that is refused and leaves the service "NOT deleted".
#[skuld::test]
fn an_uninstall_that_meets_a_starting_service_waits_for_it_before_the_stop() {
    let mut fake = FakeScm::new([Some(Observed::Running), Some(Observed::Stopped)])
        .queried([(QueriedState::StartPending, stopped_clean())])
        .stopped_with([stopped_clean()]);
    assert_eq!(
        uninstall_teardown(&mut fake, unbounded(), Kind::Simple).unwrap(),
        StopWaited::Confirmed
    );
    assert_eq!(
        fake.log,
        vec![
            "query",
            "arm_restart_resolves",
            "wait",
            "got_running",
            "query",
            "arm_stopped",
            "control_stop",
            "wait",
            "got_stopped",
            "delete",
            "query",
        ]
    );
}

#[skuld::test]
fn an_uninstall_whose_starting_service_ends_stopped_sends_no_stop() {
    let mut fake = FakeScm::new([Some(Observed::Pending), Some(Observed::Stopped)])
        .found(QueriedState::StartPending, stopped_clean())
        .stopped_with([daemon_failed()])
        .restarting();
    assert_eq!(
        uninstall_teardown(&mut fake, unbounded(), Kind::Simple).unwrap(),
        StopWaited::Confirmed
    );
    assert_eq!(
        fake.log,
        vec![
            "query",
            "arm_restart_resolves",
            "wait",
            "got_pending",
            "arm_restart_resolves",
            "wait",
            "got_stopped",
            "delete",
            "query",
        ]
    );
    assert_eq!(fake.recovery_reads, 0, "uninstall never waits on a restart");
}

#[skuld::test]
fn an_uninstall_that_runs_out_waiting_for_a_start_deletes_nothing() {
    let mut fake = FakeScm::new([None]).found(QueriedState::StartPending, stopped_clean());
    assert_eq!(
        uninstall_teardown(&mut fake, spent(), Kind::Simple).unwrap(),
        StopWaited::Expired
    );
    assert!(!fake.log.contains(&"delete"), "{:?}", fake.log);
    assert!(!fake.log.contains(&"control_stop"), "{:?}", fake.log);
}

/// A restart that fires between the stop and the delete is in `START_PENDING` when step 3 reads it:
/// that step waits for `RUNNING` and stops it.
#[skuld::test]
fn a_restart_starting_when_the_delete_is_done_is_stopped_once_it_runs() {
    let mut fake = FakeScm::new([Some(Observed::Running), Some(Observed::Stopped)])
        .restarting()
        .queried([
            (QueriedState::Stopped, daemon_failed()),
            (QueriedState::StartPending, stopped_clean()),
        ])
        .stopped_with([stopped_clean()]);
    assert_eq!(
        uninstall_teardown(&mut fake, unbounded(), Kind::Simple).unwrap(),
        StopWaited::Confirmed
    );
    assert_eq!(
        fake.log,
        vec![
            "query",
            "delete",
            "query",
            "arm_restart_resolves",
            "wait",
            "got_running",
            "query",
            "arm_stopped",
            "control_stop",
            "wait",
            "got_stopped",
        ]
    );
}

#[skuld::test]
fn a_restart_starting_when_the_delete_is_done_that_fails_needs_no_stop() {
    let mut fake = FakeScm::new([Some(Observed::Stopped)])
        .queried([
            (QueriedState::Stopped, daemon_failed()),
            (QueriedState::StartPending, stopped_clean()),
        ])
        .stopped_with([daemon_failed()]);
    assert_eq!(
        uninstall_teardown(&mut fake, unbounded(), Kind::Simple).unwrap(),
        StopWaited::Confirmed
    );
    assert!(!fake.log.contains(&"control_stop"), "{:?}", fake.log);
}

/// Step 1 reads a stopping service again once it has armed, as `stop` does.
#[skuld::test]
fn an_uninstall_that_meets_a_stopping_service_reads_it_again_after_the_arm() {
    let mut fake = FakeScm::new([Some(Observed::Stopped)])
        .found(QueriedState::StopPending, stopped_clean())
        .stopped_with([stopped_clean()]);
    assert_eq!(
        uninstall_teardown(&mut fake, unbounded(), Kind::Simple).unwrap(),
        StopWaited::Confirmed
    );
    assert_eq!(
        fake.log,
        vec![
            "query",
            "arm_stopped",
            "query",
            "wait",
            "got_stopped",
            "delete",
            "query"
        ]
    );
}

#[skuld::test]
fn an_uninstall_whose_stopping_service_stopped_before_the_arm_needs_no_wait() {
    let mut fake = FakeScm::new([]).restarting().queried([
        (QueriedState::StopPending, stopped_clean()),
        (QueriedState::Stopped, daemon_failed()),
        (QueriedState::Stopped, daemon_failed()),
    ]);
    assert_eq!(
        uninstall_teardown(&mut fake, unbounded(), Kind::Simple).unwrap(),
        StopWaited::Confirmed
    );
    assert_eq!(
        fake.log,
        vec!["query", "arm_stopped", "query", "reopen", "delete", "query"]
    );
}

#[skuld::test]
fn an_uninstall_whose_stopping_service_started_again_before_the_arm_reads_it_from_the_top() {
    let mut fake = FakeScm::new([Some(Observed::Stopped)])
        .queried([
            (QueriedState::StopPending, stopped_clean()),
            (QueriedState::Running, stopped_clean()),
        ])
        .stopped_with([stopped_clean()]);
    assert_eq!(
        uninstall_teardown(&mut fake, unbounded(), Kind::Simple).unwrap(),
        StopWaited::Confirmed
    );
    assert_eq!(
        fake.log,
        vec![
            "query",
            "arm_stopped",
            "query",
            "reopen",
            "query",
            "arm_stopped",
            "control_stop",
            "wait",
            "got_stopped",
            "delete",
            "query",
        ]
    );
}

/// A daemon the stop could not kill is stopped as far as SCM knows: the service is deleted, and the
/// caller is told the daemon may still be running.
#[skuld::test]
fn an_uninstall_of_a_daemon_that_could_not_be_killed_deletes_and_says_so() {
    let mut fake = FakeScm::new([]).found(QueriedState::Stopped, incomplete_stop());
    assert_eq!(
        uninstall_teardown(&mut fake, unbounded(), Kind::Simple).unwrap(),
        StopWaited::Incomplete
    );
    assert_eq!(fake.log, vec!["query", "delete", "query"]);

    let mut fake = FakeScm::new([Some(Observed::Stopped)]).stopped_after(Reported::incomplete_stop());
    assert_eq!(
        uninstall_teardown(&mut fake, unbounded(), Kind::Simple).unwrap(),
        StopWaited::Incomplete
    );
    assert_eq!(fake.log.last(), Some(&"query"));
    assert!(fake.log.contains(&"delete"), "{:?}", fake.log);
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
    request_start(&mut fake, Kind::Simple).unwrap();

    let mut fake = FakeScm::new([Some(Observed::Running)]).already_running(QueriedState::Running);
    assert_eq!(
        start_via_notify(&mut fake, unbounded(), Kind::Simple, Kind::Simple).unwrap(),
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
            start_via_notify(&mut fake, deadline, Kind::Simple, Kind::Simple).unwrap(),
            Waited::Confirmed
        );
        assert_eq!(
            fake.log,
            vec!["query", "arm_running", "start"],
            "nothing left to wait for"
        );
    }
}

/// `StartPending` is on its way, not there: under a spent deadline that is still an expiry.
#[skuld::test]
fn a_start_the_scm_answers_start_pending_still_expires_on_a_spent_deadline() {
    let mut fake = FakeScm::new([])
        .already_running(QueriedState::StartPending)
        .arming(Waited::Expired);
    assert_eq!(
        start_via_notify(&mut fake, spent(), Kind::Simple, Kind::Simple).unwrap(),
        Waited::Expired
    );
}

#[skuld::test]
fn a_plain_start_over_a_stopping_service_is_refused() {
    let mut fake = FakeScm::new([]).already_running(QueriedState::StopPending);
    assert!(request_start(&mut fake, Kind::Simple).is_err());
}

/// `restart --timeout 0`'s start, over the service the real `goetia-shim`
/// presents after an unconfirmed stop: `RUNNING` for its whole teardown,
/// never `STOP_PENDING`. That 1056 is a refusal — reading it as a start
/// reports a restart that leaves the daemon down.
#[skuld::test]
fn a_start_after_an_unconfirmed_stop_refuses_a_service_still_reading_running() {
    let mut fake = FakeScm::new([]).already_running(QueriedState::Running);
    assert!(request_start_after_stop(&mut fake, Kind::Simple).is_err());
    assert_eq!(
        fake.log,
        vec!["query", "start"],
        "it arms nothing and waits for nothing"
    );
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
        assert!(request_start_after_stop(&mut fake, Kind::Simple).is_err(), "{state:?}");
    }
}

#[skuld::test]
fn a_start_after_an_unconfirmed_stop_that_the_scm_accepts_is_ok() {
    let mut fake = FakeScm::new([]);
    request_start_after_stop(&mut fake, Kind::Simple).unwrap();
}

// a start over a daemon that may still be running ---------------------------------------------------------------------

/// A service `STOPPED` with the record of a stop that could not kill the daemon holds a daemon that
/// may be running, and `StartServiceW` would accept the start. No start goes out over it, whichever
/// verb or budget asks.
#[skuld::test]
fn no_start_is_issued_over_the_record_of_an_incomplete_stop() {
    let found = |fake: FakeScm| fake.found(QueriedState::Stopped, incomplete_stop());
    let mut fake = found(FakeScm::new([]));
    let e = start_via_notify(&mut fake, unbounded(), Kind::Simple, Kind::Simple).unwrap_err();
    assert!(e.to_string().contains("may still be running"), "{e}");
    assert_eq!(fake.log, vec!["query"]);

    let mut fake = found(FakeScm::new([]));
    let e = request_start(&mut fake, Kind::Simple).unwrap_err();
    assert!(e.to_string().contains("may still be running"), "{e}");
    assert_eq!(fake.log, vec!["query"]);

    let mut fake = found(FakeScm::new([]));
    let e = request_start_after_stop(&mut fake, Kind::Simple).unwrap_err();
    assert!(e.to_string().contains("may still be running"), "{e}");
    assert_eq!(fake.log, vec!["query"]);
}

/// Only that record refuses: every other state, and every other record, is started as before.
#[skuld::test]
fn a_start_over_any_other_state_or_record_is_issued() {
    for (state, exit) in [
        (QueriedState::Stopped, stopped_clean()),
        (QueriedState::Stopped, daemon_failed()),
        (QueriedState::Stopped, shim_failed()),
        (QueriedState::Running, incomplete_stop()),
        (QueriedState::StartPending, incomplete_stop()),
    ] {
        let mut fake = FakeScm::new([]).found(state, exit);
        request_start(&mut fake, Kind::Simple).unwrap();
        assert_eq!(fake.log, vec!["query", "start"], "{state:?} {exit:?}");

        let mut fake = FakeScm::new([]).found(state, exit);
        request_start_after_stop(&mut fake, Kind::Simple).unwrap();
        assert_eq!(fake.log, vec!["query", "start"], "{state:?} {exit:?}");
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

// a service that has never started ------------------------------------------------------------------------------------

/// `ERROR_SERVICE_NEVER_STARTED`: what SCM holds for a service that has not run since it was created
/// or since boot. Not a failure, so nothing is queued.
fn never_started() -> RecordedExit {
    RecordedExit {
        win32: 1077,
        specific: 0,
    }
}

#[skuld::test]
fn a_service_that_has_never_started_has_nothing_pending_on_every_stop_path() {
    let found = || {
        FakeScm::new([])
            .restarting()
            .found(QueriedState::Stopped, never_started())
    };

    let mut fake = found();
    assert_eq!(
        stop_via_notify(&mut fake, unbounded(), Kind::Simple).unwrap(),
        StopWaited::Confirmed
    );
    assert_eq!(fake.recovery_reads, 0);

    let mut fake = found();
    assert_eq!(request_stop(&mut fake, Kind::Simple).unwrap(), RequestedStop::Issued);
    assert_eq!(fake.recovery_reads, 0);

    let mut fake = found();
    assert_eq!(
        uninstall_teardown(&mut fake, unbounded(), Kind::Simple).unwrap(),
        StopWaited::Confirmed
    );
}

// the shim's vocabulary is the shim's ---------------------------------------------------------------------------------

/// `(0, 1003)` is how `goetia-shim` reports a daemon it could not kill. A `type: managed` service
/// speaks SCM itself and may report that pair for its own reasons.
#[skuld::test]
fn a_managed_service_that_stops_with_the_shims_incomplete_pair_is_a_stopped_service() {
    let found = || FakeScm::new([]).found(QueriedState::Stopped, incomplete_stop());

    let mut fake = found();
    assert_eq!(
        stop_via_notify(&mut fake, unbounded(), Kind::Managed).unwrap(),
        StopWaited::Confirmed
    );
    let mut fake = found();
    assert_eq!(request_stop(&mut fake, Kind::Managed).unwrap(), RequestedStop::Issued);
    let mut fake = found();
    assert_eq!(
        uninstall_teardown(&mut fake, unbounded(), Kind::Managed).unwrap(),
        StopWaited::Confirmed
    );
    // The same record delivered by the `STOPPED` callback of a stop that was issued.
    let mut fake = FakeScm::new([Some(Observed::Stopped)]).stopped_with([incomplete_stop()]);
    assert_eq!(
        stop_via_notify(&mut fake, unbounded(), Kind::Managed).unwrap(),
        StopWaited::Confirmed
    );
    let mut fake = FakeScm::new([Some(Observed::Stopped)]).stopped_with([incomplete_stop()]);
    assert_eq!(
        uninstall_teardown(&mut fake, unbounded(), Kind::Managed).unwrap(),
        StopWaited::Confirmed
    );
}

#[skuld::test]
fn a_start_over_a_managed_service_with_the_shims_incomplete_pair_is_not_refused() {
    let found = || FakeScm::new([Some(Observed::Running)]).found(QueriedState::Stopped, incomplete_stop());

    request_start(&mut found(), Kind::Managed).unwrap();
    request_start_after_stop(&mut found(), Kind::Managed).unwrap();
    assert_eq!(
        start_via_notify(&mut found(), unbounded(), Kind::Managed, Kind::Managed).unwrap(),
        Waited::Confirmed
    );

    // A `type: simple` service with the record still refuses: a second daemon would run.
    assert!(request_start(&mut found(), Kind::Simple).is_err());
    assert!(request_start_after_stop(&mut found(), Kind::Simple).is_err());
    assert!(start_via_notify(&mut found(), unbounded(), Kind::Simple, Kind::Simple).is_err());
}

/// A start whose spec blob could not be read waits strictly (a `STOPPED` before `RUNNING` is a failed
/// start, as for `type: managed`) but reads the exit record as the shim's: refusing a start over a
/// daemon that may still run costs a refusal, and starting over it costs a second daemon.
#[skuld::test]
fn a_start_judged_strictly_still_refuses_over_the_shims_incomplete_record() {
    let found = || FakeScm::new([Some(Observed::Running)]).found(QueriedState::Stopped, incomplete_stop());
    let e = start_via_notify(&mut found(), unbounded(), Kind::Managed, Kind::Simple).unwrap_err();
    assert!(e.to_string().contains("may still be running"), "{e}");

    let mut fake = FakeScm::new([Some(Observed::Stopped)]).stopped_after(Reported::Clean);
    let e = start_via_notify(&mut fake, unbounded(), Kind::Managed, Kind::Simple).unwrap_err();
    assert!(e.to_string().contains("failed start"), "{e}");
}

/// The record of an incomplete stop is read as the shim's wherever the kind says so, whatever the
/// service is read as otherwise.
#[skuld::test]
fn a_stop_that_reads_the_shims_vocabulary_reports_the_incomplete_record() {
    let found = || FakeScm::new([]).found(QueriedState::Stopped, incomplete_stop());
    assert_eq!(
        stop_via_notify(&mut found(), unbounded(), Kind::Simple).unwrap(),
        StopWaited::Incomplete
    );
    assert_eq!(
        request_stop(&mut found(), Kind::Simple).unwrap(),
        RequestedStop::Incomplete
    );
    assert_eq!(
        uninstall_teardown(&mut found(), unbounded(), Kind::Simple).unwrap(),
        StopWaited::Incomplete
    );
}

// a refusal from a new run --------------------------------------------------------------------------------------------

/// A service that stopped and came back between the read and the control is `RUNNING` again, in a
/// new process. The state is the same; the run is not, so the refusal is read again.
#[skuld::test]
fn a_refusal_from_a_new_run_in_the_same_state_is_read_again_on_every_path() {
    let runs = || {
        FakeScm::new([Some(Observed::Stopped)])
            .controlled([ControlReply::StateMoved])
            .queried([
                (QueriedState::Running, stopped_clean()),
                (QueriedState::Running, stopped_clean()),
                (QueriedState::Running, stopped_clean()),
            ])
            .running_as([10, 11, 11])
            .stopped_with([stopped_clean()])
    };

    let mut fake = runs();
    assert_eq!(
        stop_via_notify(&mut fake, unbounded(), Kind::Simple).unwrap(),
        StopWaited::Confirmed
    );
    assert_eq!(
        fake.log,
        vec![
            "query",
            "arm_stopped",
            "control_stop",
            "query",
            "reopen",
            "query",
            "arm_stopped",
            "control_stop",
            "wait",
            "got_stopped",
        ]
    );

    let mut fake = runs();
    assert_eq!(
        uninstall_teardown(&mut fake, unbounded(), Kind::Simple).unwrap(),
        StopWaited::Confirmed
    );
    assert!(fake.log.contains(&"delete"), "{:?}", fake.log);

    let mut fake = FakeScm::new([])
        .controlled([ControlReply::StateMoved])
        .queried([
            (QueriedState::Running, stopped_clean()),
            (QueriedState::Running, stopped_clean()),
            (QueriedState::Running, stopped_clean()),
        ])
        .running_as([10, 11, 11]);
    assert_eq!(request_stop(&mut fake, Kind::Simple).unwrap(), RequestedStop::Issued);
    assert_eq!(
        fake.log,
        vec!["query", "control_stop", "query", "query", "control_stop"]
    );
}

/// The same run, refusing, is the answer whatever its process id.
#[skuld::test]
fn a_refusal_from_the_same_run_is_an_error_whatever_its_process_id() {
    let mut fake = FakeScm::new([])
        .controlled([ControlReply::StateMoved])
        .queried([
            (QueriedState::Running, stopped_clean()),
            (QueriedState::Running, stopped_clean()),
        ])
        .running_as([10, 10]);
    let e = stop_via_notify(&mut fake, unbounded(), Kind::Simple).unwrap_err();
    assert!(e.to_string().contains("refused the stop control"), "{e}");
}

/// A `RUNNING` callback is the past by the time the stop acts on it, so the service is read before
/// the control: the process id decides whether a refusal found it moved.
#[skuld::test]
fn a_stop_that_waited_for_running_reads_the_run_before_it_sends_the_control() {
    let mut fake = FakeScm::new([Some(Observed::Running), Some(Observed::Stopped)])
        .controlled([ControlReply::StateMoved])
        .queried([
            (QueriedState::StartPending, stopped_clean()),
            (QueriedState::Running, stopped_clean()),
            (QueriedState::Running, stopped_clean()),
            (QueriedState::Running, stopped_clean()),
        ])
        .running_as([0, 10, 11, 11])
        .stopped_with([stopped_clean()]);
    assert_eq!(
        stop_via_notify(&mut fake, unbounded(), Kind::Simple).unwrap(),
        StopWaited::Confirmed
    );
    assert_eq!(
        fake.log,
        vec![
            "query",
            "arm_restart_resolves",
            "wait",
            "got_running",
            "query",
            "arm_stopped",
            "control_stop",
            "query",
            "reopen",
            "query",
            "arm_stopped",
            "control_stop",
            "wait",
            "got_stopped",
        ]
    );
}

// a refusal after the deadline ----------------------------------------------------------------------------------------

/// A reread is an RPC, not a wait: `--timeout` bounds the waiting, not learning where the service
/// is. A stop whose control was refused by a service that moved says what it moved to, however
/// little is left of the deadline.
#[skuld::test]
fn a_refusal_from_a_moved_service_is_read_again_after_the_deadline_has_run_out() {
    for (moved_to, record, want) in [
        (QueriedState::Stopped, stopped_clean(), StopWaited::Confirmed),
        (QueriedState::Stopped, daemon_failed(), StopWaited::RestartPending),
        (QueriedState::StartPending, stopped_clean(), StopWaited::RestartPending),
    ] {
        let mut fake = FakeScm::new([])
            .restarting()
            .arming(Waited::Expired)
            .controlled([ControlReply::StateMoved])
            .queried([
                (QueriedState::Running, stopped_clean()),
                (moved_to, record),
                (moved_to, record),
            ]);
        assert_eq!(
            stop_via_notify(&mut fake, spent(), Kind::Simple).unwrap(),
            want,
            "{moved_to:?} {record:?}"
        );
    }

    // `uninstall` goes on to delete a service the reread found stopped.
    let mut fake = FakeScm::new([])
        .arming(Waited::Expired)
        .controlled([ControlReply::StateMoved])
        .queried([
            (QueriedState::Running, stopped_clean()),
            (QueriedState::Stopped, stopped_clean()),
            (QueriedState::Stopped, stopped_clean()),
            (QueriedState::Stopped, stopped_clean()),
        ]);
    assert_eq!(
        uninstall_teardown(&mut fake, spent(), Kind::Simple).unwrap(),
        StopWaited::Confirmed
    );
    assert!(fake.log.contains(&"delete"), "{:?}", fake.log);
}
