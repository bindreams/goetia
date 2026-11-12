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
    /// The `dwWaitHint` of a `Stopped` the script delivers.
    wait_hint: u32,
    /// Whether each notification the script delivers accepts `SERVICE_ACCEPT_STOP`, front to back;
    /// `true` once it runs out.
    accepts: VecDeque<bool>,
    /// What the notification `wait_callback` returned last accepted.
    last_accepts: bool,
    /// What each `control_stop` answers, front to back; `Ok` once it runs out.
    stop_replies: VecDeque<StopReply>,
    /// What `query_status` answers, front to back, the last answer repeating; empty fails the test, so
    /// a refused STOP's status is always scripted.
    queried: VecDeque<QueriedStatus>,
    /// The notification no `control_stop` has answered yet. `None` after a `control_stop` and after
    /// an expiry.
    unanswered: Option<(Observed, bool)>,
    /// Each `control_stop` that did not follow a `Running` or `Paused` notification accepting STOP.
    /// Only the settle tests read it; the stop and start waits send theirs without a notification.
    bad_stops: Vec<String>,
    /// Whether an `arm` that registered has been neither delivered nor expired. An `arm` over it
    /// cancels it by reopening the handles, so the next one is the first on a fresh handle.
    armed: bool,
    /// The service's state as SCM would hold it: `None` until the first notification is delivered,
    /// then the state that notification reported, moved by [`Self::drifting_after`].
    state: Option<QueriedState>,
    /// The states the latest `arm` registered for.
    mask: &'static [QueriedState],
    /// Whether no `arm` has been made on the current handle: the actor is fresh, or the last `arm`
    /// cancelled an unconsumed registration by reopening the handles.
    fresh_handle: bool,
    /// Whether the latest `arm` was the first on its handle.
    fresh_arm: bool,
    /// How many notifications have been delivered.
    deliveries: usize,
    /// States the service moves to after the n-th notification, before the next `arm`.
    drifts: Vec<(usize, QueriedState)>,
    /// Whether [`Self::starting_in`] made this a model of SCM that queues a fresh arm's delivery.
    modelled: bool,
    /// The notification a fresh `arm` queued for the state current at the time: delivered by the next
    /// `wait_callback` whatever the service has done since, as SCM's APC is.
    at_once: Option<Observed>,
    /// States the service moves to as an action starts: `(step, state)`, applied once.
    shifts: Vec<(&'static str, QueriedState)>,
    /// The states in which SCM refuses an `arm` with 1072 (`ERROR_SERVICE_MARKED_FOR_DELETE`).
    refusing: &'static [QueriedState],
    /// The n-th `arm` (1-based) meets a service that is gone: opening its fresh handle failed with 1060.
    gone_at_arm: Option<usize>,
}

/// What a scripted `control_stop` answers.
#[derive(Debug, Clone, Copy)]
enum StopReply {
    Ok,
    /// `ERROR_SERVICE_NOT_ACTIVE`, which the real actor answers with `Ok(())`: the service was
    /// already `STOPPED` when the STOP arrived, so nothing is modelled as stopping.
    NotActive,
    Os(i32),
}

impl FakeScm {
    fn new(waits: impl IntoIterator<Item = Option<Observed>>) -> Self {
        Self {
            log: vec![],
            waits: waits.into_iter().collect(),
            what_arm_reports: Waited::Confirmed,
            start_fails: false,
            start_reply: StartReply::Accepted,
            deadlines: vec![],
            arm_deadlines: vec![],
            recorded: shim_failed(),
            wait_hint: 0,
            accepts: VecDeque::new(),
            last_accepts: true,
            stop_replies: VecDeque::new(),
            queried: VecDeque::new(),
            unanswered: None,
            bad_stops: vec![],
            armed: false,
            state: None,
            mask: &[],
            fresh_handle: true,
            fresh_arm: false,
            deliveries: 0,
            drifts: vec![],
            modelled: false,
            at_once: None,
            shifts: vec![],
            refusing: &[],
            gone_at_arm: None,
        }
    }

    /// The service is in `state` before any action, and a fresh `arm` queues a notification of the
    /// state current in its mask (P20, P27 c, f), which the next `wait_callback` delivers whatever the
    /// service has done since: a delivery made at the arm is not a report on what follows.
    fn starting_in(mut self, state: QueriedState) -> Self {
        self.state = Some(state);
        self.modelled = true;
        self
    }

    /// The service is in `state` as `step` runs: `control_stop`, `start`, `arm` (after its registration,
    /// before its read) or `wait`. A recovery restart that lands between goetia's arm and its request.
    fn shifting_at(mut self, step: &'static str, state: QueriedState) -> Self {
        self.shifts.push((step, state));
        self
    }

    /// SCM refuses an `arm` with 1072 while the service is in one of `states`: it is marked for
    /// deletion. Round 8 measured it for a re-arm excluding `STOPPED` in `STOPPED`; arms in `RUNNING` and
    /// the pending states returned 0 (P13, P22b, P22d), so a test of another state is a model, not a
    /// measurement.
    fn refusing_arms_in(mut self, states: &'static [QueriedState]) -> Self {
        self.refusing = states;
        self
    }

    /// The `n`-th `arm` (1-based) finds the service deleted: the cancel of the registration before it
    /// could not open a fresh handle (1060).
    fn gone_at_arm(mut self, n: usize) -> Self {
        self.gone_at_arm = Some(n);
        self
    }

    fn shift(&mut self, step: &'static str) {
        if let Some(at) = self.shifts.iter().position(|(s, _)| *s == step) {
            self.state = Some(self.shifts.remove(at).1);
        }
    }

    /// After the `n`-th notification the service moves on to `state`, before the next `arm`: a
    /// restart that runs its course while goetia's thread is between a callback and its re-arm.
    fn drifting_after(mut self, n: usize, state: QueriedState) -> Self {
        self.drifts.push((n, state));
        self
    }

    /// Whether the notification `observed` is one the latest `arm`'s mask admits. `Pending` is
    /// either pending state.
    fn mask_admits(&self, observed: Observed) -> bool {
        use QueriedState::{Paused, Running, StartPending, StopPending, Stopped};
        let admits = |state| self.mask.contains(&state);
        match observed {
            Observed::Running => admits(Running),
            Observed::Paused => admits(Paused),
            Observed::Stopped => admits(Stopped),
            Observed::Pending => admits(StartPending) || admits(StopPending),
        }
    }

    /// The state `observed` reports. `Pending` is read off the mask, which names one pending state
    /// when a pending state can be delivered at all.
    fn state_of(&self, observed: Observed) -> QueriedState {
        match observed {
            Observed::Running => QueriedState::Running,
            Observed::Paused => QueriedState::Paused,
            Observed::Stopped => QueriedState::Stopped,
            Observed::Pending if self.mask.contains(&QueriedState::StopPending) => QueriedState::StopPending,
            Observed::Pending => QueriedState::StartPending,
        }
    }

    /// SCM's rule, as far as it is measured: the first registration on a handle fires at once on a
    /// current state in its mask (P20, P27 c, f). A re-arm with the same context and buffer on the same
    /// handle does not fire on a state already current at the arm, whether or not it changed since the
    /// last delivery: P20 and P27 a measured the unchanged case, and the changed one is not measured,
    /// so it is modelled as silent. Later a registration fires only on a transition into the mask,
    /// which is the next scripted notification (P27 d, e). A wait with neither never returns; here it
    /// panics instead of hanging.
    /// Not applied before the first notification, when the state is not scripted.
    fn assert_something_can_fire(&self) {
        let Some(state) = self.state else { return };
        let fires_now = self.fresh_arm && self.mask.contains(&state);
        let fires_later = match self.waits.front() {
            Some(Some(observed)) => self.mask_admits(*observed),
            Some(None) => true,
            None => false,
        };
        if fires_now || fires_later {
            return;
        }
        let mask = self.mask;
        if mask.contains(&state) {
            panic!(
                "lost wake-up: state {state:?} in mask {mask:?} was current at a same-handle re-arm, which does not fire, and nothing is scripted to change it"
            );
        }
        panic!("lost wake-up: state {state:?} outside mask {mask:?}");
    }

    /// Notification `n` accepts `SERVICE_ACCEPT_STOP` iff `accepts[n]`.
    fn accepting(mut self, accepts: impl IntoIterator<Item = bool>) -> Self {
        self.accepts = accepts.into_iter().collect();
        self
    }

    /// The `control_stop` calls answer `replies` in order.
    fn stop_replies(mut self, replies: impl IntoIterator<Item = StopReply>) -> Self {
        self.stop_replies = replies.into_iter().collect();
        self
    }

    /// A `query_status` answers that the service is in `state`, accepting STOP iff `accepts_stop`.
    fn queried(mut self, state: QueriedState, accepts_stop: bool) -> Self {
        self.queried = [scripted(state, accepts_stop)].into();
        self
    }

    /// The next `query_status` after the scripted ones answers that the service is in `state`,
    /// accepting STOP iff `accepts_stop`: the service moved on between two reads.
    fn then_queried(mut self, state: QueriedState, accepts_stop: bool) -> Self {
        self.queried.push_back(scripted(state, accepts_stop));
        self
    }

