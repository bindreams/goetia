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
    /// What `query_status` answers; `None` fails the test, so a refused STOP's status is always scripted.
    queried: Option<QueriedStatus>,
    /// The notification no `control_stop` has answered yet. `None` after a `control_stop` and after
    /// an expiry.
    unanswered: Option<(Observed, bool)>,
    /// Each `control_stop` that did not follow a `Running` or `Paused` notification accepting STOP.
    /// Only the settle tests read it; the stop and start waits send theirs without a notification.
    bad_stops: Vec<String>,
    /// Whether an `arm` that registered has been neither delivered nor expired. SCM allows one
    /// registration per service handle, and an actor serves one wait, so an `arm` over it is a bug.
    armed: bool,
    /// Whether `start` has been issued, which widens the start wait's mask ([`WantState::states`]).
    started: bool,
    /// The service's state as SCM would hold it: `None` until the first notification is delivered,
    /// then the state that notification reported, moved by [`Self::drifting_after`].
    state: Option<QueriedState>,
    /// The states the latest `arm` registered for.
    mask: &'static [QueriedState],
    /// The state the latest notification reported. A re-arm with the same context and buffer on the
    /// same handle does not fire on a state that has not changed, even with that state in the mask
    /// (P20, P27 a). This model has one handle and one context, so it is always that shape.
    last_delivered: Option<QueriedState>,
    /// How many notifications have been delivered.
    deliveries: usize,
    /// States the service moves to after the n-th notification, before the next `arm`.
    drifts: Vec<(usize, QueriedState)>,
}

/// What a scripted `control_stop` answers.
#[derive(Debug, Clone, Copy)]
enum StopReply {
    Ok,
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
            queried: None,
            unanswered: None,
            bad_stops: vec![],
            armed: false,
            started: false,
            state: None,
            mask: &[],
            last_delivered: None,
            deliveries: 0,
            drifts: vec![],
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