    fn stop_count(&self) -> usize {
        self.log.iter().filter(|step| **step == "control_stop").count()
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

    /// `start` answers `ERROR_SERVICE_ALREADY_RUNNING`, with `queried` as the
    /// follow-up query's state.
    fn already_running(mut self, queried: QueriedState) -> Self {
        self.start_reply = StartReply::AlreadyRunning(queried);
        self
    }

    fn arm_count(&self) -> usize {
        self.log.iter().filter(|step| step.starts_with("arm_")).count()
    }

    /// The status SCM would report now, from the model: the state, with `SERVICE_ACCEPT_STOP` in
    /// `RUNNING` and `PAUSED`, and the record and wait hint a delivered `Stopped` carries. Before the
    /// first notification the state is not scripted, and the first arm's caller does not read it.
    /// The status [`ScmActor::arm`] returns: the service's, or the scripted answer of a service not
    /// yet modelled.
    fn status_at_arm(&self) -> QueriedStatus {
        match (self.state, self.queried.front()) {
            (None, Some(scripted)) => *scripted,
            _ => self.model_status(),
        }
    }

    fn model_status(&self) -> QueriedStatus {
        let state = self.state.unwrap_or(QueriedState::Stopped);
        QueriedStatus {
            state,
            accepts_stop: matches!(state, QueriedState::Running | QueriedState::Paused),
            exit: self.recorded,
            wait_hint: self.wait_hint,
        }
    }
}

/// The notification of `state`; either pending state is [`Observed::Pending`].
fn observed_in(state: QueriedState) -> Observed {
    match state {
        QueriedState::Running => Observed::Running,
        QueriedState::Paused => Observed::Paused,
        QueriedState::Stopped => Observed::Stopped,
        _ => Observed::Pending,
    }
}

/// A status a script gives to `query_status`, with no exit record.
fn scripted(state: QueriedState, accepts_stop: bool) -> QueriedStatus {
    QueriedStatus {
        state,
        accepts_stop,
        exit: RecordedExit { win32: 0, specific: 0 },
        wait_hint: 0,
    }
}

impl ScmActor for FakeScm {
    fn arm(&mut self, want: WantState, deadline: Deadline) -> std::io::Result<Armed> {
        self.arm_deadlines.push(deadline);
        self.log.push(match want {
            WantState::Stopped => "arm_stopped",
            WantState::RunningPausedOrStopped => "arm_settle",
            WantState::StoppedOnly => "arm_stopped_only",
        });
        if Some(self.arm_count()) == self.gone_at_arm {
            self.armed = false;
            return Ok(Armed::Gone);
        }
        // An unconsumed registration is cancelled by reopening the handles, as `SystemScmActor` does.
        if std::mem::take(&mut self.armed) {
            self.fresh_handle = true;
        }
        if let Some(at) = self.drifts.iter().position(|(n, _)| *n == self.deliveries) {
            self.state = Some(self.drifts.remove(at).1);
        }
        self.mask = want.states();
        self.fresh_arm = std::mem::take(&mut self.fresh_handle);
        self.at_once = None;
        if self.what_arm_reports == Waited::Expired {
            return Ok(Armed::Expired);
        }
        // The status is read after the registration, so the service can move in between.
        let registered = self.state;
        self.shift("arm");
        let status = self.status_at_arm();
        if self.state.is_some() && self.refusing.contains(&status.state) {
            return Ok(Armed::Unwatchable(status));
        }
        self.armed = true;
        if let (true, true, Some(state)) = (self.modelled, self.fresh_arm, registered)
            && self.mask.contains(&state)
        {
            self.at_once = Some(observed_in(state));
        }
        // The settle arm is the one whose read is acted on: the STOP that follows answers it.
        if want == WantState::RunningPausedOrStopped {
            self.unanswered = match status.state {
                QueriedState::Running => Some((Observed::Running, status.accepts_stop)),
                QueriedState::Paused => Some((Observed::Paused, status.accepts_stop)),
                _ => None,
            };
        }
        Ok(Armed::Registered(status))
    }
    fn control_stop(&mut self) -> std::io::Result<()> {
        self.log.push("control_stop");
        self.shift("control_stop");
        match self.unanswered.take() {
            Some((Observed::Running | Observed::Paused, true)) => {}
            other => self.bad_stops.push(format!("after {other:?}")),
        }
        match self.stop_replies.pop_front().unwrap_or(StopReply::Ok) {
            StopReply::Ok => {
                // The STOP is taken: a service not yet modelled is stopping.
                self.state.get_or_insert(QueriedState::StopPending);
                Ok(())
            }
            StopReply::NotActive => Ok(()),
            StopReply::Os(code) => Err(std::io::Error::from_raw_os_error(code)),
        }
    }
    fn start(&mut self) -> std::io::Result<StartReply> {
        self.log.push("start");
        self.shift("start");
        if self.start_fails {
            return Err(std::io::Error::other("scripted start failure"));
        }
        // A start SCM accepts leaves a stopped service starting; one it answers 1056 leaves the state
        // the answer carries.
        match self.start_reply {
            StartReply::Accepted if matches!(self.state, None | Some(QueriedState::Stopped)) => {
                self.state = Some(QueriedState::StartPending);
            }
            StartReply::AlreadyRunning(queried) => {
                self.state.get_or_insert(queried);
            }
            StartReply::Accepted => {}
        }
        Ok(self.start_reply)
    }
    fn wait_callback(&mut self, deadline: Deadline) -> std::io::Result<Option<Observed>> {
        self.log.push("wait");
        self.shift("wait");
        // Delivered, or expired: an expiry closes and drains the actor's handles.
        self.armed = false;
        self.deadlines.push(deadline);
        let observed = match self.at_once.take() {
            // Queued at the arm, so it reports the state then, not the one the service is in now.
            Some(queued) => {
                self.deliveries += 1;
                Some(queued)
            }
            None => {
                self.assert_something_can_fire();
                let observed = self.waits.pop_front().expect("script ran dry");
                if let Some(observed) = observed {
                    self.state = Some(self.state_of(observed));
                    self.deliveries += 1;
                }
                observed
            }
        };
        self.log.push(match observed {
            Some(Observed::Stopped) => "got_stopped",
            Some(Observed::Running) => "got_running",
            Some(Observed::Paused) => "got_paused",
            Some(Observed::Pending) => "got_pending",
            None => "got_nothing",
        });
        self.last_accepts = self.accepts.pop_front().unwrap_or(true);
        self.unanswered = observed.map(|o| (o, self.last_accepts));
        Ok(observed)
    }
    fn query_status(&mut self) -> std::io::Result<QueriedStatus> {
        self.log.push("query_status");
        // The service's state once a notification has told it, else the scripted answer.
        if self.state.is_some() {
            return Ok(self.model_status());
        }
        let answer = if self.queried.len() > 1 {
            self.queried.pop_front()
        } else {
            self.queried.front().copied()
        };
        Ok(answer.expect("query_status was not scripted"))
    }
    fn delivered_exit(&self) -> RecordedExit {
        self.recorded
    }
    fn delivered_wait_hint(&self) -> u32 {
        self.wait_hint
    }
    fn delivered_controls_accepted(&self) -> u32 {
        // `SERVICE_ACCEPT_STOP` is the only control the fake models.
        if self.last_accepts { SERVICE_ACCEPT_STOP } else { 0 }
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
fn stop_via_notify_sends_the_stop_then_arms_stopped_and_gates_on_stopped() {
    let mut fake = FakeScm::new([Some(Observed::Stopped)]);
    assert_eq!(
        stop_via_notify(&mut fake, unbounded(), false).unwrap(),
        Waited::Confirmed
    );
    assert_eq!(fake.log, vec!["control_stop", "arm_stopped", "wait", "got_stopped"]);
}

#[skuld::test]
fn stop_re_arms_after_a_non_terminal_callback() {
    // A pending (intermediate) callback fires while waiting for STOPPED, then
    // STOPPED; the non-terminal callback must trigger a re-arm.
    let mut fake = FakeScm::new([Some(Observed::Pending), Some(Observed::Stopped)]);
    assert_eq!(
        stop_via_notify(&mut fake, unbounded(), false).unwrap(),
        Waited::Confirmed
    );
    assert_eq!(
        fake.log,
        vec![
            "control_stop",
            "arm_stopped",
            "wait",
            "got_pending",
            "arm_stopped_only", // re-arm: only STOPPED is left to wait for; its status is read
            "wait",
            "got_stopped",
        ]
    );
}

/// A service in `STOP_PENDING` (a shim tearing the daemon down) refuses a second `STOP` with 1061
/// without reaching the handler (P18c). That stop is under way, so `stop` waits for it to end.
#[skuld::test]
fn stop_waits_out_a_stop_already_under_way() {
    let mut fake = FakeScm::new([Some(Observed::Pending), Some(Observed::Stopped)])
        .stop_replies([StopReply::Os(ERROR_SERVICE_CANNOT_ACCEPT_CTRL)])
        .queried(QueriedState::StopPending, false);
    assert_eq!(
        stop_via_notify(&mut fake, unbounded(), false).unwrap(),
        Waited::Confirmed
    );
    assert_eq!(
        fake.log,
        vec![
            "control_stop",
            "query_status",
            "arm_stopped",
            "wait",
            "got_pending",
            "arm_stopped_only",
            "wait",
            "got_stopped",
        ]
    );
}

/// The wait stays bounded by the caller's deadline: a stop under way that outlasts it expires.
#[skuld::test]
fn stop_over_a_stop_under_way_still_expires_on_the_deadline() {
    let mut fake = FakeScm::new([None])
        .stop_replies([StopReply::Os(ERROR_SERVICE_CANNOT_ACCEPT_CTRL)])
        .queried(QueriedState::StopPending, false);
    assert_eq!(stop_via_notify(&mut fake, unbounded(), false).unwrap(), Waited::Expired);
}

/// An `end` that nothing may call: the process is not held, or SCM has not confirmed `STOPPED`.
fn no_end(_: u32, _: Deadline) -> Result<EndProcessOutcome<u32>, ProcessWaitFailure> {
    panic!("a process is ended once SCM has confirmed the stop, and only a held one")
}

/// [`no_end`] in the shape an uninstall's `end` has, which takes no deadline.
#[cfg(windows)]
fn no_end_of(process: u32) -> Result<EndProcessOutcome<u32>, ProcessWaitFailure> {
    no_end(process, unbounded())
}

/// [`end_process`] over `terminate` and `exit`, in the shape [`stop_and_await_exit`] takes: the one
/// function both `stop` and `uninstall` end the process they held with.
fn ending(
    terminate: impl FnOnce(&u32) -> io::Result<()>,
    exit: impl FnOnce(&u32, Deadline, Option<&io::Error>) -> io::Result<Waited>,
) -> impl FnOnce(u32, Deadline) -> Result<EndProcessOutcome<u32>, ProcessWaitFailure> {
    move |process, deadline| end_process(process, deadline, terminate, exit)
}

/// SCM records `STOPPED` before the service's process exits, so a stop that held the process ends it
/// and then waits for its exit once SCM has confirmed, under the deadline the stop had.
#[skuld::test]
fn stop_and_await_exit_terminates_then_waits_for_the_held_process_after_stopped() {
    let mut fake = FakeScm::new([Some(Observed::Stopped)]);
    let deadline = Budget::Bounded(std::time::Duration::from_secs(3600)).start();
    let calls = std::cell::RefCell::new(vec![]);

    let outcome = stop_and_await_exit(
        &mut fake,
        deadline,
        &mut HoldOutcome::Held(7u32),
        false,
        ending(
            |process| {
                calls.borrow_mut().push(("terminate", *process, None));
                Ok(())
            },
            |process, by, _| {
                calls.borrow_mut().push(("wait", *process, Some(by)));
                Ok(Waited::Confirmed)
            },
        ),
    );

    assert!(matches!(outcome.unwrap(), StopAndAwaitExitOutcome::Stopped));
    assert_eq!(fake.log, vec!["control_stop", "arm_stopped", "wait", "got_stopped"]);
    assert_eq!(
        *calls.borrow(),
        vec![("terminate", 7, None), ("wait", 7, Some(deadline))]
    );
}

/// A process that outlasts the deadline is told apart from a service that does: the service is
/// stopped, and the process is the one that did not exit.
#[skuld::test]
fn stop_and_await_exit_names_the_process_that_outlasts_the_deadline() {
    let mut fake = FakeScm::new([Some(Observed::Stopped)]);

    let outcome = stop_and_await_exit(
        &mut fake,
        unbounded(),
        &mut HoldOutcome::Held(7u32),
        false,
        ending(|_| Ok(()), |_, _, _| Ok(Waited::Expired)),
    );

    assert!(matches!(
        outcome.unwrap(),
        StopAndAwaitExitOutcome::ProcessExpired {
            process: 7,
            terminate_failed: None
        }
    ));
}

#[skuld::test]
fn stop_and_await_exit_does_not_end_the_process_when_stopped_was_not_confirmed() {
    let mut fake = FakeScm::new([None]);

    let outcome = stop_and_await_exit(&mut fake, unbounded(), &mut HoldOutcome::Held(7u32), false, no_end);

    assert!(matches!(outcome.unwrap(), StopAndAwaitExitOutcome::ServiceExpired));
}

#[skuld::test]
fn stop_and_await_exit_without_a_held_process_is_the_stop() {
    let mut fake = FakeScm::new([Some(Observed::Stopped)]);

    let outcome = stop_and_await_exit(
        &mut fake,
        unbounded(),
        &mut HoldOutcome::<u32>::NoProcess,
        false,
        no_end,
    );

    assert!(matches!(outcome.unwrap(), StopAndAwaitExitOutcome::Stopped));
}

/// A process that could not be opened is not held, and the hold keeps why for the caller, which
/// knows where to report it; a process that was held is taken out of it.
#[skuld::test]
fn stop_and_await_exit_is_the_stop_alone_for_a_refused_process_and_leaves_its_reason() {
    let mut fake = FakeScm::new([Some(Observed::Stopped)]);
    let mut held = HoldOutcome::<u32>::Refused(io::Error::other("access denied"));

    let outcome = stop_and_await_exit(&mut fake, unbounded(), &mut held, false, no_end);

    assert!(matches!(outcome.unwrap(), StopAndAwaitExitOutcome::Stopped));
    let HoldOutcome::Refused(reason) = held else {
        panic!("the reason is kept: {held:?}");
    };
    assert_eq!(reason.to_string(), "access denied");

    let mut fake = FakeScm::new([Some(Observed::Stopped)]);
    let mut held = HoldOutcome::Held(7u32);
    let outcome = stop_and_await_exit(
        &mut fake,
        unbounded(),
        &mut held,
        false,
        ending(|_| Ok(()), |_, _, _| Ok(Waited::Confirmed)),
    );
    assert!(matches!(outcome.unwrap(), StopAndAwaitExitOutcome::Stopped));
    assert!(matches!(held, HoldOutcome::NoProcess), "{held:?}");
}

#[skuld::test]
fn stop_and_await_exit_reports_a_failed_wait_for_the_process() {
    let mut fake = FakeScm::new([Some(Observed::Stopped)]);

    let outcome = stop_and_await_exit(
        &mut fake,
        unbounded(),
        &mut HoldOutcome::Held(7u32),
        false,
        ending(|_| Ok(()), |_, _, _| Err(io::Error::other("wait failed"))),
    );

    let StopAndAwaitExitOutcome::ProcessWaitFailed(failure) = outcome.unwrap() else {
        panic!("the wait for the process failed");
    };
    assert_eq!(failure.wait_failed.to_string(), "wait failed");
    assert!(failure.terminate_failed.is_none());
}

/// A terminate that fails when it is called, as the OS can fail one for a handle it no longer
/// honours (`ERROR_INVALID_HANDLE`), which is not the 'already exiting' that `request_termination`
/// turns an access denial into.
fn terminate_fails(_: &u32) -> io::Result<()> {
    Err(io::Error::other("The handle is invalid. (os error 6)"))
}

// end_process ---------------------------------------------------------------------------------------------------------

/// SCM has confirmed `STOPPED`, so a terminate that fails is not a failure of the end: the shim exits
/// on its own just after it reports `STOPPED`, and the wait for the process decides.
#[skuld::test]
fn end_process_is_done_when_the_process_exits_after_a_failed_terminate() {
    let outcome = end_process(7u32, unbounded(), terminate_fails, |_, _, _| Ok(Waited::Confirmed));

    assert!(matches!(outcome.unwrap(), EndProcessOutcome::Exited));
}

/// The failed terminate is not lost: the wait is told of it, for its notice, and the process that
/// outlasts the deadline carries it, as the error it was.
#[skuld::test]
fn end_process_keeps_a_failed_terminate_for_the_wait_and_for_the_expiry() {
    let told = std::cell::RefCell::new(None);

    let outcome = end_process(7u32, unbounded(), terminate_fails, |_, _, terminate_failed| {
        *told.borrow_mut() = terminate_failed.map(ToString::to_string);
        Ok(Waited::Expired)
    });

    let EndProcessOutcome::Lingers {
        process,
        terminate_failed,
    } = outcome.unwrap()
    else {
        panic!("the process outlasted the deadline");
    };
    assert_eq!(process, 7);
    assert_eq!(
        terminate_failed.expect("the failed terminate is kept").to_string(),
        "The handle is invalid. (os error 6)"
    );
    assert_eq!(
        told.into_inner().as_deref(),
        Some("The handle is invalid. (os error 6)")
    );
}

/// The wait goes on after a terminate that failed, and its failure is the error as the wait gave it,
/// its OS code and kind intact; the failed terminate is carried beside it, as the error it was: with
/// the process neither ended nor seen to exit, both are why.
#[skuld::test]
fn end_process_keeps_a_failed_wait_and_a_failed_terminate_each_as_it_was() {
    let outcome = end_process(7u32, unbounded(), terminate_fails, |_, _, _| {
        Err(io::Error::from_raw_os_error(6))
    });

    let failure = outcome.unwrap_err();
    assert_eq!(failure.wait_failed.raw_os_error(), Some(6));
    assert_eq!(failure.wait_failed.kind(), io::Error::from_raw_os_error(6).kind());
    assert_eq!(
        failure
            .terminate_failed
            .expect("the failed terminate is kept")
            .to_string(),
        "The handle is invalid. (os error 6)"
    );
}

/// The kind of a wait that failed survives, whatever it is.
#[skuld::test]
fn end_process_keeps_the_kind_of_a_failed_wait() {
    let outcome = end_process(7u32, unbounded(), terminate_fails, |_, _, _| {
        Err(io::Error::new(io::ErrorKind::TimedOut, "wait failed"))
    });

    assert_eq!(outcome.unwrap_err().wait_failed.kind(), io::ErrorKind::TimedOut);
}

/// A wait that fails after a terminate that succeeded is told as it is, with no terminate failure.
#[skuld::test]
fn end_process_reports_a_failed_wait_alone_after_a_terminate_that_succeeded() {
    let outcome = end_process(
        7u32,
        unbounded(),
        |_| Ok(()),
        |_, _, _| Err(io::Error::from_raw_os_error(6)),
    );

    let failure = outcome.unwrap_err();
    assert_eq!(failure.wait_failed.raw_os_error(), Some(6));
    assert!(failure.terminate_failed.is_none());
}

/// `stop` over a confirmed `STOPPED` fails the same way, with both.
#[skuld::test]
fn stop_and_await_exit_reports_a_failed_wait_after_a_failed_terminate_with_both() {
    let mut fake = FakeScm::new([Some(Observed::Stopped)]);

    let outcome = stop_and_await_exit(
        &mut fake,
        unbounded(),
        &mut HoldOutcome::Held(7u32),
        false,
        ending(terminate_fails, |_, _, _| Err(io::Error::from_raw_os_error(6))),
    );

    let StopAndAwaitExitOutcome::ProcessWaitFailed(failure) = outcome.unwrap() else {
        panic!("the wait for the process failed");
    };
    assert_eq!(failure.wait_failed.raw_os_error(), Some(6));
    assert!(failure.terminate_failed.is_some());
}

/// The terminate precedes the wait, and the wait is told of nothing when the terminate succeeded.
#[skuld::test]
fn end_process_terminates_then_waits_under_the_deadline() {
    let deadline = Budget::Bounded(std::time::Duration::from_secs(3600)).start();
    let calls = std::cell::RefCell::new(vec![]);

    let outcome = end_process(
        7u32,
        deadline,
        |process| {
            calls.borrow_mut().push(("terminate", *process, None, false));
            Ok(())
        },
        |process, by, terminate_failed| {
            calls
                .borrow_mut()
                .push(("wait", *process, Some(by), terminate_failed.is_some()));
            Ok(Waited::Confirmed)
        },
    );

    assert!(matches!(outcome.unwrap(), EndProcessOutcome::Exited));
    assert_eq!(
        *calls.borrow(),
        vec![("terminate", 7, None, false), ("wait", 7, Some(deadline), false)]
    );
}

/// `stop` ends its held process through [`end_process`] too: a failed terminate there is the same
/// success when the process exits, and the same expiry when it does not.
#[skuld::test]
fn stop_and_await_exit_ends_the_process_as_end_process_does_after_a_failed_terminate() {
    let mut fake = FakeScm::new([Some(Observed::Stopped)]);
    let outcome = stop_and_await_exit(
        &mut fake,
        unbounded(),
        &mut HoldOutcome::Held(7u32),
        false,
        ending(terminate_fails, |_, _, _| Ok(Waited::Confirmed)),
    );
    assert!(matches!(outcome.unwrap(), StopAndAwaitExitOutcome::Stopped));

    let mut fake = FakeScm::new([Some(Observed::Stopped)]);
    let outcome = stop_and_await_exit(
        &mut fake,
        unbounded(),
        &mut HoldOutcome::Held(7u32),
        false,
        ending(terminate_fails, |_, _, _| Ok(Waited::Expired)),
    );
    let StopAndAwaitExitOutcome::ProcessExpired {
        process,
        terminate_failed,
    } = outcome.unwrap()
    else {
        panic!("the process outlasted the deadline");
    };
    assert_eq!(process, 7);
    assert_eq!(
        terminate_failed.expect("the failed terminate is kept").to_string(),
        "The handle is invalid. (os error 6)"
    );
}

/// A hold that expired is sent the STOP all the same, and the stop is not confirmed, even when SCM
/// answers `STOPPED` at once: the process the STOP reached may be one SCM restarted since the pid it
/// still reported, which nothing holds. Nothing waits, and no process is waited for.
#[skuld::test]
fn stop_and_await_exit_after_an_expired_hold_sends_the_stop_and_is_unconfirmed() {
    let mut fake = FakeScm::new([Some(Observed::Stopped)]);

    let outcome = stop_and_await_exit(
        &mut fake,
        spent(),
        &mut HoldOutcome::<u32>::Expired { pid: 9 },
        false,
        no_end,
    );

    assert!(matches!(
        outcome.unwrap(),
        StopAndAwaitExitOutcome::Unconfirmed { pid: 9 }
    ));
    assert_eq!(fake.log, vec!["control_stop"]);
}

/// A STOP that finds the service stopped is not a confirmed stop either: the hold expired because
/// the held process died, SCM may have a recovery restart pending, and a restarted process may have
/// reported `STOPPED` and still be exiting. Which error SCM picks for it (`ERROR_SERVICE_NOT_ACTIVE`,
/// or a refusal, 1061 or 1052, over a `STOPPED` read) does not change the outcome.
#[skuld::test]
fn stop_and_await_exit_after_an_expired_hold_is_unconfirmed_whichever_way_the_stop_finds_the_service_stopped() {
    for (name, mut fake, log) in [
        (
            "ERROR_SERVICE_NOT_ACTIVE",
            FakeScm::new([]).stop_replies([StopReply::NotActive]),
            vec!["control_stop"],
        ),
        (
            "1061 over STOPPED",
            FakeScm::new([])
                .stop_replies([StopReply::Os(ERROR_SERVICE_CANNOT_ACCEPT_CTRL)])
                .queried(QueriedState::Stopped, false),
            vec!["control_stop", "query_status"],
        ),
        (
            "1052 over STOPPED",
            FakeScm::new([])
                .stop_replies([StopReply::Os(ERROR_INVALID_SERVICE_CONTROL)])
                .queried(QueriedState::Stopped, false),
            vec!["control_stop", "query_status"],
        ),
    ] {
        let outcome = stop_and_await_exit(
            &mut fake,
            spent(),
            &mut HoldOutcome::<u32>::Expired { pid: 9 },
            false,
            no_end,
        );

        assert!(
            matches!(outcome.unwrap(), StopAndAwaitExitOutcome::Unconfirmed { pid: 9 }),
            "{name}"
        );
        assert_eq!(fake.log, log, "{name}");
    }
}

/// A STOP that is refused is an error, as for any stop: nothing was sent, so there is no stop to
/// leave unconfirmed.
#[skuld::test]
fn stop_and_await_exit_after_an_expired_hold_reports_a_refused_stop() {
    let mut fake = FakeScm::new([])
        .stop_replies([StopReply::Os(ERROR_INVALID_SERVICE_CONTROL)])
        .queried(QueriedState::Running, true);

    let outcome = stop_and_await_exit(
        &mut fake,
        spent(),
        &mut HoldOutcome::<u32>::Expired { pid: 9 },
        false,
        no_end,
    );

    assert!(outcome.is_err());
}

#[skuld::test]
fn a_stop_request_over_a_stop_under_way_is_accepted() {
    let mut fake = FakeScm::new([])
        .stop_replies([StopReply::Os(ERROR_SERVICE_CANNOT_ACCEPT_CTRL)])
        .queried(QueriedState::StopPending, false);
    request_stop(&mut fake).unwrap();
    assert_eq!(fake.log, vec!["control_stop", "query_status"]);
}
/// What a refused STOP is told, by the state the status read after the refusal reports.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Refusal {
    /// A stop is under way (or done): `stop` waits for it, `request_stop` is done.
    UnderWay,
    /// The service was starting when the STOP was sent and accepts STOP now: not delivered, say so.
    NotDelivered,
    /// `RUNNING` or `PAUSED` with no `SERVICE_ACCEPT_STOP`: nothing is stopping it.
    DoesNotAccept,
    /// `START_PENDING`: the STOP was dropped, [`STARTING`].
    Starting,
    /// Any other state: the OS error, code intact.
    Raw,
}

/// A STOP refused with 1052 or 1061 means what the service's status says, not what the code says:
/// no probe measured which code SCM gives a `RUNNING` service that accepts no STOP, and Microsoft
/// documents 1052 for it. Every code-by-state cell is decided by the status read after the refusal.
/// An error is not followed by a wait. A 1061 over `RUNNING` accepting STOP sends the STOP once more
/// (the tests after this one); each script has a second refusal for it.
#[skuld::test]
fn a_refused_stop_is_decided_by_the_status_read_after_it() {
    use QueriedState::{ContinuePending, PausePending, Paused, Running, StartPending, StopPending, Stopped};
    use Refusal::{DoesNotAccept, NotDelivered, Raw, Starting, UnderWay};
    for (code, state, accepts_stop, expected) in [
        (ERROR_SERVICE_CANNOT_ACCEPT_CTRL, StopPending, false, UnderWay),
        (ERROR_SERVICE_CANNOT_ACCEPT_CTRL, Stopped, false, UnderWay),
        (ERROR_SERVICE_CANNOT_ACCEPT_CTRL, Running, true, UnderWay),
        (ERROR_SERVICE_CANNOT_ACCEPT_CTRL, Paused, true, UnderWay),
        (ERROR_SERVICE_CANNOT_ACCEPT_CTRL, Running, false, DoesNotAccept),
        (ERROR_SERVICE_CANNOT_ACCEPT_CTRL, Paused, false, DoesNotAccept),
        (ERROR_SERVICE_CANNOT_ACCEPT_CTRL, StartPending, false, Starting),
        (ERROR_SERVICE_CANNOT_ACCEPT_CTRL, StartPending, true, Starting),
        (ERROR_SERVICE_CANNOT_ACCEPT_CTRL, ContinuePending, false, Raw),
        (ERROR_SERVICE_CANNOT_ACCEPT_CTRL, PausePending, false, Raw),
        (ERROR_INVALID_SERVICE_CONTROL, StopPending, false, UnderWay),
        (ERROR_INVALID_SERVICE_CONTROL, Stopped, false, UnderWay),
        (ERROR_INVALID_SERVICE_CONTROL, Running, true, NotDelivered),
        (ERROR_INVALID_SERVICE_CONTROL, Paused, true, NotDelivered),
        (ERROR_INVALID_SERVICE_CONTROL, Running, false, DoesNotAccept),
        (ERROR_INVALID_SERVICE_CONTROL, Paused, false, DoesNotAccept),
        (ERROR_INVALID_SERVICE_CONTROL, StartPending, false, Starting),
        (ERROR_INVALID_SERVICE_CONTROL, StartPending, true, Starting),
        (ERROR_INVALID_SERVICE_CONTROL, ContinuePending, false, Raw),
        (ERROR_INVALID_SERVICE_CONTROL, PausePending, false, Raw),
    ] {
        let script = || {
            FakeScm::new([Some(Observed::Stopped)])
                .stop_replies([StopReply::Os(code), StopReply::Os(code)])
                .queried(state, accepts_stop)
        };
        let cell = format!("{code} then {state:?} accepting {accepts_stop}");
        let mut waiting = script();
        let mut immediate = script();
        let results = [
            stop_via_notify(&mut waiting, unbounded(), false).map(|waited| {
                assert_eq!(waited, Waited::Confirmed, "{cell}");
            }),
            request_stop(&mut immediate),
        ];
        if expected == UnderWay {
            for result in results {
                result.unwrap_or_else(|e| panic!("{cell}: {e}"));
            }
            assert!(waiting.log.contains(&"arm_stopped"), "{cell}: {:?}", waiting.log);
            continue;
        }
        assert_eq!(waiting.log, vec!["control_stop", "query_status"], "{cell}");
        assert_eq!(immediate.log, vec!["control_stop", "query_status"], "{cell}");
        for result in results {
            assert_told(expected, &result.expect_err(&cell), code, &cell);
        }
    }
}

/// Asserts that `err` says what a refusal of kind `expected` is told. `code` is the refusal's OS code,
/// which only [`Refusal::Raw`] keeps.
fn assert_told(expected: Refusal, err: &std::io::Error, code: i32, cell: &str) {
    let text = err.to_string();
    match expected {
        Refusal::DoesNotAccept => assert!(
            text.contains("does not accept STOP") && text.contains("goetia daemon kill"),
            "{cell}: {text}"
        ),
        Refusal::NotDelivered => {
            for part in ["was not delivered", "accepts STOP now", "run `stop` again"] {
                assert!(text.contains(part), "{cell}: missing {part:?}: {text}");
            }
            assert!(!text.contains("os error"), "{cell}: {text}");
        }
        Refusal::Starting => {
            for part in [
                "SCM is starting the service",
                "was not delivered and is not queued",
                "run `stop` again once the service is running",
            ] {
                assert!(text.contains(part), "{cell}: missing {part:?}: {text}");
            }
            // Not measured: a restart can fail before RUNNING (P9).
            assert!(!text.contains("will come up"), "{cell}: {text}");
            assert!(!text.contains("os error"), "{cell}: {text}");
        }
        Refusal::Raw => assert_eq!(err.raw_os_error(), Some(code), "{cell}: {text}"),
        Refusal::UnderWay => unreachable!(),
    }
}

/// SCM refuses a STOP with 1061 to a service in `START_PENDING` (Microsoft documents it), even one that
/// already accepts STOP. If the service is `RUNNING` by the status read, that STOP was never delivered
/// and nothing is stopping it, so the STOP is sent once more and this one is delivered.
#[skuld::test]
fn a_stop_refused_in_start_pending_is_sent_again_once_running() {
    let script = || {
        FakeScm::new([Some(Observed::Stopped)])
            .stop_replies([StopReply::Os(ERROR_SERVICE_CANNOT_ACCEPT_CTRL), StopReply::Ok])
            .queried(QueriedState::Running, true)
    };
    let mut waiting = script();
    assert_eq!(
        stop_via_notify(&mut waiting, unbounded(), false).unwrap(),
        Waited::Confirmed
    );
    assert_eq!(
        waiting.log,
        vec![
            "control_stop",
            "query_status",
            "control_stop",
            "arm_stopped",
            "wait",
            "got_stopped"
        ]
    );
    let mut immediate = script();
    request_stop(&mut immediate).unwrap();
    assert_eq!(immediate.log, vec!["control_stop", "query_status", "control_stop"]);
}

/// After one delivered STOP, SCM refuses further STOPs with 1061 itself while the service stays
/// `RUNNING` (P16a). The second STOP is refused too, so a STOP is under way: one more is not sent, and
/// the wait goes on until the service stops.
#[skuld::test]
fn a_stop_refused_after_a_delivered_stop_waits_for_it() {
    let script = || {
        FakeScm::new([Some(Observed::Stopped)])
            .stop_replies([
                StopReply::Os(ERROR_SERVICE_CANNOT_ACCEPT_CTRL),
                StopReply::Os(ERROR_SERVICE_CANNOT_ACCEPT_CTRL),
            ])
            .queried(QueriedState::Running, true)
    };
    let mut waiting = script();
    assert_eq!(
        stop_via_notify(&mut waiting, unbounded(), false).unwrap(),
        Waited::Confirmed
    );
    assert_eq!(
        waiting.log,
        vec![
            "control_stop",
            "query_status",
            "control_stop",
            "query_status",
            "arm_stopped",
            "wait",
            "got_stopped"
        ]
    );
    let mut immediate = script();
    request_stop(&mut immediate).unwrap();
    assert_eq!(immediate.stop_count(), 2);
}

/// The resent STOP's refusal is decided by a fresh status read through the table of
/// [`a_refused_stop_is_decided_by_the_status_read_after_it`], with no third STOP: the first read shows
/// `RUNNING` accepting STOP, then the service moves to `state` before the second read.
#[skuld::test]
fn a_resent_stop_that_is_refused_is_decided_by_a_fresh_status_read() {
    use QueriedState::{Paused, Running, StartPending, StopPending, Stopped};
    use Refusal::{DoesNotAccept, NotDelivered, Starting, UnderWay};
    for (code, state, accepts_stop, expected) in [
        (ERROR_SERVICE_CANNOT_ACCEPT_CTRL, StopPending, false, UnderWay),
        (ERROR_SERVICE_CANNOT_ACCEPT_CTRL, Stopped, false, UnderWay),
        (ERROR_SERVICE_CANNOT_ACCEPT_CTRL, Running, true, UnderWay),
        (ERROR_SERVICE_CANNOT_ACCEPT_CTRL, Paused, true, UnderWay),
        (ERROR_SERVICE_CANNOT_ACCEPT_CTRL, Running, false, DoesNotAccept),
        (ERROR_SERVICE_CANNOT_ACCEPT_CTRL, StartPending, false, Starting),
        (ERROR_INVALID_SERVICE_CONTROL, StopPending, false, UnderWay),
        (ERROR_INVALID_SERVICE_CONTROL, Running, true, NotDelivered),
        (ERROR_INVALID_SERVICE_CONTROL, Running, false, DoesNotAccept),
        (ERROR_INVALID_SERVICE_CONTROL, StartPending, false, Starting),
    ] {
        let script = || {
            FakeScm::new([Some(Observed::Stopped)])
                .stop_replies([StopReply::Os(ERROR_SERVICE_CANNOT_ACCEPT_CTRL), StopReply::Os(code)])
                .queried(Running, true)
                .then_queried(state, accepts_stop)
        };
        let cell = format!("1061, then {code} and {state:?} accepting {accepts_stop}");
        let mut waiting = script();
        let mut immediate = script();
        let results = [
            stop_via_notify(&mut waiting, unbounded(), false).map(|waited| {
                assert_eq!(waited, Waited::Confirmed, "{cell}");
            }),
            request_stop(&mut immediate),
        ];
        assert_eq!(waiting.stop_count(), 2, "{cell}: no third STOP: {:?}", waiting.log);
        assert_eq!(immediate.stop_count(), 2, "{cell}: no third STOP: {:?}", immediate.log);
        for result in results {
            if expected == UnderWay {
                result.unwrap_or_else(|e| panic!("{cell}: {e}"));
            } else {
                assert_told(expected, &result.expect_err(&cell), code, &cell);
            }
        }
        assert!(waiting.log.contains(&"arm_stopped") == (expected == UnderWay), "{cell}");
    }
}

/// A resent STOP refused for any other reason is that OS error.
#[skuld::test]
fn a_resent_stop_refused_with_another_code_is_the_os_error() {
    let mut fake = FakeScm::new([Some(Observed::Stopped)])
        .stop_replies([StopReply::Os(ERROR_SERVICE_CANNOT_ACCEPT_CTRL), StopReply::Os(5)])
        .queried(QueriedState::Running, true);
    assert_eq!(
        stop_via_notify(&mut fake, unbounded(), false)
            .unwrap_err()
            .raw_os_error(),
        Some(5)
    );
    assert_eq!(fake.stop_count(), 2);
}

/// Any other refusal is an error that keeps its OS code, without a status read.
#[skuld::test]
fn a_stop_refused_with_another_code_is_the_os_error() {
    let script = || FakeScm::new([Some(Observed::Stopped)]).stop_replies([StopReply::Os(5)]);
    let mut waiting = script();
    let mut immediate = script();
    assert_eq!(
        stop_via_notify(&mut waiting, unbounded(), false)
            .unwrap_err()
            .raw_os_error(),
        Some(5)
    );
    assert_eq!(request_stop(&mut immediate).unwrap_err().raw_os_error(), Some(5));
    assert_eq!(
        waiting.log,
        vec!["control_stop"],
        "nothing is armed for a STOP that was not sent"
    );
}

/// A failure that races the stop goes `STOPPED (1066)`, and SCM restarts the service under its
/// restart policy. If the restart reaches `RUNNING` (or is `START_PENDING`) before the re-arm, no
/// notification will ever come: the re-arm is read, and the service is reported as not stopped.
/// Without the read the model panics with a lost wake-up, where a real wait would hang.
#[skuld::test]
fn a_stop_whose_service_restarted_before_the_re_arm_says_so_instead_of_waiting() {
    for (state, said) in [
        (QueriedState::Running, "is running again"),
        (QueriedState::StartPending, "is starting again"),
    ] {
        let mut fake = FakeScm::new([Some(Observed::Pending)]).drifting_after(1, state);
        let err = stop_via_notify(&mut fake, unbounded(), false).unwrap_err().to_string();
        for part in [said, "after it began to stop", "restart policy", "Run `stop` again."] {
            assert!(err.contains(part), "{state:?}: missing {part:?}: {err}");
        }
        assert_eq!(fake.stop_count(), 1, "{state:?}: no second STOP is sent");
        assert_eq!(
            fake.log,
            vec!["control_stop", "arm_stopped", "wait", "got_pending", "arm_stopped_only"],
            "{state:?}"
        );
    }
}

/// A caller that has seen `STOP_PENDING` already is not waiting for a stop to begin: it arms for
/// `STOPPED` alone, and a first read of anything but `STOP_PENDING` or `STOPPED` is the failure.
#[skuld::test]
fn a_wait_after_a_seen_stop_pending_fails_on_a_first_read_that_is_neither_pending_nor_stopped() {
    for (state, said) in [
        (QueriedState::Running, "is running again"),
        (QueriedState::Paused, "is paused again"),
        (QueriedState::StartPending, "is starting again"),
    ] {
        let mut fake = FakeScm::new([]).starting_in(state);
        let err = wait_after_stop(&mut fake, unbounded(), "", StopSeen::Pending, false)
            .unwrap_err()
            .to_string();
        assert!(err.contains(said), "{state:?}: {err}");
        assert_eq!(fake.log, vec!["arm_stopped_only"], "{state:?}");
    }
}

/// The confirming path: `STOP_PENDING` read at the arm, then `STOPPED` delivered.
#[skuld::test]
fn a_wait_after_a_seen_stop_pending_waits_out_stop_pending_and_confirms_stopped() {
    let mut fake = FakeScm::new([Some(Observed::Stopped)]).starting_in(QueriedState::StopPending);
    assert_eq!(
        wait_after_stop(&mut fake, unbounded(), "", StopSeen::Pending, false).unwrap(),
        Waited::Confirmed
    );
    assert_eq!(fake.log, vec!["arm_stopped_only", "wait", "got_stopped"]);
}

/// The same race when the service has stopped and stayed so: the re-arm reports `STOPPED` at once.
#[skuld::test]
fn a_stop_whose_service_stopped_before_the_re_arm_is_confirmed_by_the_read() {
    let mut fake = FakeScm::new([Some(Observed::Pending)]).drifting_after(1, QueriedState::Stopped);
    assert_eq!(
        stop_via_notify(&mut fake, unbounded(), false).unwrap(),
        Waited::Confirmed
    );
    assert_eq!(
        fake.log,
        vec!["control_stop", "arm_stopped", "wait", "got_pending", "arm_stopped_only"]
    );
}

/// `settle_and_stop` waits with the same loop after its accepted STOP. Its error does not advise
/// `stop`: `uninstall` words its own retry.
#[skuld::test]
fn uninstalls_stop_wait_also_reads_after_its_re_arm() {
    let mut fake = FakeScm::new([Some(Observed::Running), Some(Observed::Pending)])
        .stop_replies([StopReply::Ok])
        .drifting_after(2, QueriedState::Running);
    let err = settle_and_stop(&mut fake, unbounded()).unwrap_err().to_string();
    assert!(
        err.contains("is running again") && !err.contains("`stop` again"),
        "{err}"
    );
}

/// A recovery restart can run between an arm and the STOP: the service fails to `STOPPED` just before
/// the arm, SCM restarts it, and the STOP lands on the new run. A `STOPPED` queued at an arm made before
/// the STOP is not the STOP's answer, and taking it confirms a stop of a running service.
#[skuld::test]
fn a_stop_is_not_confirmed_by_a_stopped_the_service_left_before_the_stop() {
    let mut fake = FakeScm::new([Some(Observed::Stopped)])
        .starting_in(QueriedState::Stopped)
        .shifting_at("control_stop", QueriedState::Running);
    assert_eq!(
        stop_via_notify(&mut fake, unbounded(), false).unwrap(),
        Waited::Confirmed
    );
    assert_eq!(fake.state, Some(QueriedState::Stopped), "confirmed on a stale STOPPED");
    assert_eq!(fake.log, vec!["control_stop", "arm_stopped", "wait", "got_stopped"]);
}

/// The arm follows the STOP, so a service that is already `STOPPED` is seen in the status the arm
/// returns, and nothing waits.
#[skuld::test]
fn a_stop_whose_service_stopped_before_the_arm_is_confirmed_by_the_read() {
    let mut fake = FakeScm::new([]).drifting_after(0, QueriedState::Stopped);
    assert_eq!(
        stop_via_notify(&mut fake, unbounded(), false).unwrap(),
        Waited::Confirmed
    );
    assert_eq!(fake.log, vec!["control_stop", "arm_stopped"]);
}

/// The STOP is issued whatever is left of the budget; only the arm after it is bounded.
#[skuld::test]
fn a_stop_is_issued_even_when_the_arm_after_it_expires() {
    let mut fake = FakeScm::new([]).arming(Waited::Expired);
    assert_eq!(stop_via_notify(&mut fake, spent(), false).unwrap(), Waited::Expired);
    assert_eq!(fake.log, vec!["control_stop", "arm_stopped"]);
}

// settle_and_stop ----

/// [`super::settle_and_stop`] for a caller that has nothing to do before the STOP.
fn settle_and_stop<A: ScmActor>(a: &mut A, deadline: Deadline) -> io::Result<Waited> {
    super::settle_and_stop(a, deadline, false, |_, _| Ok(()))
}

/// The `hold` of a caller that logs it in the fake's own log, so that its place among the steps is
/// what the log says.
fn holding(fake: &mut FakeScm, _: HoldKind) -> io::Result<()> {
    fake.log.push("hold");
    Ok(())
}

/// The caller's `hold` runs once the state is settled and before each STOP, not before the wait for
/// a state to settle: what it holds is the process of the service as it is now. A service that is already
/// stopped has no STOP and no hold.
#[skuld::test]
fn settle_and_stop_runs_the_callers_step_before_each_stop_it_sends() {
    let mut fake = FakeScm::new([Some(Observed::Running), Some(Observed::Stopped)]).stop_replies([StopReply::Ok]);
    assert_eq!(
        super::settle_and_stop(&mut fake, unbounded(), false, holding).unwrap(),
        Waited::Confirmed
    );
    assert_eq!(
        fake.log,
        [
            "arm_settle",
            "wait",
            "got_running",
            "hold",
            "control_stop",
            "arm_stopped",
            "wait",
            "got_stopped"
        ]
    );

    // A refused STOP is sent again after the re-arm, and the process is held again for it: the
    // service may have been restarted in between.
    let mut fake = FakeScm::new([Some(Observed::Running), Some(Observed::Stopped)])
        .stop_replies([StopReply::Os(INVALID_SERVICE_CONTROL), StopReply::Ok]);
    assert_eq!(
        super::settle_and_stop(&mut fake, unbounded(), false, holding).unwrap(),
        Waited::Confirmed
    );
    assert_eq!(
        fake.log,
        [
            "arm_settle",
            "wait",
            "got_running",
            "hold",
            "control_stop",
            "arm_settle",
            "hold",
            "control_stop",
            "arm_stopped",
            "wait",
            "got_stopped"
        ]
    );

    let mut fake = FakeScm::new([Some(Observed::Stopped)]);
    assert_eq!(
        super::settle_and_stop(&mut fake, unbounded(), false, holding).unwrap(),
        Waited::Confirmed
    );
    assert_eq!(fake.log, ["arm_settle", "wait", "got_stopped"]);
}

/// A service found `STOP_PENDING` (stopped by someone else, or by a `stop` whose wait ran out) is
/// held once, before the wait for its `STOPPED`, with no STOP of ours: the process that is exiting is
/// the one the caller must wait for.
#[skuld::test]
fn settle_and_stop_holds_a_service_found_stop_pending_once_before_waiting() {
    let mut fake = FakeScm::new([Some(Observed::Stopped)]).starting_in(QueriedState::StopPending);
    assert_eq!(
        super::settle_and_stop(&mut fake, unbounded(), false, holding).unwrap(),
        Waited::Confirmed
    );
    assert_eq!(fake.log, ["arm_settle", "hold", "wait", "got_stopped"]);
}

/// A hold that fails ends the settle with its error, as for a STOP.
#[skuld::test]
fn settle_and_stop_ends_with_the_error_of_a_hold_of_a_stop_pending_service() {
    let mut fake = FakeScm::new([Some(Observed::Stopped)]).starting_in(QueriedState::StopPending);

    let error =
        super::settle_and_stop(&mut fake, unbounded(), false, |_, _| Err(io::Error::other("no hold"))).unwrap_err();

    assert_eq!(error.to_string(), "no hold");
    assert_eq!(fake.log, ["arm_settle"]);
}

/// A step that fails ends the settle with its error and no STOP: the service is left as it was.
#[skuld::test]
fn settle_and_stop_sends_no_stop_when_the_callers_step_fails() {
    let mut fake = FakeScm::new([Some(Observed::Running)]);

    let error =
        super::settle_and_stop(&mut fake, unbounded(), false, |_, _| Err(io::Error::other("no hold"))).unwrap_err();

    assert_eq!(error.to_string(), "no hold");
    assert_eq!(fake.log, ["arm_settle", "wait", "got_running"]);
}

/// What one SCM interleaving must make [`settle_and_stop`] do.
enum Outcome {
    Confirmed,
    /// An error whose text contains this.
    Fails(&'static str),
}

/// One SCM interleaving: the notifications SCM delivers, whether each accepts STOP, what each STOP
/// is answered, and the steps the loop must take in response.
struct Row {
    name: &'static str,
    waits: Vec<Option<Observed>>,
    accepts: Vec<bool>,
    replies: Vec<StopReply>,
    outcome: Outcome,
    log: Vec<&'static str>,
}

const INVALID_SERVICE_CONTROL: i32 = 1052;
const CANNOT_ACCEPT_CTRL: i32 = 1061;

fn settle_rows() -> Vec<Row> {
    use Observed::{Paused, Pending, Running, Stopped};
    vec![
        Row {
            name: "stopped at once",
            waits: vec![Some(Stopped)],
            accepts: vec![],
            replies: vec![],
            outcome: Outcome::Confirmed,
            log: vec!["arm_settle", "wait", "got_stopped"],
        },
        Row {
            name: "running, the stop is accepted, then stopped",
            waits: vec![Some(Running), Some(Stopped)],
            accepts: vec![],
            replies: vec![StopReply::Ok],
            outcome: Outcome::Confirmed,
            log: vec![
                "arm_settle",
                "wait",
                "got_running",
                "control_stop",
                "arm_stopped",
                "wait",
                "got_stopped",
            ],
        },
        Row {
            name: "paused, the stop is accepted, then stopped",
            waits: vec![Some(Paused), Some(Stopped)],
            accepts: vec![],
            replies: vec![StopReply::Ok],
            outcome: Outcome::Confirmed,
            log: vec![
                "arm_settle",
                "wait",
                "got_paused",
                "control_stop",
                "arm_stopped",
                "wait",
                "got_stopped",
            ],
        },
        Row {
            name: "running, a restart began (1052), running again by the re-arm, the stop is accepted, then stopped",
            waits: vec![Some(Running), Some(Stopped)],
            accepts: vec![],
            replies: vec![StopReply::Os(INVALID_SERVICE_CONTROL), StopReply::Ok],
            outcome: Outcome::Confirmed,
            log: vec![
                "arm_settle",
                "wait",
                "got_running",
                "control_stop",
                "arm_settle",
                "control_stop",
                "arm_stopped",
                "wait",
                "got_stopped",
            ],
        },
        Row {
            name: "running, a stop was already delivered (1061), sent once more and refused again: under way, then stopped",
            waits: vec![Some(Running), Some(Stopped)],
            accepts: vec![],
            replies: vec![StopReply::Os(CANNOT_ACCEPT_CTRL), StopReply::Os(CANNOT_ACCEPT_CTRL)],
            outcome: Outcome::Confirmed,
            log: vec![
                "arm_settle",
                "wait",
                "got_running",
                "control_stop",
                "arm_settle",
                "control_stop",
                "arm_stopped",
                "wait",
                "got_stopped",
            ],
        },
        Row {
            name: "running, 1061 because the service was starting, the STOP sent once more is accepted, then stopped",
            waits: vec![Some(Running), Some(Stopped)],
            accepts: vec![],
            replies: vec![StopReply::Os(CANNOT_ACCEPT_CTRL), StopReply::Ok],
            outcome: Outcome::Confirmed,
            log: vec![
                "arm_settle",
                "wait",
                "got_running",
                "control_stop",
                "arm_settle",
                "control_stop",
                "arm_stopped",
                "wait",
                "got_stopped",
            ],
        },
        Row {
            name: "1052, 1061 and an accepted stop in a row",
            waits: vec![Some(Running), Some(Stopped)],
            accepts: vec![],
            replies: vec![
                StopReply::Os(INVALID_SERVICE_CONTROL),
                StopReply::Os(CANNOT_ACCEPT_CTRL),
                StopReply::Ok,
            ],
            outcome: Outcome::Confirmed,
            log: vec![
                "arm_settle",
                "wait",
                "got_running",
                "control_stop",
                "arm_settle",
                "control_stop",
                "arm_settle",
                "control_stop",
                "arm_stopped",
                "wait",
                "got_stopped",
            ],
        },
        Row {
            name: "running and STOP not accepted",
            waits: vec![Some(Running)],
            accepts: vec![false],
            replies: vec![],
            outcome: Outcome::Fails("does not accept STOP"),
            log: vec!["arm_settle", "wait", "got_running"],
        },
        Row {
            name: "paused and STOP not accepted",
            waits: vec![Some(Paused)],
            accepts: vec![false],
            replies: vec![],
            outcome: Outcome::Fails("does not accept STOP"),
            log: vec!["arm_settle", "wait", "got_paused"],
        },
        Row {
            name: "running, then another error",
            waits: vec![Some(Running)],
            accepts: vec![],
            replies: vec![StopReply::Os(5)],
            outcome: Outcome::Fails("os error 5"),
            log: vec!["arm_settle", "wait", "got_running", "control_stop"],
        },
        Row {
            name: "running, the stop is accepted, stop pending, then stopped",
            waits: vec![Some(Running), Some(Pending), Some(Stopped)],
            accepts: vec![],
            replies: vec![StopReply::Ok],
            outcome: Outcome::Confirmed,
            log: vec![
                "arm_settle",
                "wait",
                "got_running",
                "control_stop",
                "arm_stopped",
                "wait",
                "got_pending",
                "arm_stopped_only",
                "wait",
                "got_stopped",
            ],
        },
        Row {
            name: "a pending notification that accepts STOP is not stopped",
            waits: vec![Some(Pending), Some(Stopped)],
            accepts: vec![true, true],
            replies: vec![],
            outcome: Outcome::Confirmed,
            log: vec!["arm_settle", "wait", "got_pending", "arm_settle", "wait", "got_stopped"],
        },
    ]
}

/// Every interleaving ends, returns `Confirmed` only on a `Stopped` observation, and sends a STOP
/// only on a `Running` or `Paused` notification that accepts it. The fake runs dry, and panics, if
/// the loop asks for a notification the row did not script.
#[skuld::test]
fn settle_and_stop_handles_every_scm_interleaving() {
    for row in settle_rows() {
        let mut fake = FakeScm::new(row.waits.clone())
            .accepting(row.accepts.clone())
            .stop_replies(row.replies.clone());
        let result = settle_and_stop(&mut fake, unbounded());
        match (&row.outcome, result) {
            (Outcome::Confirmed, Ok(Waited::Confirmed)) => {
                assert_eq!(
                    fake.log.last(),
                    Some(&"got_stopped"),
                    "{}: Confirmed without a Stopped observation: {:?}",
                    row.name,
                    fake.log
                );
            }
            (Outcome::Fails(text), Err(e)) => {
                assert!(e.to_string().contains(text), "{}: {e}", row.name);
            }
            (_, other) => panic!("{}: unexpected {other:?}", row.name),
        }
        assert_eq!(fake.log, row.log, "{}", row.name);
        assert!(fake.bad_stops.is_empty(), "{}: {:?}", row.name, fake.bad_stops);
        assert!(fake.waits.is_empty(), "{}: unconsumed notifications", row.name);
    }
}

/// A re-arm on the same handle is not delivered for a state already current at the arm, so each
/// re-arm below is followed by a read that decides before any wait. The fake panics with a lost
/// wake-up where the code would have waited on a registration that cannot fire.
#[skuld::test]
fn settle_and_stop_reads_after_the_re_arm_that_follows_a_pending_notification() {
    use QueriedState::{Running, Stopped};
    // Stopped before the re-arm: confirmed by the read.
    let mut fake = FakeScm::new([Some(Observed::Pending)]).drifting_after(1, Stopped);
    assert_eq!(settle_and_stop(&mut fake, unbounded()).unwrap(), Waited::Confirmed);
    assert_eq!(fake.log, vec!["arm_settle", "wait", "got_pending", "arm_settle"]);

    // Running before the re-arm: the STOP is sent as if it had been notified, and the service is
    // STOPPED by the arm after it.
    let mut fake = FakeScm::new([Some(Observed::Pending)])
        .stop_replies([StopReply::Ok])
        .drifting_after(1, Running)
        .drifting_after(1, Stopped);
    assert_eq!(settle_and_stop(&mut fake, unbounded()).unwrap(), Waited::Confirmed);
    assert_eq!(
        fake.log,
        vec![
            "arm_settle",
            "wait",
            "got_pending",
            "arm_settle",
            "control_stop",
            "arm_stopped"
        ]
    );
    assert!(fake.bad_stops.is_empty(), "{:?}", fake.bad_stops);
}

/// After a 1052 the restart that began may be RUNNING again before the re-arm: the read says so and
/// the STOP is sent as for a notified RUNNING.
#[skuld::test]
fn settle_and_stop_sends_the_stop_again_when_the_read_after_a_1052_shows_running() {
    let mut fake = FakeScm::new([Some(Observed::Running), Some(Observed::Stopped)])
        .stop_replies([StopReply::Os(INVALID_SERVICE_CONTROL), StopReply::Ok]);
    assert_eq!(settle_and_stop(&mut fake, unbounded()).unwrap(), Waited::Confirmed);
    assert_eq!(fake.stop_count(), 2);
    assert!(fake.bad_stops.is_empty(), "{:?}", fake.bad_stops);
}

/// The service may be STOPPED before the re-arm that follows a refused STOP, whichever code refused.
#[skuld::test]
fn settle_and_stop_is_confirmed_by_a_read_of_stopped_after_a_refused_stop() {
    for code in [INVALID_SERVICE_CONTROL, CANNOT_ACCEPT_CTRL] {
        let mut fake = FakeScm::new([Some(Observed::Running)])
            .stop_replies([StopReply::Os(code)])
            .drifting_after(1, QueriedState::Stopped);
        assert_eq!(
            settle_and_stop(&mut fake, unbounded()).unwrap(),
            Waited::Confirmed,
            "{code}"
        );
        assert_eq!(
            fake.log,
            vec!["arm_settle", "wait", "got_running", "control_stop", "arm_settle"],
            "{code}"
        );
    }
}

/// A STOP the SCM accepted can be followed by `STOPPED` before the arm that waits for it: the shim
/// finishes its teardown first. The read after that arm confirms it.
#[skuld::test]
fn settle_and_stop_is_confirmed_by_a_read_of_stopped_after_an_accepted_stop() {
    let mut fake = FakeScm::new([Some(Observed::Running)])
        .stop_replies([StopReply::Ok])
        .drifting_after(1, QueriedState::Stopped);
    assert_eq!(settle_and_stop(&mut fake, unbounded()).unwrap(), Waited::Confirmed);
    assert_eq!(
        fake.log,
        vec!["arm_settle", "wait", "got_running", "control_stop", "arm_stopped"]
    );
}

#[skuld::test]
fn settle_and_stop_names_the_state_that_refuses_a_stop() {
    let mut fake = FakeScm::new([Some(Observed::Paused)]).accepting([false]);
    let err = settle_and_stop(&mut fake, unbounded()).unwrap_err().to_string();
    assert!(err.contains("PAUSED"), "{err}");
}

/// A bounded wait that runs out is an expiry, never a confirmation.
#[skuld::test]
fn settle_and_stop_reports_each_kind_of_expiry() {
    let mut fake = FakeScm::new([]).arming(Waited::Expired);
    assert_eq!(settle_and_stop(&mut fake, spent()).unwrap(), Waited::Expired);
    assert_eq!(fake.log, vec!["arm_settle"]);

    let mut fake = FakeScm::new([None]);
    assert_eq!(settle_and_stop(&mut fake, spent()).unwrap(), Waited::Expired);

    let mut fake = FakeScm::new([Some(Observed::Running), None]);
    assert_eq!(settle_and_stop(&mut fake, spent()).unwrap(), Waited::Expired);
    assert_eq!(fake.stop_count(), 1);
}

#[skuld::test]
fn start_via_notify_starts_then_arms_running_and_gates_on_running() {
    let mut fake = FakeScm::new([Some(Observed::Running)]);
    assert_eq!(
        start_via_notify(&mut fake, unbounded(), Kind::Simple).unwrap(),
        Waited::Confirmed
    );
    assert_eq!(fake.log, vec!["start", "arm_settle", "wait", "got_running"]);
}

/// A `RUNNING` current before the start is not the start's answer: the arm follows the start, so it
/// queues nothing for the old run, and what confirms is the notification the new run delivers.
#[skuld::test]
fn a_start_is_not_confirmed_by_a_running_the_service_left_before_the_start() {
    let mut fake = FakeScm::new([Some(Observed::Running)])
        .starting_in(QueriedState::Running)
        .shifting_at("start", QueriedState::Stopped);
    assert_eq!(
        start_via_notify(&mut fake, unbounded(), Kind::Simple).unwrap(),
        Waited::Confirmed
    );
    assert!(
        fake.waits.is_empty(),
        "the scripted RUNNING of the new run is what confirmed"
    );
    assert_eq!(fake.log, vec!["start", "arm_settle", "wait", "got_running"]);
}

/// The arm follows the start, so a service that is `RUNNING` by then is seen in the status the arm
/// returns and nothing waits.
#[skuld::test]
fn a_start_whose_service_is_running_before_the_arm_is_confirmed_by_the_read() {
    let mut fake = FakeScm::new([]).drifting_after(0, QueriedState::Running);
    assert_eq!(
        start_via_notify(&mut fake, unbounded(), Kind::Simple).unwrap(),
        Waited::Confirmed
    );
    assert_eq!(fake.log, vec!["start", "arm_settle"]);
}

/// A service that stopped before the arm is judged like a notified `STOPPED`, with the exit record and
/// wait hint the read carries.
#[skuld::test]
fn a_start_whose_service_stopped_before_the_arm_is_judged_by_the_read() {
    type Script = fn(FakeScm) -> FakeScm;
    let cases: [(&str, Kind, Script, Option<&str>); 6] = [
        (
            "daemon ended",
            Kind::Simple,
            |f| f.stopped_after(Reported::from_daemon_code(7)),
            None,
        ),
        (
            "shim failed",
            Kind::Simple,
            |f| f.stopped_after(Reported::shim_failure(Some(2))),
            Some("exit code 2"),
        ),
        (
            "incomplete stop",
            Kind::Simple,
            |f| f.stopped_after(Reported::incomplete_stop()),
            Some("may still be running"),
        ),
        (
            "refusal",
            Kind::Simple,
            |f| f.stopped_after(Reported::Refused),
            Some("no daemon"),
        ),
        (
            "SCM's failed start, joined",
            Kind::Simple,
            |f| {
                f.already_running(QueriedState::StartPending)
                    .stopped_by_scm_with_wait_hint(2000)
            },
            Some("the shim never connected to SCM"),
        ),
        (
            "managed",
            Kind::Managed,
            |f| f.stopped_after(Reported::from_daemon_code(7)),
            Some("failed start"),
        ),
    ];
    for (name, kind, script, error) in cases {
        let mut fake = script(FakeScm::new([]).drifting_after(0, QueriedState::Stopped));
        let result = start_via_notify(&mut fake, unbounded(), kind);
        match error {
            None => assert_eq!(result.unwrap(), Waited::Confirmed, "{name}"),
            Some(text) => {
                let err = result.unwrap_err().to_string();
                assert!(err.contains("failed start") && err.contains(text), "{name}: {err}");
            }
        }
        assert_eq!(fake.log, vec!["start", "arm_settle"], "{name}");
    }
}

#[skuld::test]
fn a_terminal_stopped_after_start_is_still_an_error_not_an_expiry() {
    // A service that stopped instead of reaching Running is a *failed
    // start*, not a wait that ran out of budget — the caller's remedy
    // differs, so the two must not collapse into one another.
    let mut fake = FakeScm::new([Some(Observed::Stopped)]);
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

/// The shim's `(0, SHIM_REFUSED)` says it refused before it started a daemon: a failed start that says
/// so, never one that claims a daemon ran or may still be running.
#[skuld::test]
fn a_refusal_record_is_a_failed_start_with_no_daemon() {
    for joined in [None, Some(QueriedState::StartPending)] {
        let mut fake = FakeScm::new([Some(Observed::Stopped)]).stopped_after(Reported::Refused);
        if let Some(queried) = joined {
            fake = fake.already_running(queried);
        }

        let err = start_via_notify(&mut fake, unbounded(), Kind::Simple).unwrap_err();

        let message = err.to_string();
        assert!(message.contains("failed start"), "{joined:?}: {message}");
        assert!(message.contains("refused"), "{joined:?}: {message}");
        assert!(message.contains("no daemon"), "{joined:?}: {message}");
        assert!(!message.contains("may still be running"), "{joined:?}: {message}");
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
/// wait hint of 2000, and the shim never connected to SCM.
#[skuld::test]
fn a_failed_scm_recovery_start_that_a_start_joined_is_a_failed_start() {
    let mut fake = FakeScm::new([Some(Observed::Stopped)])
        .already_running(QueriedState::StartPending)
        .stopped_by_scm_with_wait_hint(2000);

    let err = start_via_notify(&mut fake, unbounded(), Kind::Simple).unwrap_err();

    let message = err.to_string();
    assert!(message.contains("failed start"), "{message}");
    assert!(
        message.contains("the shim never connected to SCM, so no daemon ran"),
        "{message}"
    );
}

/// A managed service's own host ran, so its `STOPPED (0, 0)` with a leftover wait hint is a failed
/// start, but not one where "the shim never connected to SCM".
#[skuld::test]
fn a_managed_stopped_with_a_wait_hint_that_a_start_joined_does_not_claim_nothing_ran() {
    let mut fake = FakeScm::new([Some(Observed::Stopped)])
        .already_running(QueriedState::StartPending)
        .stopped_by_scm_with_wait_hint(2000);

    let err = start_via_notify(&mut fake, unbounded(), Kind::Managed).unwrap_err();

    let message = err.to_string();
    assert!(message.contains("failed start"), "{message}");
    assert!(!message.contains("never connected to SCM"), "{message}");
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
        Reported::Refused,
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

/// `PAUSED` is a settled state that is not `RUNNING`: the start fails naming it instead of waiting for a
/// `RUNNING` that a paused service does not reach.
#[skuld::test]
fn a_start_whose_service_settles_paused_fails_naming_the_state() {
    let mut fake = FakeScm::new([Some(Observed::Paused)]);
    let err = start_via_notify(&mut fake, unbounded(), Kind::Managed).unwrap_err();
    let text = err.to_string();
    assert!(
        text.contains("paused, not running") && text.contains("`sc continue`"),
        "{text}"
    );
    assert!(fake.mask.contains(&QueriedState::Paused), "{:?}", fake.mask);
    assert_eq!(
        fake.log,
        vec!["start", "arm_settle", "wait", "got_paused", "query_status"]
    );
}

#[skuld::test]
fn a_failed_start_names_the_recorded_code() {
    let mut fake = FakeScm::new([Some(Observed::Stopped)]).stopped_after(Reported::shim_failure(Some(2)));
    let err = start_via_notify(&mut fake, unbounded(), Kind::Simple).unwrap_err();
    assert!(err.to_string().contains("exit code 2"), "{err}");
}

#[skuld::test]
fn a_stop_wait_whose_deadline_expires_reports_expired() {
    let mut fake = FakeScm::new([Some(Observed::Pending), None]);
    assert_eq!(stop_via_notify(&mut fake, spent(), false).unwrap(), Waited::Expired);
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
    assert_eq!(fake.log, vec!["start", "arm_settle", "wait", "got_nothing"]);
}

/// A deadline no re-derivation inside the wait can reproduce: an hour out,
/// where a fresh `Budget::DEFAULT.start()` (or any other budget the code
/// could reach for) lands somewhere else.
fn an_hour_out() -> Deadline {
    Budget::Bounded(std::time::Duration::from_secs(3600)).start()
}

/// A stop's re-arm keeps the caller's deadline: a service that chatters STOP_PENDING forever ends at the
/// caller's budget.
#[skuld::test]
fn a_pending_stop_callback_rearms_under_the_same_deadline() {
    let deadline = an_hour_out();
    let mut fake = FakeScm::new([Some(Observed::Pending), Some(Observed::Stopped)]);
    assert_eq!(stop_via_notify(&mut fake, deadline, false).unwrap(), Waited::Confirmed);
    assert_eq!(fake.arm_count(), 2);
    assert_eq!(fake.arm_deadlines, vec![deadline; 2]);
    assert_eq!(fake.deadlines, vec![deadline; 2]);
}

/// The start's arm and its wait are bounded by the caller's deadline, not a fresh one.
#[skuld::test]
fn a_start_arms_and_waits_under_the_callers_deadline() {
    let deadline = an_hour_out();
    let mut fake = FakeScm::new([Some(Observed::Running)]);
    assert_eq!(
        start_via_notify(&mut fake, deadline, Kind::Simple).unwrap(),
        Waited::Confirmed
    );
    assert_eq!(fake.arm_deadlines, vec![deadline]);
    assert_eq!(fake.deadlines, vec![deadline]);
}

/// What `wait_until` hands its closure: each state off the target, with whether a notification of
/// this wait was already delivered.
#[skuld::test]
fn wait_until_tells_the_closure_whether_a_pending_state_was_delivered() {
    let mut fake = FakeScm::new([Some(Observed::Pending)])
        .starting_in(QueriedState::Running)
        .drifting_after(1, QueriedState::Running);
    let mut seen = vec![];
    let waited = wait_until(
        &mut fake,
        WantState::Stopped,
        WantState::StoppedOnly,
        QueriedState::Stopped,
        unbounded(),
        |sighting| {
            seen.push((sighting.status.state, sighting.pending_delivered));
            Ok(if sighting.pending_delivered {
                ControlFlow::Break(Waited::Expired)
            } else {
                ControlFlow::Continue(())
            })
        },
    )
    .unwrap();
    assert_eq!(waited, Waited::Expired);
    assert_eq!(
        seen,
        vec![(QueriedState::Running, false), (QueriedState::Running, true)]
    );
    assert_eq!(fake.log, vec!["arm_stopped", "wait", "got_pending", "arm_stopped_only"]);
}

/// A `Stopped` notification off the target carries the status of that notification: its record, wait
/// hint and controls accepted, which the arm's status (`START_PENDING`, no record) does not.
#[skuld::test]
fn wait_until_hands_a_notified_stopped_to_the_closure_with_its_record() {
    let mut fake = FakeScm::new([Some(Observed::Stopped)])
        .queried(QueriedState::StartPending, false)
        .stopped_by_scm_with_wait_hint(2000)
        .accepting([false]);
    fake.recorded = RecordedExit {
        win32: 1066,
        specific: 42,
    };
    let mut seen = vec![];
    let waited = wait_until(
        &mut fake,
        WantState::RunningPausedOrStopped,
        WantState::RunningPausedOrStopped,
        QueriedState::Running,
        unbounded(),
        |sighting| {
            seen.push(sighting);
            Ok(match sighting.status.state {
                QueriedState::Stopped => ControlFlow::Break(Waited::Expired),
                _ => ControlFlow::Continue(()),
            })
        },
    )
    .unwrap();
    assert_eq!(waited, Waited::Expired);
    let stopped = seen.last().copied().unwrap();
    assert_eq!(
        stopped.status,
        QueriedStatus {
            state: QueriedState::Stopped,
            accepts_stop: false,
            exit: RecordedExit {
                win32: 1066,
                specific: 42,
            },
            wait_hint: 2000,
        }
    );
    assert!(!stopped.pending_delivered);
}

/// A settled state off the target that the closure lets pass is waited out like a pending one: the
/// wait re-arms with `rearm`, and the arm's status is decided before the wait.
#[skuld::test]
fn wait_until_rearms_with_its_second_mask_after_a_notification_the_closure_lets_pass() {
    let mut fake =
        FakeScm::new([Some(Observed::Paused), Some(Observed::Stopped)]).queried(QueriedState::StartPending, false);
    let mut seen = vec![];
    let waited = wait_until(
        &mut fake,
        WantState::RunningPausedOrStopped,
        WantState::StoppedOnly,
        QueriedState::Stopped,
        unbounded(),
        |sighting| {
            seen.push((sighting.status.state, sighting.pending_delivered));
            Ok(ControlFlow::Continue(()))
        },
    )
    .unwrap();
    assert_eq!(waited, Waited::Confirmed);
    assert_eq!(
        seen,
        vec![
            (QueriedState::StartPending, false),
            (QueriedState::Paused, false),
            (QueriedState::Paused, true),
        ]
    );
    assert_eq!(
        fake.log,
        vec![
            "arm_settle",
            "wait",
            "got_paused",
            "query_status",
            "arm_stopped_only",
            "wait",
            "got_stopped"
        ]
    );
}

/// Under the settled mask, a notified `Stopped` is off the target `Running`: the closure lets it pass,
/// and the wait re-arms with the same mask and decides on that arm's status before it waits.
#[skuld::test]
fn wait_until_rearms_after_a_notified_state_the_closure_lets_pass_under_the_settled_mask() {
    let mut fake =
        FakeScm::new([Some(Observed::Stopped), Some(Observed::Running)]).queried(QueriedState::StartPending, false);
    let mut seen = vec![];
    let waited = wait_until(
        &mut fake,
        WantState::RunningPausedOrStopped,
        WantState::RunningPausedOrStopped,
        QueriedState::Running,
        unbounded(),
        |sighting| {
            seen.push((sighting.status.state, sighting.pending_delivered));
            Ok(ControlFlow::Continue(()))
        },
    )
    .unwrap();
    assert_eq!(waited, Waited::Confirmed);
    assert_eq!(
        seen,
        vec![
            (QueriedState::StartPending, false),
            (QueriedState::Stopped, false),
            (QueriedState::Stopped, true),
        ]
    );
    assert_eq!(
        fake.log,
        vec!["arm_settle", "wait", "got_stopped", "arm_settle", "wait", "got_running"]
    );
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
    assert_eq!(fake.log, vec!["start", "arm_settle"]);
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

// a service marked for deletion ---------------------------------------------------------------------------------------

const STOPPED_ONLY: &[QueriedState] = &[QueriedState::Stopped];
const RUNNING_ONLY: &[QueriedState] = &[QueriedState::Running];

/// Round 8: an arm on a delete-marked, `STOPPED` service is refused with 1072. The service is where
/// every stop wants it, so each arm site ends its wait as confirmed on the status read in the arm's
/// place.
#[skuld::test]
fn a_stop_wait_is_confirmed_by_the_status_of_an_arm_refused_on_a_marked_service() {
    // The arm after the STOP.
    let mut fake = FakeScm::new([])
        .drifting_after(0, QueriedState::Stopped)
        .refusing_arms_in(STOPPED_ONLY);
    assert_eq!(
        stop_via_notify(&mut fake, unbounded(), false).unwrap(),
        Waited::Confirmed
    );
    assert_eq!(fake.log, vec!["control_stop", "arm_stopped"]);

    // The re-arm after a `STOP_PENDING` delivery.
    let mut fake = FakeScm::new([Some(Observed::Pending)])
        .drifting_after(1, QueriedState::Stopped)
        .refusing_arms_in(STOPPED_ONLY);
    assert_eq!(
        stop_via_notify(&mut fake, unbounded(), false).unwrap(),
        Waited::Confirmed
    );
    assert_eq!(
        fake.log,
        vec!["control_stop", "arm_stopped", "wait", "got_pending", "arm_stopped_only"]
    );
}

#[skuld::test]
fn settle_and_stop_is_confirmed_by_the_status_of_an_arm_refused_on_a_marked_service() {
    // The first arm.
    let mut fake = FakeScm::new([])
        .starting_in(QueriedState::Stopped)
        .refusing_arms_in(STOPPED_ONLY);
    assert_eq!(settle_and_stop(&mut fake, unbounded()).unwrap(), Waited::Confirmed);
    assert_eq!(fake.log, vec!["arm_settle"]);

    // The re-arm after a notification outside the mask.
    let mut fake = FakeScm::new([Some(Observed::Pending)])
        .drifting_after(1, QueriedState::Stopped)
        .refusing_arms_in(STOPPED_ONLY);
    assert_eq!(settle_and_stop(&mut fake, unbounded()).unwrap(), Waited::Confirmed);
    assert_eq!(fake.log, vec!["arm_settle", "wait", "got_pending", "arm_settle"]);

    // The re-arm after a refused STOP.
    let mut fake = FakeScm::new([Some(Observed::Running)])
        .stop_replies([StopReply::Os(INVALID_SERVICE_CONTROL)])
        .drifting_after(1, QueriedState::Stopped)
        .refusing_arms_in(STOPPED_ONLY);
    assert_eq!(settle_and_stop(&mut fake, unbounded()).unwrap(), Waited::Confirmed);
    assert_eq!(
        fake.log,
        vec!["arm_settle", "wait", "got_running", "control_stop", "arm_settle"]
    );

    // The arm after an accepted STOP.
    let mut fake = FakeScm::new([Some(Observed::Running)])
        .drifting_after(1, QueriedState::Stopped)
        .refusing_arms_in(STOPPED_ONLY);
    assert_eq!(settle_and_stop(&mut fake, unbounded()).unwrap(), Waited::Confirmed);
    assert_eq!(
        fake.log,
        vec!["arm_settle", "wait", "got_running", "control_stop", "arm_stopped"]
    );
}

/// Any state but the wanted one, read where SCM refused to watch, ends the wait with an error that
/// says why nothing is waited for. Round 8 measured the refusal only in `STOPPED`; the other states
/// are modelled, and the answer is never a wait on a registration SCM refused.
#[skuld::test]
fn an_arm_refused_on_a_marked_service_that_is_not_where_the_wait_wants_it_is_an_error() {
    let mut stop = FakeScm::new([])
        .starting_in(QueriedState::Running)
        .refusing_arms_in(RUNNING_ONLY);
    let mut start = FakeScm::new([])
        .drifting_after(0, QueriedState::Stopped)
        .refusing_arms_in(STOPPED_ONLY);
    let mut settle = FakeScm::new([Some(Observed::Pending)])
        .drifting_after(1, QueriedState::Running)
        .refusing_arms_in(RUNNING_ONLY);
    let errors = [
        ("stop", stop_via_notify(&mut stop, unbounded(), false).unwrap_err()),
        (
            "start",
            start_via_notify(&mut start, unbounded(), Kind::Simple).unwrap_err(),
        ),
        ("settle", settle_and_stop(&mut settle, unbounded()).unwrap_err()),
    ];
    for (name, err) in errors {
        let text = err.to_string();
        for part in ["marked for deletion", "refused to watch"] {
            assert!(text.contains(part), "{name}: missing {part:?}: {text}");
        }
    }
}

/// A start wants `RUNNING`: a marked service read as that is started.
#[skuld::test]
fn a_start_wait_is_confirmed_by_the_running_status_of_an_arm_refused_on_a_marked_service() {
    let mut fake = FakeScm::new([])
        .drifting_after(0, QueriedState::Running)
        .refusing_arms_in(RUNNING_ONLY);
    assert_eq!(
        start_via_notify(&mut fake, unbounded(), Kind::Simple).unwrap(),
        Waited::Confirmed
    );
    assert_eq!(fake.log, vec!["start", "arm_settle"]);
}

#[skuld::test]
fn the_error_for_a_marked_service_names_the_state_read() {
    let text = marked_for_deletion(QueriedState::StopPending).to_string();
    assert!(
        text.contains("refused to watch") && text.contains("marked for deletion") && text.contains("StopPending"),
        "{text}"
    );
    assert!(!text.contains("no longer reports"), "{text}");
}

/// A cancel that finds the service deleted (the open of its fresh handle met 1060) ends a stop wait as
/// confirmed, as the delete that follows would.
#[skuld::test]
fn a_stop_wait_reads_a_service_gone_at_an_arm_as_stopped() {
    let mut fake = FakeScm::new([Some(Observed::Pending)]).gone_at_arm(2);
    assert_eq!(
        stop_via_notify(&mut fake, unbounded(), false).unwrap(),
        Waited::Confirmed
    );
    assert_eq!(
        fake.log,
        vec!["control_stop", "arm_stopped", "wait", "got_pending", "arm_stopped_only"]
    );

    let mut fake = FakeScm::new([Some(Observed::Pending)]).gone_at_arm(2);
    assert_eq!(settle_and_stop(&mut fake, unbounded()).unwrap(), Waited::Confirmed);
    assert_eq!(fake.log, vec!["arm_settle", "wait", "got_pending", "arm_settle"]);
}

/// A start is for `RUNNING`: a service that is gone is not that.
#[skuld::test]
fn a_start_wait_reads_a_service_gone_at_an_arm_as_an_error() {
    let mut fake = FakeScm::new([]).gone_at_arm(1);
    let text = start_via_notify(&mut fake, unbounded(), Kind::Simple)
        .unwrap_err()
        .to_string();
    assert!(text.contains("no longer exists"), "{text}");
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

/// A service marked for deletion, held by the actor's handles; it is deleted when the last closes.
struct FakeSession {
    log: Vec<&'static str>,
    /// Open handles to the service: the actor's own before the cancel.
    open: usize,
    /// Whether closing the last handle deletes the service, as for a marked, `STOPPED` one (P12a).
    marked_stopped: bool,
    deleted: bool,
    /// The error opening a fresh handle meets regardless of the service, if any.
    open_error: Option<i32>,
}

impl FakeSession {
    fn new(marked_stopped: bool) -> Self {
        Self {
            log: vec![],
            open: 1,
            marked_stopped,
            deleted: false,
            open_error: None,
        }
    }
}

impl ScmHandles for FakeSession {
    fn close_handles(&mut self) {
        self.log.push("close");
        self.open -= 1;
        if self.open == 0 && self.marked_stopped {
            self.deleted = true;
        }
    }
    fn registration_outstanding(&self) -> bool {
        true
    }
    fn drain_queued_apc(&mut self) {
        self.log.push("drain");
    }
}

impl ReopenHandles for FakeSession {
    type Fresh = ();
    fn open_fresh(&mut self) -> std::io::Result<()> {
        if self.deleted {
            self.log.push("open_failed");
            return Err(std::io::Error::from_raw_os_error(1060));
        }
        if let Some(code) = self.open_error {
            self.log.push("open_failed");
            return Err(std::io::Error::from_raw_os_error(code));
        }
        self.log.push("open");
        self.open += 1;
        Ok(())
    }
    fn install(&mut self, (): ()) {
        self.log.push("install");
    }
}

/// P12a, P13a/b: closing the last handle to a marked, `STOPPED` service deletes it, so the fresh handle
/// is opened while the old one still holds the service. Closing first fails here with 1060.
#[skuld::test]
fn cancelling_a_registration_opens_the_fresh_handles_before_it_closes_the_old() {
    let mut fake = FakeSession::new(true);
    assert_eq!(cancel_registration(&mut fake).unwrap(), Cancelled::Reopened);
    assert!(!fake.deleted, "the cancel deleted the service");
    assert_eq!(fake.open, 1);
    // The old handles still close before the drain (`close_then_drain`).
    assert_eq!(fake.log, vec!["open", "close", "drain", "install"]);
}

/// Opening the fresh handle meets 1060: the service is gone. The old handles are closed and drained,
/// nothing is installed, and the caller is told.
#[skuld::test]
fn a_cancel_whose_fresh_open_meets_1060_reports_the_service_gone() {
    let mut fake = FakeSession::new(false);
    fake.open_error = Some(1060);
    assert_eq!(cancel_registration(&mut fake).unwrap(), Cancelled::ServiceGone);
    assert_eq!(fake.log, vec!["open_failed", "close", "drain"]);
}

/// Any other error leaves the old handles and their registration as they were, for the actor's own
/// teardown to close and drain.
#[skuld::test]
fn a_cancel_whose_fresh_open_fails_otherwise_touches_nothing() {
    let mut fake = FakeSession::new(false);
    fake.open_error = Some(5); // ERROR_ACCESS_DENIED
    let err = cancel_registration(&mut fake).unwrap_err();
    assert_eq!(err.raw_os_error(), Some(5));
    assert_eq!(fake.log, vec!["open_failed"]);
    assert_eq!(fake.open, 1);
}

// the already-running (1056) decision ---------------------------------------------------------------------------------

#[skuld::test]
fn a_queried_running_recovers_an_already_running_start() {
    // The service is up: 1056 here is the goal reached.
    assert!(already_running_is_recoverable(QueriedState::Running));
}

#[skuld::test]
fn a_queried_start_continue_or_pause_pending_recovers_an_already_running_start() {
    // A service something else started, resumed or is pausing is not settled, and the wait resolves
    // it: a failing pause returns to `Running`. Rejecting it failed a start that was going to
    // succeed — narrowing this predicate back to `Running`-only fails here.
    for state in [
        QueriedState::StartPending,
        QueriedState::ContinuePending,
        QueriedState::PausePending,
    ] {
        assert!(already_running_is_recoverable(state), "{state:?}");
    }
}

#[skuld::test]
fn no_other_queried_state_recovers_an_already_running_start() {
    // None of these is on its way to `Running` or unsettled, so treating 1056 as "the wait will
    // complete" would report a start that did not happen instead of what the service is actually doing.
    for state in [QueriedState::Stopped, QueriedState::StopPending, QueriedState::Paused] {
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
    for deadline in [spent(), unbounded()] {
        let mut fake = FakeScm::new([]).already_running(QueriedState::Running);
        assert_eq!(
            start_via_notify(&mut fake, deadline, Kind::Simple).unwrap(),
            Waited::Confirmed
        );
        assert_eq!(fake.log, vec!["start"], "nothing left to wait for");
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

/// Whatever the budget, a `start` over a stopping service is never a success, and the error says
/// what to do: the service is stopping, SCM may start it again itself, and `start` is run again once
/// it has stopped.
#[skuld::test]
fn a_start_over_a_stopping_service_says_it_is_stopping() {
    let mut fake = FakeScm::new([]).already_running(QueriedState::StopPending);
    let waiting = start_via_notify(&mut fake, unbounded(), Kind::Simple)
        .unwrap_err()
        .to_string();
    assert_eq!(fake.log, vec!["start"], "nothing is waited for");
    let immediate = request_start(&mut FakeScm::new([]).already_running(QueriedState::StopPending))
        .unwrap_err()
        .to_string();
    for text in [waiting, immediate] {
        assert!(text.contains("is stopping"), "{text}");
        assert!(text.contains("once it has stopped"), "{text}");
        assert!(text.contains("restart policy"), "{text}");
        assert!(!text.contains("stay down"), "{text}");
    }
}

/// The first arm queues a delivery of the state at registration, and its status is read after: a service
/// that was `PAUSED` at registration and is resuming at the read is not a paused one, so the delivery
/// alone does not fail the start. The state now decides.
#[skuld::test]
fn a_start_is_not_failed_by_a_paused_the_service_left_before_it_was_delivered() {
    let mut fake = FakeScm::new([])
        .starting_in(QueriedState::Stopped)
        .shifting_at("start", QueriedState::Paused)
        .shifting_at("arm", QueriedState::ContinuePending)
        .shifting_at("wait", QueriedState::Running);
    assert_eq!(
        start_via_notify(&mut fake, unbounded(), Kind::Managed).unwrap(),
        Waited::Confirmed
    );
    assert_eq!(
        fake.log,
        vec!["start", "arm_settle", "wait", "got_paused", "query_status"]
    );
}

/// A delivered `PAUSED` whose read finds the service resuming is not decided: the wait re-arms under the
/// same deadline, and the `RUNNING` that follows confirms.
#[skuld::test]
fn a_delivered_paused_whose_read_is_pending_keeps_the_start_waiting() {
    let deadline = an_hour_out();
    let mut fake = FakeScm::new([Some(Observed::Running)])
        .starting_in(QueriedState::Stopped)
        .shifting_at("start", QueriedState::Paused)
        .shifting_at("arm", QueriedState::ContinuePending);
    assert_eq!(
        start_via_notify(&mut fake, deadline, Kind::Managed).unwrap(),
        Waited::Confirmed
    );
    assert_eq!(
        fake.log,
        vec![
            "start",
            "arm_settle",
            "wait",
            "got_paused",
            "query_status",
            "arm_settle",
            "wait",
            "got_running"
        ]
    );
    assert_eq!(fake.arm_deadlines, vec![deadline; 2]);
    assert_eq!(fake.deadlines, vec![deadline; 2]);
}

/// A delivered `PAUSED` whose read finds the service `STOPPED` is judged as a `STOPPED` is, by `kind`.
#[skuld::test]
fn a_delivered_paused_whose_read_is_stopped_is_judged_by_kind() {
    let mut fake = FakeScm::new([])
        .starting_in(QueriedState::Stopped)
        .shifting_at("start", QueriedState::Paused)
        .shifting_at("arm", QueriedState::ContinuePending)
        .shifting_at("wait", QueriedState::Stopped)
        .stopped_after(Reported::from_daemon_code(7));
    let err = start_via_notify(&mut fake, unbounded(), Kind::Managed).unwrap_err();
    assert!(err.to_string().contains("failed start"), "{err}");
    assert_eq!(
        fake.log,
        vec!["start", "arm_settle", "wait", "got_paused", "query_status"]
    );
}

/// A 1056 over a service that is resuming or pausing is on its way to a settled state: the start waits
/// for it. `RUNNING` confirms, and `PAUSED` is told as paused. The request-only start accepts it, as it
/// does a starting one.
#[skuld::test]
fn a_start_over_a_resuming_or_pausing_service_waits_for_what_it_settles_in() {
    for queried in [QueriedState::ContinuePending, QueriedState::PausePending] {
        let mut fake = FakeScm::new([Some(Observed::Running)]).already_running(queried);
        assert_eq!(
            start_via_notify(&mut fake, unbounded(), Kind::Managed).unwrap(),
            Waited::Confirmed,
            "{queried:?}"
        );
        assert_eq!(
            fake.log,
            vec!["start", "arm_settle", "wait", "got_running"],
            "{queried:?}"
        );

        let mut fake = FakeScm::new([Some(Observed::Paused)]).already_running(queried);
        let err = start_via_notify(&mut fake, unbounded(), Kind::Managed).unwrap_err();
        assert_eq!(err.to_string(), PAUSED, "{queried:?}");

        request_start(&mut FakeScm::new([]).already_running(queried)).unwrap();
    }
}

/// A start over a paused service says the same whether the pause came before the start (1056) or
/// settled after it, and names the state and the way out.
#[skuld::test]
fn a_start_over_a_paused_service_says_it_is_paused() {
    let notified = start_via_notify(&mut FakeScm::new([Some(Observed::Paused)]), unbounded(), Kind::Managed)
        .unwrap_err()
        .to_string();
    // Paused when the arm reads the status, before any wait.
    let mut fake = FakeScm::new([]).drifting_after(0, QueriedState::Paused);
    let at_arm = start_via_notify(&mut fake, unbounded(), Kind::Managed)
        .unwrap_err()
        .to_string();
    assert_eq!(fake.log, vec!["start", "arm_settle"]);
    // 1056 over a paused service: nothing is waited for.
    let mut fake = FakeScm::new([]).already_running(QueriedState::Paused);
    let waiting = start_via_notify(&mut fake, unbounded(), Kind::Managed)
        .unwrap_err()
        .to_string();
    assert_eq!(fake.log, vec!["start"]);
    let immediate = request_start(&mut FakeScm::new([]).already_running(QueriedState::Paused))
        .unwrap_err()
        .to_string();
    assert_eq!(waiting, notified);
    assert_eq!(immediate, notified);
    assert!(
        notified.contains("paused, not running") && notified.contains("`sc continue`"),
        "{notified}"
    );
    assert_eq!(at_arm, notified);
}

#[skuld::test]
fn a_start_after_an_unconfirmed_stop_over_a_stopping_service_says_so() {
    let mut fake = FakeScm::new([]).already_running(QueriedState::StopPending);
    let text = request_start_after_stop(&mut fake).unwrap_err().to_string();
    assert!(text.contains("is stopping"), "{text}");
}

/// `restart --timeout 0`'s start, over the service the real `goetia-shim`
/// presents between a stop request and its `STOP_PENDING` report: still
/// `RUNNING`. That 1056 is a refusal — reading it as a start reports a
/// restart that leaves the daemon down.
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
    // prove it bites with `cargo nextest run --profile ci <name>` and expect
    // TIMEOUT.
    let mut fake = FakeRegistrar::new([], ERROR_SERVICE_NOTIFY_CLIENT_LAGGING);
    assert_eq!(
        register_until_deadline(&mut fake, spent()).unwrap(),
        Registration::Expired
    );
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
        Registration::Registered
    );
    assert_eq!(fake.log, vec!["register", "reopen", "register"]);
}

#[skuld::test]
fn a_registration_refused_for_a_marked_service_is_reported_not_retried() {
    let mut fake = FakeRegistrar::new([], ERROR_SERVICE_MARKED_FOR_DELETE);
    assert_eq!(
        register_until_deadline(&mut fake, unbounded()).unwrap(),
        Registration::MarkedForDelete
    );
    assert_eq!(fake.log, vec!["register"]);
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

// a stop that left the daemon running ---------------------------------------------------------------------------------

/// The shim reported that it could not show the daemon's tree gone, and SCM recorded it.
fn incomplete(fake: FakeScm) -> FakeScm {
    fake.stopped_after(Reported::incomplete_stop())
}

/// How a stop is confirmed, as the script of each of the two stop waits. `None` for a way the wait has no
/// such script for.
struct Confirming {
    name: &'static str,
    stop: Option<fn() -> FakeScm>,
    settle: Option<fn() -> FakeScm>,
}

fn confirmations() -> [Confirming; 5] {
    let under_way = || {
        FakeScm::new([Some(Observed::Pending), Some(Observed::Stopped)])
            .stop_replies([StopReply::Os(ERROR_SERVICE_CANNOT_ACCEPT_CTRL)])
            .queried(QueriedState::StopPending, false)
    };
    [
        Confirming {
            name: "a stop we sent",
            stop: Some(|| FakeScm::new([Some(Observed::Stopped)])),
            settle: Some(|| {
                FakeScm::new([Some(Observed::Running), Some(Observed::Stopped)]).stop_replies([StopReply::Ok])
            }),
        },
        // 1061: a stop is under way.
        Confirming {
            name: "a stop under way",
            stop: Some(under_way),
            settle: Some(|| {
                FakeScm::new([Some(Observed::Running), Some(Observed::Stopped)]).stop_replies([
                    StopReply::Os(ERROR_SERVICE_CANNOT_ACCEPT_CTRL),
                    StopReply::Os(ERROR_SERVICE_CANNOT_ACCEPT_CTRL),
                ])
            }),
        },
        // The status an arm returns, and the one read in place of an arm SCM refused on a marked service.
        Confirming {
            name: "the status of an arm",
            stop: Some(|| FakeScm::new([]).drifting_after(0, QueriedState::Stopped)),
            settle: None,
        },
        Confirming {
            name: "the status of a refused arm on a marked service",
            stop: Some(|| {
                FakeScm::new([])
                    .drifting_after(0, QueriedState::Stopped)
                    .refusing_arms_in(STOPPED_ONLY)
            }),
            settle: None,
        },
        // `stop_via_notify` sends its STOP first, so it has no already-stopped service to find.
        Confirming {
            name: "a service already stopped",
            stop: None,
            settle: Some(|| {
                FakeScm::new([])
                    .starting_in(QueriedState::Stopped)
                    .refusing_arms_in(STOPPED_ONLY)
            }),
        },
    ]
}

/// What the stop wait `run` came to with the record `fake` holds: whether it failed as an incomplete
/// stop. Any other end is a confirmed stop.
fn is_judged(ended: io::Result<Waited>) -> bool {
    match ended {
        Ok(waited) => {
            assert_eq!(waited, Waited::Confirmed);
            false
        }
        Err(e) => {
            assert!(is_incomplete_stop(&e), "{e}");
            true
        }
    }
}

/// A shim's stop that SCM confirmed with the incomplete-stop record fails, whichever way the stop was
/// confirmed (see [`confirmations`]: a STOP we sent, one already under way (1061), the status of an arm,
/// the status read in place of an arm refused on a marked service, a service already `STOPPED`), and the
/// same in `stop`'s wait and `uninstall`'s.
#[skuld::test]
fn a_shims_stop_with_the_incomplete_stop_record_fails_however_it_was_confirmed() {
    for Confirming { name, stop, settle } in confirmations() {
        if let Some(script) = stop {
            let ended = stop_via_notify(&mut incomplete(script()), unbounded(), true);
            assert!(is_judged(ended), "stop_via_notify over {name}");
        }
        if let Some(script) = settle {
            let ended = super::settle_and_stop(&mut incomplete(script()), unbounded(), true, |_, _| Ok(()));
            assert!(is_judged(ended), "settle_and_stop over {name}");
        }
    }
}

/// Not a shim's service: the record is the service's own, and no stop over it fails.
#[skuld::test]
fn a_stop_of_a_service_that_is_not_a_shims_is_not_judged_by_the_shims_record() {
    for Confirming { name, stop, settle } in confirmations() {
        if let Some(script) = stop {
            let ended = stop_via_notify(&mut incomplete(script()), unbounded(), false);
            assert!(!is_judged(ended), "stop_via_notify over {name}");
        }
        if let Some(script) = settle {
            let ended = super::settle_and_stop(&mut incomplete(script()), unbounded(), false, |_, _| Ok(()));
            assert!(!is_judged(ended), "settle_and_stop over {name}");
        }
    }
}

/// Every other record of a shim's stop is a stop: the clean one, a daemon's code, a refusal, a shim failure.
#[skuld::test]
fn a_shims_stop_with_any_other_record_does_not_fail() {
    for reported in [
        Reported::Clean,
        Reported::from_daemon_code(7),
        Reported::Refused,
        Reported::shim_failure(Some(2)),
    ] {
        let mut fake = FakeScm::new([Some(Observed::Stopped)]).stopped_after(reported);
        assert_eq!(
            stop_via_notify(&mut fake, unbounded(), true).unwrap(),
            Waited::Confirmed,
            "{reported:?}"
        );
    }
}

/// A service found gone is stopped with nothing read, so there is no record to judge.
#[skuld::test]
fn a_shims_service_found_gone_is_a_stop() {
    let mut fake = FakeScm::new([Some(Observed::Pending)]).gone_at_arm(2);
    assert_eq!(
        stop_via_notify(&mut fake, unbounded(), true).unwrap(),
        Waited::Confirmed
    );
}

/// `stop_and_await_exit` fails over the record, as the stop under it does.
#[skuld::test]
fn stop_and_await_exit_fails_over_a_shims_incomplete_stop_record() {
    let mut fake = incomplete(FakeScm::new([Some(Observed::Stopped)]));

    let err =
        stop_and_await_exit(&mut fake, unbounded(), &mut HoldOutcome::<u32>::NoProcess, true, no_end).unwrap_err();

    assert!(is_incomplete_stop(&err), "{err}");
}

/// The shim process goetia holds is ended before the incomplete stop is returned: its KILL_ON_JOB_CLOSE
/// job is the last thing that can still kill the tree.
#[skuld::test]
fn stop_and_await_exit_ends_the_held_process_of_an_incomplete_stop_before_failing() {
    let mut fake = incomplete(FakeScm::new([Some(Observed::Stopped)]));
    let calls = std::cell::RefCell::new(vec![]);

    let err = stop_and_await_exit(
        &mut fake,
        unbounded(),
        &mut HoldOutcome::Held(7u32),
        true,
        ending(
            |process| {
                calls.borrow_mut().push(("terminate", *process));
                Ok(())
            },
            |process, _, _| {
                calls.borrow_mut().push(("wait", *process));
                Ok(Waited::Confirmed)
            },
        ),
    )
    .unwrap_err();

    assert!(is_incomplete_stop(&err), "{err}");
    assert_eq!(*calls.borrow(), vec![("terminate", 7), ("wait", 7)]);
}

/// The incomplete stop is returned whatever became of ending the held process, and says when the
/// process was not seen to exit, with why: goetia has no log at this layer to say it in.
#[skuld::test]
fn an_incomplete_stop_says_when_the_held_process_was_not_ended() {
    let plain = stop_and_await_exit(
        &mut incomplete(FakeScm::new([Some(Observed::Stopped)])),
        unbounded(),
        &mut HoldOutcome::<u32>::NoProcess,
        true,
        no_end,
    )
    .unwrap_err()
    .to_string();
    let ran = |terminate: io::Result<()>, exit: io::Result<Waited>| {
        let mut fake = incomplete(FakeScm::new([Some(Observed::Stopped)]));
        let err = stop_and_await_exit(
            &mut fake,
            unbounded(),
            &mut HoldOutcome::Held(7u32),
            true,
            ending(|_| terminate, |_, _, _| exit),
        )
        .unwrap_err();
        assert!(is_incomplete_stop(&err), "{err}");
        err.to_string()
    };

    assert_eq!(
        ran(Ok(()), Ok(Waited::Confirmed)),
        plain,
        "an exited process adds nothing"
    );
    assert_eq!(
        ran(Err(io::Error::other("denied")), Ok(Waited::Confirmed)),
        plain,
        "a process that exited anyway adds nothing"
    );
    let lingers = ran(Ok(()), Ok(Waited::Expired));
    assert!(
        lingers.starts_with(&plain) && lingers.contains("had not exited"),
        "{lingers}"
    );
    let refused = ran(Err(io::Error::other("denied")), Ok(Waited::Expired));
    assert!(
        refused.contains("had not exited") && refused.contains("terminating it failed") && refused.contains("denied"),
        "{refused}"
    );
    let failed = ran(Ok(()), Err(io::Error::other("wait failed")));
    assert!(
        failed.starts_with(&plain)
            && failed.contains("waiting for it to exit failed")
            && failed.contains("wait failed"),
        "{failed}"
    );
    let both = ran(Err(io::Error::other("denied")), Err(io::Error::other("wait failed")));
    assert!(
        both.contains("waiting for it to exit failed")
            && both.contains("wait failed")
            && both.contains("terminating it failed")
            && both.contains("denied"),
        "{both}"
    );
}

// the process held across an incomplete stop --------------------------------------------------------------------------

/// The process `end_on_incomplete_stop` ended, with what `held` was left as.
fn ended_by_incomplete_stop(waited: io::Result<Waited>, held: &mut HoldOutcome<u32>) -> (io::Result<Waited>, Vec<u32>) {
    let ended = std::cell::RefCell::new(vec![]);
    let waited = end_on_incomplete_stop(waited, held, |process| {
        ended.borrow_mut().push(process);
        Ok(EndProcessOutcome::Exited)
    });
    (waited, ended.into_inner())
}

/// Only an incomplete stop ends the process here: a confirmed stop and any other failure leave it to
/// the caller, which ends it after a confirmed stop (see [`stop_and_await_exit`]) or drops it.
#[skuld::test]
fn only_an_incomplete_stop_ends_the_held_process() {
    for waited in [
        Ok(Waited::Confirmed),
        Ok(Waited::Expired),
        Err(io::Error::other("not left stopped")),
    ] {
        let mut held = HoldOutcome::Held(7u32);

        let (passed, ended) = ended_by_incomplete_stop(waited, &mut held);

        assert!(ended.is_empty(), "{passed:?}");
        assert!(matches!(held, HoldOutcome::Held(7)), "{held:?}");
    }
}

/// With nothing held the incomplete stop is returned as it is, and a process goetia could not open
/// keeps the reason it was not held.
#[skuld::test]
fn an_incomplete_stop_with_no_process_held_is_returned_as_it_is() {
    for held in [
        HoldOutcome::NoProcess,
        HoldOutcome::Refused(io::Error::other("access denied")),
    ] {
        let mut held = held;
        let waited = stop_via_notify(
            &mut incomplete(FakeScm::new([Some(Observed::Stopped)])),
            unbounded(),
            true,
        );

        let (waited, ended) = ended_by_incomplete_stop(waited, &mut held);

        assert!(is_incomplete_stop(&waited.unwrap_err()));
        assert!(ended.is_empty());
        assert!(
            matches!(held, HoldOutcome::NoProcess | HoldOutcome::Refused(_)),
            "{held:?}"
        );
    }
}

// start's vocabulary, through what `runs_a_shim` answers --------------------------------------------------------------

/// A service whose blob will not decode is a shim's only when its image says so: a start over an image
/// of the user's own, or one that could not be read, is judged as strictly as a `type: managed`
/// service's, and its failure says that the blob could not be read. Driven through `start`'s own wait
/// over the scripted SCM, from the answer of the predicate `start` and `stop` share.
/// The service ends right after it was started, which only the shim's vocabulary calls a start.
#[cfg(windows)]
#[skuld::test]
fn a_start_over_an_unreadable_blob_is_strict_unless_the_image_is_a_shim() {
    use crate::backend::scm::manager::{shim_answer, start_on};
    use crate::backend::scm::test_support::SHIM_HEX;
    use crate::spec::Id;

    let shim = format!(r#""C:\Program Files\Goetia\goetia-shim-{SHIM_HEX}.exe" --service-host"#);
    let corrupt = std::sync::Arc::new(crate::error::Error::blob("corrupt"));
    let id = Id::try_from("frpc").unwrap();

    for (image, started) in [
        (Some(shim.as_str()), true),
        (Some(r"C:\bin\frpc.exe"), false),
        (None, false),
    ] {
        let answer = shim_answer(Err(&corrupt), || image.map(str::to_string));
        let mut fake = FakeScm::new([Some(Observed::Stopped)]).stopped_after(Reported::Clean);

        let result = start_on(&mut fake, &id, Budget::Unbounded, unbounded(), &answer);

        match (started, result) {
            (true, Ok(())) => {}
            (false, Err(e)) => {
                let said = e.report().to_string();
                assert!(
                    said.contains("could not be read") && said.contains("corrupt"),
                    "{image:?}: {said}"
                );
            }
            (_, result) => panic!("{image:?}: started should be {started}, got {result:?}"),
        }
    }
}

/// A start that fails over an unreadable blob names the blob whichever vocabulary the image gave it:
/// a shim's, which is not a `type: managed` service's, says so.
#[cfg(windows)]
#[skuld::test]
fn a_failed_start_of_a_shim_known_by_its_image_names_the_unreadable_blob() {
    use crate::backend::scm::manager::{shim_answer, start_on};
    use crate::backend::scm::test_support::SHIM_HEX;
    use crate::spec::Id;

    let shim = format!(r#""C:\Program Files\Goetia\goetia-shim-{SHIM_HEX}.exe" --service-host"#);
    let corrupt = std::sync::Arc::new(crate::error::Error::blob("corrupt"));
    let answer = shim_answer(Err(&corrupt), || Some(shim));
    let mut fake = FakeScm::new([Some(Observed::Stopped)]).stopped_after(Reported::incomplete_stop());

    let err = start_on(
        &mut fake,
        &Id::try_from("frpc").unwrap(),
        Budget::Unbounded,
        unbounded(),
        &answer,
    )
    .unwrap_err();

    let said = err.report().to_string();
    assert!(said.contains("could not be read") && said.contains("corrupt"), "{said}");
    assert!(said.contains("may still be running"), "{said}");
    assert!(
        !said.contains("type: managed"),
        "the start was judged as a shim's: {said}"
    );
}

/// A start that times out over an unreadable blob names the blob too, whichever vocabulary the image
/// gave it: the timeout is a start judged without the blob as much as a failed one.
#[cfg(windows)]
#[skuld::test]
fn a_timed_out_start_over_an_unreadable_blob_names_the_blob() {
    use crate::backend::scm::manager::{shim_answer, start_on};
    use crate::backend::scm::test_support::SHIM_HEX;
    use crate::spec::Id;

    let shim = format!(r#""C:\Program Files\Goetia\goetia-shim-{SHIM_HEX}.exe" --service-host"#);
    let corrupt = std::sync::Arc::new(crate::error::Error::blob("corrupt"));

    for image in [shim.as_str(), r"C:\bin\frpc.exe"] {
        let answer = shim_answer(Err(&corrupt), || Some(image.to_string()));
        let mut fake = FakeScm::new([None]);

        let err = start_on(
            &mut fake,
            &Id::try_from("frpc").unwrap(),
            Budget::Bounded(std::time::Duration::from_secs(3600)),
            unbounded(),
            &answer,
        )
        .unwrap_err();

        assert!(
            matches!(err.cause(), crate::error::Error::WaitTimeout { awaited: "running", .. }),
            "{image}: {err:?}"
        );
        let said = err.report().to_string();
        assert!(
            said.contains("could not be read") && said.contains("corrupt"),
            "{image}: {said}"
        );
    }
}

/// A blob that decodes decides the vocabulary of `start`: `type: simple` is the shim's, so a service
/// that ends right after it was started is started, and `type: managed` is strict and fails, with no
/// blob error to name since the blob was read.
#[cfg(windows)]
#[skuld::test]
fn a_start_over_a_decodable_blob_is_read_by_its_kind() {
    use crate::backend::scm::manager::{shim_answer, start_on};
    use crate::backend::scm::test_support::blob_of;
    use crate::spec::Id;

    let id = Id::try_from("frpc").unwrap();
    let image_is_not_read = || panic!("a blob that decodes decides; the image is not read");

    for kind in [Kind::Simple, Kind::Managed] {
        let answer = shim_answer(Ok(&blob_of(kind)), image_is_not_read);
        let mut fake = FakeScm::new([Some(Observed::Stopped)]).stopped_after(Reported::Clean);

        let result = start_on(&mut fake, &id, Budget::Unbounded, unbounded(), &answer);

        match (kind, result) {
            (Kind::Simple, Ok(())) => {}
            (Kind::Managed, Err(e)) => {
                let said = e.report().to_string();
                assert!(said.contains("stopped before reaching Running"), "{said}");
                assert!(!said.contains("blob"), "{said}");
            }
            (kind, result) => panic!("{kind:?}: unexpected {result:?}"),
        }
    }
}

// stop and uninstall over the scripted SCM ----------------------------------------------------------------------------

/// The pid of a process held as a number, which is all a stop's error asks of one.
#[cfg(windows)]
impl crate::backend::scm::kill::HeldProcess for u32 {
    fn pid(&self) -> u32 {
        *self
    }
}

/// The answers `runs_a_shim` gives for a service that runs a shim, one known not to, and one that is
/// not known either way.
#[cfg(windows)]
fn answers() -> [(&'static str, crate::backend::scm::manager::ShimAnswer); 3] {
    use crate::backend::scm::manager::shim_answer;
    use crate::backend::scm::test_support::blob_of;

    let corrupt = std::sync::Arc::new(crate::error::Error::blob("corrupt"));
    [
        ("a shim", shim_answer(Ok(&blob_of(Kind::Simple)), || None)),
        ("not a shim", shim_answer(Ok(&blob_of(Kind::Managed)), || None)),
        ("unknown", shim_answer(Err(&corrupt), || None)),
    ]
}

/// A `(0, 1003)` record is judged for a service that runs a shim and for one not known not to, and left
/// alone for one known not to: its own `(0, 1003)` is not the shim's.
#[cfg(windows)]
#[skuld::test]
fn a_stop_judges_the_incomplete_stop_record_of_every_service_not_known_to_run_no_shim() {
    use crate::backend::scm::manager::stop_on;
    use crate::spec::Id;

    let id = Id::try_from("frpc").unwrap();
    for (what, answer) in answers() {
        let mut fake = incomplete(FakeScm::new([Some(Observed::Stopped)]));

        let result = stop_on(
            &mut fake,
            &id,
            Budget::Unbounded,
            unbounded(),
            &answer,
            |_| Ok(HoldOutcome::<u32>::NoProcess),
            no_end,
        );

        match (what, result) {
            ("not a shim", Ok(())) => {}
            ("not a shim", Err(e)) => panic!("a service known not to run a shim was judged: {}", e.report()),
            (_, Err(e)) => assert!(e.report().to_string().contains("`(0, 1003)`"), "{what}: {}", e.report()),
            (_, Ok(())) => panic!("{what}: an incomplete stop was a stop"),
        }
    }
}

/// A stop that does not wait holds nothing and reads no record: it sends the STOP and returns.
#[cfg(windows)]
#[skuld::test]
fn a_stop_that_does_not_wait_holds_nothing() {
    use crate::backend::scm::manager::stop_on;
    use crate::spec::Id;

    let [(_, answer), ..] = answers();
    let mut fake = incomplete(FakeScm::new([]));
    let holds = std::cell::Cell::new(0);

    stop_on(
        &mut fake,
        &Id::try_from("frpc").unwrap(),
        Budget::Immediate,
        spent(),
        &answer,
        |_| {
            holds.set(holds.get() + 1);
            Ok(HoldOutcome::<u32>::NoProcess)
        },
        no_end,
    )
    .unwrap();

    assert_eq!(holds.get(), 0, "a stop that does not wait holds no process");
    assert_eq!(fake.log, vec!["control_stop"]);
}

/// A held process that outlasts the stop's budget is the process-wait timeout, naming the pid it held.
#[cfg(windows)]
#[skuld::test]
fn a_held_process_that_outlasts_the_stop_is_a_wait_timeout_naming_its_pid() {
    use crate::backend::scm::manager::stop_on;
    use crate::error::Error;
    use crate::spec::Id;

    let [(_, answer), ..] = answers();
    let budget = Budget::Bounded(std::time::Duration::from_secs(3600));
    let mut fake = FakeScm::new([Some(Observed::Stopped)]);

    let err = stop_on(
        &mut fake,
        &Id::try_from("frpc").unwrap(),
        budget,
        budget.start(),
        &answer,
        |_| Ok(HoldOutcome::Held(7u32)),
        |process, _| {
            Ok(EndProcessOutcome::Lingers {
                process,
                terminate_failed: None,
            })
        },
    )
    .unwrap_err();

    assert!(matches!(err, Error::ProcessWaitTimeout { pid: 7, .. }), "{err:?}");
}

/// A held process whose wait failed after its terminate failed leaves `stop` the wait's OS code, and
/// says that terminating it failed too.
#[cfg(windows)]
#[skuld::test]
fn a_failed_wait_for_the_held_process_is_a_stop_error_with_the_waits_code_and_the_failed_terminate() {
    use crate::backend::scm::manager::stop_on;
    use crate::spec::Id;

    let [(_, answer), ..] = answers();
    let mut fake = FakeScm::new([Some(Observed::Stopped)]);

    let err = stop_on(
        &mut fake,
        &Id::try_from("frpc").unwrap(),
        Budget::Unbounded,
        unbounded(),
        &answer,
        |_| Ok(HoldOutcome::Held(7u32)),
        ending(terminate_fails, |_, _, _| Err(io::Error::from_raw_os_error(6))),
    )
    .unwrap_err();

    assert_eq!(err.raw_os_error(), Some(6), "{}", err.report());
    let said = err.report().to_string();
    assert!(
        said.contains("terminating it failed") && said.contains("os error 6"),
        "{said}"
    );
}

/// What an uninstall that disabled the service found it as: not marked, with this start type before.
#[cfg(windows)]
const DISABLED_BY_US: (bool, Option<windows_service::service::ServiceStartType>) =
    (false, Some(windows_service::service::ServiceStartType::AutoStart));

/// An uninstall whose stop SCM confirmed with the shim's incomplete-stop record ends the process it held
/// before it fails, and fails saying the service was left stopped, disabled and not deleted.
#[cfg(windows)]
#[skuld::test]
fn an_uninstall_over_an_incomplete_stop_ends_the_held_process_before_failing() {
    use crate::backend::scm::manager::uninstall_stop_on;
    use crate::spec::Id;

    let [(_, answer), ..] = answers();
    let mut fake =
        incomplete(FakeScm::new([Some(Observed::Running), Some(Observed::Stopped)]).stop_replies([StopReply::Ok]));
    let ended = std::cell::RefCell::new(vec![]);
    let (marked, previous) = DISABLED_BY_US;

    let err = uninstall_stop_on(
        &mut fake,
        &Id::try_from("frpc").unwrap(),
        &answer,
        marked,
        previous,
        Some(|| Ok(HoldOutcome::Held(7u32))),
        |process| {
            ended.borrow_mut().push(process);
            Ok(EndProcessOutcome::Exited)
        },
    )
    .expect_err("an incomplete stop fails the uninstall");

    assert_eq!(*ended.borrow(), vec![7]);
    let said = err.report().to_string();
    assert!(said.contains("may still be running"), "{said}");
    assert!(said.contains("NOT deleted and is now disabled"), "{said}");
}

/// An uninstall holds a process only when it is told to, once before its STOP, and judges the record by
/// the same answer as `stop`: a service known not to run a shim finishes its stop whatever it recorded.
#[cfg(windows)]
#[skuld::test]
fn an_uninstall_holds_only_when_told_to_and_judges_only_what_may_be_a_shim() {
    use crate::backend::scm::manager::uninstall_stop_on;
    use crate::spec::Id;

    let id = Id::try_from("frpc").unwrap();
    let (marked, previous) = DISABLED_BY_US;
    let stop =
        || incomplete(FakeScm::new([Some(Observed::Running), Some(Observed::Stopped)]).stop_replies([StopReply::Ok]));
    for (what, answer) in answers() {
        let result = uninstall_stop_on(
            &mut stop(),
            &id,
            &answer,
            marked,
            previous,
            None::<fn() -> crate::error::Result<HoldOutcome<u32>>>,
            no_end_of,
        );

        match (what, result) {
            ("not a shim", Ok(HoldOutcome::NoProcess)) => {}
            ("not a shim", other) => panic!(
                "a service known not to run a shim was judged or held: {:?}",
                other.err()
            ),
            (_, Err(_)) => {}
            (_, Ok(_)) => panic!("{what}: an incomplete stop was a stop"),
        }

        let holds = std::cell::Cell::new(0);
        let held = uninstall_stop_on(
            &mut stop(),
            &id,
            &answer,
            marked,
            previous,
            Some(|| {
                holds.set(holds.get() + 1);
                Ok(HoldOutcome::Held(7u32))
            }),
            |_| Ok(EndProcessOutcome::Exited),
        );

        assert_eq!(holds.get(), 1, "{what}: held once, before the STOP");
        if what == "not a shim" {
            assert!(matches!(held, Ok(HoldOutcome::Held(7))), "{what}: {:?}", held.err());
        }
    }
}

/// A wait that expires under an uninstall's unbounded deadline is goetia's own invariant breach: a stop
/// failure whether or not someone else had marked the service, never their doing.
#[cfg(windows)]
#[skuld::test]
fn an_uninstall_whose_unbounded_stop_expires_is_a_stop_failure_whether_or_not_it_was_marked() {
    use crate::backend::scm::manager::uninstall_stop_on;
    use crate::spec::Id;

    let [(_, answer), ..] = answers();
    for marked in [false, true] {
        let mut fake = FakeScm::new([Some(Observed::Running), None]).stop_replies([StopReply::Ok]);

        let err = uninstall_stop_on(
            &mut fake,
            &Id::try_from("frpc").unwrap(),
            &answer,
            marked,
            None,
            None::<fn() -> crate::error::Result<HoldOutcome<u32>>>,
            no_end_of,
        )
        .expect_err("an expiry with no bound is a failure");

        let said = err.report().to_string();
        assert!(said.contains("goetia's own error"), "marked {marked}: {said}");
        assert!(said.contains("could not be stopped"), "marked {marked}: {said}");
        assert!(
            !said.contains("marked for deletion by someone else"),
            "marked {marked}: {said}"
        );
        assert_eq!(said.contains("is now disabled"), !marked, "marked {marked}: {said}");
        assert_eq!(
            said.contains("already marked for deletion"),
            marked,
            "marked {marked}: {said}"
        );
    }
}

/// A hold that fails is goetia's or the hold's doing, never that of whoever marked the service for
/// deletion: it is told with its cause whether or not the service was marked. Only an uninstall that
/// disabled the service says it did.
#[cfg(windows)]
#[skuld::test]
fn an_uninstall_whose_hold_failed_is_a_stop_failure_whether_or_not_it_was_marked() {
    use crate::backend::scm::manager::uninstall_stop_on;
    use crate::spec::Id;

    let [(_, answer), ..] = answers();
    for marked in [false, true] {
        let mut fake = FakeScm::new([Some(Observed::Running), Some(Observed::Stopped)]).stop_replies([StopReply::Ok]);

        let err = uninstall_stop_on(
            &mut fake,
            &Id::try_from("frpc").unwrap(),
            &answer,
            marked,
            None,
            Some(|| -> crate::error::Result<HoldOutcome<u32>> { Err(crate::error::Error::blob("held badly")) }),
            no_end_of,
        )
        .expect_err("the hold failed");

        let said = err.report().to_string();
        assert!(said.contains("held badly"), "marked {marked}: {said}");
        assert!(said.contains("could not be stopped"), "marked {marked}: {said}");
        assert!(
            !said.contains("marked for deletion by someone else"),
            "marked {marked}: {said}"
        );
        assert_eq!(said.contains("is now disabled"), !marked, "marked {marked}: {said}");
        assert_eq!(
            said.contains("already marked for deletion"),
            marked,
            "marked {marked}: {said}"
        );
    }
}

/// A service already `STOP_PENDING` when the uninstall finds it is stopping without a STOP of ours, so a
/// hold that fails there is not told as a failed stop: the service is already stopping, and the advice
/// is to wait for it and re-run, with `kill` for a stop that never ends. Marked or not.
#[cfg(windows)]
#[skuld::test]
fn an_uninstall_whose_hold_of_a_stop_pending_service_failed_says_the_service_is_already_stopping() {
    use crate::backend::scm::manager::uninstall_stop_on;
    use crate::spec::Id;

    let [(_, answer), ..] = answers();
    for marked in [false, true] {
        let mut fake = FakeScm::new([Some(Observed::Stopped)]).starting_in(QueriedState::StopPending);

        let err = uninstall_stop_on(
            &mut fake,
            &Id::try_from("frpc").unwrap(),
            &answer,
            marked,
            None,
            Some(|| -> crate::error::Result<HoldOutcome<u32>> { Err(crate::error::Error::blob("held badly")) }),
            no_end_of,
        )
        .expect_err("the hold failed");

        let said = err.report().to_string();
        assert!(said.contains("held badly"), "marked {marked}: {said}");
        assert!(said.contains("already stopping"), "marked {marked}: {said}");
        assert!(said.contains("goetia daemon kill frpc"), "marked {marked}: {said}");
        assert!(
            !said.contains("could not be stopped") && !said.contains("does not accept STOP"),
            "no STOP was sent: marked {marked}: {said}"
        );
        assert_eq!(said.contains("is now disabled"), !marked, "marked {marked}: {said}");
        assert_eq!(
            said.contains("goetia daemon uninstall frpc"),
            !marked,
            "marked {marked}: {said}"
        );
        assert!(!fake.log.contains(&"control_stop"), "marked {marked}: {:?}", fake.log);
    }
}

/// A hold that expired cannot be told by an uninstall, whose waits have no bound: it fails, naming the
/// pid SCM still reported, rather than going on as if it had held a process. A marked service is no
/// different.
#[cfg(windows)]
#[skuld::test]
fn an_uninstall_whose_hold_expired_fails_naming_the_pid() {
    use crate::backend::scm::manager::uninstall_stop_on;
    use crate::spec::Id;

    let [(_, answer), ..] = answers();
    for marked in [false, true] {
        let mut fake = FakeScm::new([Some(Observed::Running), Some(Observed::Stopped)]).stop_replies([StopReply::Ok]);

        let err = uninstall_stop_on(
            &mut fake,
            &Id::try_from("frpc").unwrap(),
            &answer,
            marked,
            None,
            Some(|| Ok(HoldOutcome::<u32>::Expired { pid: 9 })),
            no_end_of,
        )
        .expect_err("an expired hold is not a held process");

        let said = err.report().to_string();
        assert!(said.contains("process 9"), "marked {marked}: {said}");
        assert_eq!(
            said.contains("already marked for deletion"),
            marked,
            "marked {marked}: {said}"
        );
        assert_eq!(said.contains("is now disabled"), !marked, "marked {marked}: {said}");
    }
}

// the incomplete stop's wording ---------------------------------------------------------------------------------------

/// A failed stop and a failed start over the shim's incomplete-stop record say what SCM recorded and
/// what it means, and do not claim who wrote it.
#[cfg(windows)]
#[skuld::test]
fn an_incomplete_stop_is_told_by_what_scm_recorded_and_not_by_who_wrote_it() {
    use crate::backend::scm::manager::{shim_answer, start_on};
    use crate::backend::scm::test_support::blob_of;
    use crate::spec::Id;

    let mut fake = incomplete(FakeScm::new([Some(Observed::Stopped)]));
    let stop = stop_and_await_exit(&mut fake, unbounded(), &mut HoldOutcome::<u32>::NoProcess, true, no_end)
        .unwrap_err()
        .to_string();

    let answer = shim_answer(Ok(&blob_of(Kind::Simple)), || None);
    let mut fake = FakeScm::new([Some(Observed::Stopped)]).stopped_after(Reported::incomplete_stop());
    let start = start_on(
        &mut fake,
        &Id::try_from("frpc").unwrap(),
        Budget::Unbounded,
        unbounded(),
        &answer,
    )
    .unwrap_err()
    .report()
    .to_string();

    for said in [stop, start] {
        assert!(
            said.contains("SCM recorded `(0, 1003)`, the code goetia-shim records when it could not end the daemon's process tree, so the daemon may still be running"),
            "{said}"
        );
        assert!(!said.contains("if the service runs a shim"), "{said}");
    }
}