    /// SCM's rule for this actor's shape (same handle, context and buffer): a registration fires at
    /// once only on a current state in its mask that it has not already reported (P20, P27 a), and later
    /// only on a transition into the mask, which is the next scripted notification. A wait with
    /// neither never returns; here it panics instead of hanging.
    /// Not applied before the first notification, when the state is not scripted.
    fn assert_something_can_fire(&self) {
        let Some(state) = self.state else { return };
        let fires_now = self.mask.contains(&state) && Some(state) != self.last_delivered;
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
                "lost wake-up: state {state:?} in mask {mask:?} was already reported, and nothing is scripted to change it"
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
        self.queried = Some(QueriedStatus { state, accepts_stop });
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
}

impl ScmActor for FakeScm {
    fn arm(&mut self, want: WantState, deadline: Deadline) -> std::io::Result<Waited> {
        self.arm_deadlines.push(deadline);
        self.log.push(match want {
            WantState::Stopped => "arm_stopped",
            WantState::Running => "arm_running",
            WantState::RunningPausedOrStopped => "arm_settle",
            WantState::StoppedOnly => "arm_stopped_only",
        });
        assert!(!self.armed, "arm over an outstanding registration");
        if let Some(at) = self.drifts.iter().position(|(n, _)| *n == self.deliveries) {
            self.state = Some(self.drifts.remove(at).1);
        }
        self.mask = want.states(self.started);
        self.armed = self.what_arm_reports == Waited::Confirmed;
        Ok(self.what_arm_reports)
    }
    fn control_stop(&mut self) -> std::io::Result<()> {
        self.log.push("control_stop");
        match self.unanswered.take() {
            Some((Observed::Running | Observed::Paused, true)) => {}
            other => self.bad_stops.push(format!("after {other:?}")),
        }
        match self.stop_replies.pop_front().unwrap_or(StopReply::Ok) {
            StopReply::Ok => Ok(()),
            StopReply::Os(code) => Err(std::io::Error::from_raw_os_error(code)),
        }
    }
    fn start(&mut self) -> std::io::Result<StartReply> {
        self.log.push("start");
        self.started = true;
        if self.start_fails {
            return Err(std::io::Error::other("scripted start failure"));
        }
        Ok(self.start_reply)
    }
    fn wait_callback(&mut self, deadline: Deadline) -> std::io::Result<Option<Observed>> {
        self.log.push("wait");
        // Delivered, or expired: an expiry closes and drains the actor's handles.
        self.armed = false;
        self.deadlines.push(deadline);
        self.assert_something_can_fire();
        let observed = self.waits.pop_front().expect("script ran dry");
        if let Some(observed) = observed {
            let state = self.state_of(observed);
            self.state = Some(state);
            self.last_delivered = Some(state);
            self.deliveries += 1;
        }
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
    fn accepts_stop(&self) -> bool {
        self.last_accepts
    }
    fn query_status(&mut self) -> std::io::Result<QueriedStatus> {
        self.log.push("query_status");
        // The service's state once a notification has told it, else the scripted answer.
        if let Some(state) = self.state {
            return Ok(QueriedStatus {
                state,
                accepts_stop: false,
            });
        }
        Ok(self.queried.expect("query_status was not scripted"))
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
            "arm_stopped_only", // re-arm: only STOPPED is left to wait for
            "query_status",     // read after the re-arm: the state may have moved on
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
    assert_eq!(stop_via_notify(&mut fake, unbounded()).unwrap(), Waited::Confirmed);
    assert_eq!(
        fake.log,
        vec![
            "arm_stopped",
            "control_stop",
            "query_status",
            "wait",
            "got_pending",
            "arm_stopped_only",
            "query_status",
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
    assert_eq!(stop_via_notify(&mut fake, unbounded()).unwrap(), Waited::Expired);
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
/// An error is not followed by a wait.
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
                .stop_replies([StopReply::Os(code)])
                .queried(state, accepts_stop)
        };
        let cell = format!("{code} then {state:?} accepting {accepts_stop}");
        let mut waiting = script();
        let mut immediate = script();
        let results = [
            stop_via_notify(&mut waiting, unbounded()).map(|waited| {
                assert_eq!(waited, Waited::Confirmed, "{cell}");
            }),
            request_stop(&mut immediate),
        ];
        if expected == UnderWay {
            for result in results {
                result.unwrap_or_else(|e| panic!("{cell}: {e}"));
            }
            assert!(waiting.log.contains(&"wait"), "{cell}: {:?}", waiting.log);
            continue;
        }
        assert_eq!(
            waiting.log,
            vec!["arm_stopped", "control_stop", "query_status"],
            "{cell}"
        );
        assert_eq!(immediate.log, vec!["control_stop", "query_status"], "{cell}");
        for result in results {
            let err = result.expect_err(&cell);
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
    }
}

/// Any other refusal is an error that keeps its OS code, without a status read.
#[skuld::test]
fn a_stop_refused_with_another_code_is_the_os_error() {
    let script = || FakeScm::new([Some(Observed::Stopped)]).stop_replies([StopReply::Os(5)]);
    let mut waiting = script();
    let mut immediate = script();
    assert_eq!(
        stop_via_notify(&mut waiting, unbounded()).unwrap_err().raw_os_error(),
        Some(5)
    );
    assert_eq!(request_stop(&mut immediate).unwrap_err().raw_os_error(), Some(5));
    assert_eq!(waiting.log, vec!["arm_stopped", "control_stop"]);
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
        let err = stop_via_notify(&mut fake, unbounded()).unwrap_err().to_string();
        for part in [said, "after it began to stop", "restart policy", "Run `stop` again."] {
            assert!(err.contains(part), "{state:?}: missing {part:?}: {err}");
        }
        assert_eq!(fake.stop_count(), 1, "{state:?}: no second STOP is sent");
        assert_eq!(
            fake.log,
            vec![
                "arm_stopped",
                "control_stop",
                "wait",
                "got_pending",
                "arm_stopped_only",
                "query_status"
            ],
            "{state:?}"
        );
    }
}

/// The same race when the service has stopped and stayed so: the re-arm reports `STOPPED` at once.
#[skuld::test]
fn a_stop_whose_service_stopped_before_the_re_arm_is_confirmed_by_the_read() {
    let mut fake = FakeScm::new([Some(Observed::Pending)]).drifting_after(1, QueriedState::Stopped);
    assert_eq!(stop_via_notify(&mut fake, unbounded()).unwrap(), Waited::Confirmed);
    assert_eq!(
        fake.log,
        vec![
            "arm_stopped",
            "control_stop",
            "wait",
            "got_pending",
            "arm_stopped_only",
            "query_status"
        ]
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

// settle_and_stop ----

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
            name: "running, a restart began (1052), running again, the stop is accepted, then stopped",
            waits: vec![Some(Running), Some(Running), Some(Stopped)],
            accepts: vec![],
            replies: vec![StopReply::Os(INVALID_SERVICE_CONTROL), StopReply::Ok],
            outcome: Outcome::Confirmed,
            log: vec![
                "arm_settle",
                "wait",
                "got_running",
                "control_stop",
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
            name: "running, a stop was already delivered (1061), the re-arm waits without a second stop, then stopped",
            waits: vec![Some(Running), Some(Stopped)],
            accepts: vec![],
            replies: vec![StopReply::Os(CANNOT_ACCEPT_CTRL)],
            outcome: Outcome::Confirmed,
            log: vec![
                "arm_settle",
                "wait",
                "got_running",
                "control_stop",
                "arm_settle",
                "wait",
                "got_stopped",
            ],
        },
        Row {
            name: "1052, 1061 and an accepted stop in a row",
            waits: vec![Some(Running), Some(Running), Some(Running), Some(Stopped)],
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
                "wait",
                "got_running",
                "control_stop",
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
                "query_status",
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
    let mut fake = FakeScm::new([Some(Observed::Pending), Some(Observed::Stopped)]);
    assert_eq!(stop_via_notify(&mut fake, deadline).unwrap(), Waited::Confirmed);
    assert_eq!(fake.arm_count(), 2);
    assert_eq!(fake.arm_deadlines, vec![deadline; 2]);
    assert_eq!(fake.deadlines, vec![deadline; 2]);
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

/// Whatever the budget, a `start` over a stopping service is never a success, and the error says
/// what to do: the service is stopping, SCM may start it again itself, and `start` is run again once
/// it has stopped.
#[skuld::test]
fn a_start_over_a_stopping_service_says_it_is_stopping() {
    let mut fake = FakeScm::new([]).already_running(QueriedState::StopPending);
    let waiting = start_via_notify(&mut fake, unbounded(), Kind::Simple)
        .unwrap_err()
        .to_string();
    assert_eq!(fake.log, vec!["arm_running", "start"], "nothing is waited for");
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
