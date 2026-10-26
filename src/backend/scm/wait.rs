//! Event-driven waits for SCM state transitions via `NotifyServiceStatusChangeW`
//! — a real kernel rendezvous (an APC delivered on the actual state
//! transition, awaited in `SleepEx(INFINITE, TRUE)`), not a poll loop.
//!
//! `windows-service`'s `stop()`/`start()` return as soon as SCM has accepted
//! the request — `StartServiceW` returns once the service's `ServiceMain`
//! thread has been created, not once it reports `SERVICE_RUNNING` — and the
//! crate wraps no wait primitive of its own. The MSDN checkpoint/`dwWaitHint`
//! pattern (`QueryServiceStatusEx` in a `Sleep` loop with a hardcoded bound)
//! is exactly the sleep-poll this project forbids. The orchestration below
//! is a pure state machine over [`ScmActor`], unit-tested with a fake in
//! `wait_tests.rs`; [`system::SystemScmActor`] drives the real SCM.
#![cfg_attr(not(windows), allow(dead_code))]

use std::io;

use crate::backend::scm::exit_status::RecordedExit;
use crate::manager::budget::Deadline;
use crate::spec::Kind;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WantState {
    Stopped,
    Running,
    /// A state that is settled: `RUNNING`, `PAUSED` or `STOPPED`, and no pending one. Arming
    /// `START_PENDING` while the service is starting would not fire until the start had settled.
    RunningPausedOrStopped,
    /// `STOPPED` alone, with no `STOP_PENDING`: a service already stopping at the arm does not fire
    /// it, only the stop completing does.
    StoppedOnly,
}

impl WantState {
    /// The states a registration for `self` fires on: the single definition of the notification
    /// mask, which `system::want_to_mask` folds into bits and the unit tests' model of SCM reads.
    ///
    /// A start wait includes `STOPPED`: it is armed after `start()`, when the service has entered
    /// `StartPending`, so a `StartPending -> Stopped` delivers a real `Stopped` that ends the wait with
    /// the failed start it is.
    pub fn states(self) -> &'static [QueriedState] {
        use QueriedState::{Paused, Running, StartPending, StopPending, Stopped};
        match self {
            WantState::Stopped => &[Stopped, StopPending],
            WantState::Running => &[Running, Stopped, StartPending],
            // No `START_PENDING`: a service starting at the arm must not fire it.
            WantState::RunningPausedOrStopped => &[Running, Paused, Stopped],
            WantState::StoppedOnly => &[Stopped],
        }
    }
}

/// The service state a `wait_callback` observed. Distinct from [`WantState`]:
/// a callback can report a state that is neither what the caller wants nor
/// its opposite. `Running`/`Stopped` are terminal; `Pending` is any
/// intermediate (`StartPending`/`StopPending`) and re-arms.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Observed {
    Running,
    Paused,
    Stopped,
    Pending,
}

/// Whether a wait reached the state it was waiting for, or ran out of
/// [`Deadline`] first. An expiry is not an error: the request was issued and
/// the service may yet arrive, so what to say about it is the caller's.
///
/// `#[must_use]` because dropping this reads an expiry as success, and
/// `Result`'s own `#[must_use]` does not catch it: `?` consumes the
/// `Result` and leaves a bare `Waited` behind.
#[must_use = "an expired wait did not confirm the state; decide what to report rather than discarding it"]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Waited {
    Confirmed,
    Expired,
}

/// What an [`ScmActor::arm`] came to.
///
/// `#[must_use]`, like [`Waited`]: dropping it reads an expiry as a registration.
#[must_use = "an expired arm registered nothing; the status of a registered one is what the caller decides on"]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Armed {
    /// The deadline ran out while SCM kept answering `ERROR_SERVICE_NOTIFY_CLIENT_LAGGING`.
    Expired,
    /// The notification is registered. The status was read after the registration, so a state that
    /// is already current is seen here and not waited for.
    Registered(QueriedStatus),
    /// SCM refused the registration with `ERROR_SERVICE_MARKED_FOR_DELETE`: it no longer reports a
    /// marked service's changes. Nothing is registered, and the status read in its place is all there
    /// is to decide on.
    Unwatchable(QueriedStatus),
}

/// The granular SCM operations [`stop_via_notify`]/[`start_via_notify`] need,
/// isolated so the ordering can be unit-tested with a fake rather than a real
/// service.
pub trait ScmActor {
    /// Register a status-change notification for `want`, and return the status read right after the
    /// registration. `NotifyServiceStatusChangeW` is single-shot, so the sequence re-arms after every
    /// non-terminal callback.
    ///
    /// Every caller decides on that status before it waits, because a registration does not always
    /// fire for the state it finds. A first registration on a handle fires at once for a current
    /// state in its mask (P20, P27 c, f). A re-arm with the same context and buffer on the same
    /// handle does not fire for an unchanged state (P20, P27 a), and a change to an in-mask state
    /// between the last delivery and the re-arm is not measured. A caller that only waits can
    /// therefore wait forever on a service that is already where it wants it. The status is read
    /// after the registration, so a change after it is delivered (P27 d, e) and one before it is
    /// seen. A notification the registration queues at once reports the state at the arm, whatever
    /// the service has done since, so a caller that issues an action arms after it and decides on the
    /// status and on the notifications that follow. A caller that issues none may ignore the status of
    /// its first arm on a fresh actor: it fires at once itself.
    ///
    /// SCM refuses the registration of a service marked for deletion that is `STOPPED` with
    /// `ERROR_SERVICE_MARKED_FOR_DELETE` (round 8), and arms in other states were accepted (P22b,
    /// P22d). That refusal is [`Armed::Unwatchable`] with the status read in its place.
    ///
    /// An `arm` over a registration no [`Self::wait_callback`] has consumed cancels that one first,
    /// as a caller that acts on the status leaves one behind. The registration reports into a
    /// buffer that cannot be reused until it has fired or been cancelled.
    ///
    /// `deadline` bounds the one loop here that is not the wait itself: the
    /// `ERROR_SERVICE_NOTIFY_CLIENT_LAGGING` remedy, which reopens both
    /// handles and arms again. [`Armed::Expired`] iff the deadline ran out
    /// while the SCM kept answering that.
    fn arm(&mut self, want: WantState, deadline: Deadline) -> io::Result<Armed>;
    /// Takes no deadline, as [`Self::start`] does not: each is a single
    /// non-looping call into the SCM, so there is no iteration to bound, and
    /// what blocking they do is the SCM's own — not a wait this module
    /// created, and not one it can bound from the client side.
    fn control_stop(&mut self) -> io::Result<()>;
    /// `StartServiceW`. What an `ERROR_SERVICE_ALREADY_RUNNING` means is not
    /// decided here but by each caller, from the state the follow-up query
    /// reported — see [`StartReply`].
    fn start(&mut self) -> io::Result<StartReply>;
    /// Block in an alertable wait until the armed notification fires and
    /// return the service's observed state from the callback buffer;
    /// `Ok(None)` iff `deadline` expired with nothing delivered.
    fn wait_callback(&mut self, deadline: Deadline) -> io::Result<Option<Observed>>;
    /// The exit code SCM recorded with the `Stopped` that the last [`Self::wait_callback`]
    /// returned. Read from that notification, not queried afterwards: a recovery action may have
    /// restarted the service by then.
    fn stopped_exit(&self) -> RecordedExit;
    /// The `dwWaitHint` of the same notification, in milliseconds. A shim reports `STOPPED` with
    /// 0; SCM's own failed recovery start leaves 2000 on its `STOPPED` (measured).
    fn stopped_wait_hint(&self) -> u32;
    /// Whether the notification the last [`Self::wait_callback`] returned had
    /// `SERVICE_ACCEPT_STOP` in its `dwControlsAccepted`. Read from that notification, not queried
    /// afterwards, for the reason [`Self::stopped_exit`] is.
    fn accepts_stop(&self) -> bool;
    /// A fresh `QueryServiceStatus`: the state, controls accepted, exit record and wait hint now.
    /// Used by [`Self::arm`] for its status, and for deciding what a refused STOP (1052 or 1061)
    /// meant. Not a wait and not read from a notification.
    fn query_status(&mut self) -> io::Result<QueriedStatus>;
}

/// Stop the service, gated strictly on a real `STOPPED` callback from
/// `NotifyServiceStatusChangeW`; re-arms after a non-terminal (pending)
/// callback, under the same `deadline` throughout.
///
/// The STOP is sent first and the wait armed after it, so only what follows the STOP decides: the
/// status the arm returns and the notifications after it. A registration made before the STOP would
/// fire at once for a `STOPPED` already current, and a service that failed to `STOPPED` just before it
/// can be running again, by a recovery restart, when the STOP lands; that stale `STOPPED` would confirm
/// a stop of a running service.
pub fn stop_via_notify<A: ScmActor>(a: &mut A, deadline: Deadline) -> io::Result<Waited> {
    // Issued whatever is left of the budget: only the *waiting* is bounded.
    control_stop_or_underway(a)?;
    wait_after_stop(a, deadline, " Run `stop` again.")
}

/// [`ScmActor::control_stop`], where a stop already under way counts as sent.
///
/// SCM refuses a STOP with 1061 (`ERROR_SERVICE_CANNOT_ACCEPT_CTRL`) or 1052
/// (`ERROR_INVALID_SERVICE_CONTROL`). Microsoft documents 1061 for a service that is `STOPPED`,
/// `START_PENDING` or `STOP_PENDING`, even one that accepts STOP; P13c measured 1052 for
/// `START_PENDING` with no STOP accepted, P18c and P26 1061 for `STOP_PENDING`, and P16a 1061 for
/// every STOP after one was delivered, while the service stays `RUNNING`. So the code decides
/// nothing; the status read after the refusal does, the same for both codes:
/// - `STOP_PENDING` or `STOPPED`: a stop is under way or done. Sent.
/// - `RUNNING` or `PAUSED`, accepting STOP, after 1052: the service was not yet accepting STOP when
///   the STOP was sent (a start was in progress) and is now. The STOP was not delivered: an error
///   says so and nothing waits.
/// - `RUNNING` or `PAUSED`, accepting STOP, after 1061: either a STOP was delivered earlier and is
///   unanswered (P16a), or the service was `START_PENDING` when this one was sent and is not now
///   (Microsoft's 1061), and nothing is stopping it. The read cannot tell them apart, so the STOP is
///   sent once more. `Ok` means it is sent now; 1061 means one was already delivered, so it is under
///   way. Any other refusal is decided by a fresh status read through this table, with no third STOP.
/// - `RUNNING` or `PAUSED`, not accepting STOP: nothing is stopping it, and nothing will. An error
///   names `goetia daemon kill`. `tests/scm_integration/managed.rs` (`stop` of the `refuse-stop`
///   fixture) measures this on CI.
/// - `START_PENDING`: told as [`STARTING`].
/// - any other state: the OS error.
fn control_stop_or_underway<A: ScmActor>(a: &mut A) -> io::Result<()> {
    match a.control_stop() {
        Err(e) if is_stop_refusal(&e) => decide_refused_stop(a, e, true),
        other => other,
    }
}

fn is_stop_refusal(e: &io::Error) -> bool {
    matches!(
        e.raw_os_error(),
        Some(ERROR_SERVICE_CANNOT_ACCEPT_CTRL | ERROR_INVALID_SERVICE_CONTROL)
    )
}

/// What the refusal `e` of a STOP means, from a fresh status read. `may_resend` is false once the
/// STOP was already sent a second time, so a refusal of that one is decided without a third.
fn decide_refused_stop<A: ScmActor>(a: &mut A, e: io::Error, may_resend: bool) -> io::Result<()> {
    let status = a.query_status()?;
    match (status.state, status.accepts_stop) {
        (QueriedState::StopPending | QueriedState::Stopped, _) => Ok(()),
        (QueriedState::Running | QueriedState::Paused, true) => {
            if e.raw_os_error() != Some(ERROR_SERVICE_CANNOT_ACCEPT_CTRL) {
                Err(io::Error::other(STOP_NOT_DELIVERED))
            } else if !may_resend {
                Ok(())
            } else {
                match a.control_stop() {
                    Err(e) if is_stop_refusal(&e) => decide_refused_stop(a, e, false),
                    other => other,
                }
            }
        }
        (state @ (QueriedState::Running | QueriedState::Paused), false) => Err(io::Error::other(format!(
            "the service is {state:?} and does not accept STOP; `goetia daemon kill` ends its process"
        ))),
        (QueriedState::StartPending, _) => Err(io::Error::other(STARTING)),
        _ => Err(e),
    }
}

/// What a STOP refused with 1052 is told when the service accepts STOP by the time the status is
/// read: the STOP reached it before it did, and is gone.
const STOP_NOT_DELIVERED: &str = "the STOP was not delivered: the service was not yet accepting STOP when it was sent, and accepts STOP now; run `stop` again";

/// What a STOP refused while SCM starts the service is told, as a recovery restart does until the
/// new shim reports `RUNNING`. SCM drops that STOP: nothing queues it (P13c, with 1052).
const STARTING: &str = "SCM is starting the service (for example a restart after a failure), so the STOP was not delivered and is not queued; run `stop` again once the service is running";

/// The wait half of a stop: the `Stopped` arm (`STOPPED` and `STOP_PENDING`) is already in place, made
/// after the STOP.
///
/// After a non-terminal callback, which is `STOP_PENDING`, it re-arms for `STOPPED` alone and decides
/// on the status [`ScmActor::arm`] returns: a service that moved on between the callback and the
/// re-arm would never wake the wait. A failure that races the stop goes `STOPPED (1066)` and SCM
/// restarts the service under its restart policy (P18a), which can reach `RUNNING` before the
/// re-arm. The read decides:
/// - `STOPPED`: confirmed. The re-arm's registration stays outstanding, which the actor's teardown
///   drains ([`close_then_drain`]); nothing arms it again.
/// - `STOP_PENDING`: wait on the registration, which fires on the `STOPPED` that ends it, even if a
///   restart follows at once.
/// - SCM refused the arm because the service is marked for deletion ([`Armed::Unwatchable`]): the
///   status it returned decides, `STOPPED` as confirmed and any other state as an error.
/// - any other state: the service was running or starting again after it began to stop, so the stop
///   did not leave it stopped; an error says so, ending in `advice`. It does not wait for the
///   restart and does not send a second STOP.
///
/// `STOPPED` alone, not the first arm's mask again. A re-arm with the same context and buffer on the
/// same handle is not delivered on an unchanged state (P20, P27 a); with a new context and buffer, or
/// on a new handle, it is delivered at once (P27 b, c, f). `arm` rewrites `self.notify` in place with
/// the same `self.status` context, so it is the first shape, except after a `reopen`
/// (`ERROR_SERVICE_NOTIFY_CLIENT_LAGGING`), which is a new handle. Over a `STOP_PENDING` already
/// reported, the first mask would therefore stay silent until the next change; nothing here depends
/// on a delivery at arm time, because the status returned by the arm decides.
fn await_stopped<A: ScmActor>(a: &mut A, deadline: Deadline, advice: &str) -> io::Result<Waited> {
    loop {
        match a.wait_callback(deadline)? {
            None => return Ok(Waited::Expired),
            Some(Observed::Stopped) => return Ok(Waited::Confirmed),
            // Running/Paused cannot arrive under the stop mask; they are treated as pending.
            Some(Observed::Running | Observed::Paused | Observed::Pending) => {
                let status = match decided_or_status(a.arm(WantState::StoppedOnly, deadline)?, QueriedState::Stopped)? {
                    Ok(status) => status,
                    Err(waited) => return Ok(waited),
                };
                match status.state {
                    QueriedState::Stopped => return Ok(Waited::Confirmed),
                    QueriedState::StopPending => {}
                    state => return Err(io::Error::other(not_left_stopped(state, advice))),
                }
            }
        }
    }
}

/// What a stop is told when the service is `state` again after it began to stop, as when SCM restarts
/// it after a failure under its restart policy.
fn not_left_stopped(state: QueriedState, advice: &str) -> String {
    let now = match state {
        QueriedState::Running => "running",
        QueriedState::StartPending => "starting",
        QueriedState::Paused => "paused",
        QueriedState::ContinuePending => "continuing",
        QueriedState::PausePending => "pausing",
        QueriedState::Stopped | QueriedState::StopPending => {
            unreachable!("{state:?} is a stop, not a service left running")
        }
    };
    format!(
        "the service is {now} again after it began to stop (for example SCM restarted it after a failure, under its restart policy), so the stop did not leave it stopped.{advice}"
    )
}

/// `ControlService(STOP)` answered `ERROR_INVALID_SERVICE_CONTROL`: the service is in a state that
/// does not accept it, `START_PENDING` among them (measured).
const ERROR_INVALID_SERVICE_CONTROL: i32 = 1052;
/// `ERROR_SERVICE_CANNOT_ACCEPT_CTRL`: a STOP is already delivered and the service has not yet
/// reported `STOP_PENDING`, or reports it (measured).
const ERROR_SERVICE_CANNOT_ACCEPT_CTRL: i32 = 1061;

/// Stop the service, whatever state it is in, and wait for `STOPPED`. A STOP is sent only on a
/// `RUNNING` or `PAUSED` state that accepts it, seen in a notification or in the status an arm returns,
/// never on an earlier observation.
///
/// Loops over notifications of `RUNNING`, `PAUSED` and `STOPPED`; a pending state is not in the mask,
/// so SCM does not report one until the service has settled. `STOPPED` ends the loop. On `RUNNING` or
/// `PAUSED` the STOP is sent; SCM may have changed the state since that notification, and then it
/// rejects the STOP with 1052 (a restart began) or 1061 (a STOP is already delivered, or the service
/// was `START_PENDING`). Either way the loop re-arms, and the status the arm returns decides, as a
/// re-arm with the same context and buffer on the same handle is not delivered on a state that is
/// already current (P20, P27 a; attended 5 s windows), whether or not it changed since the last
/// delivery:
/// - `STOPPED`: confirmed.
/// - `RUNNING` or `PAUSED` accepting STOP, after 1052: the restart that began has finished, so the
///   STOP is sent as for a notified `RUNNING`, and a refusal of it is handled the same way.
/// - `RUNNING` or `PAUSED` accepting STOP, after 1061: the read cannot tell a STOP delivered earlier
///   and unanswered (P16a) from a `START_PENDING` that has since ended, so the STOP is sent once more,
///   as [`decide_refused_stop`] does. `Ok` means it is sent; 1061 again means one is under way. Either
///   way the loop goes to the wait after an accepted STOP and does not return to this arm, so no
///   status read can cause another STOP and nothing spins.
/// - `RUNNING` or `PAUSED` not accepting STOP: an error, as for a notification.
/// - any other state: wait for the next notification.
///
/// No iteration limit and no timing; a settled state that refuses STOP is an error. A STOP SCM
/// accepts is followed by the same wait as [`stop_via_notify`]'s, armed after the STOP; that holds
/// once nothing can start the service again, which `uninstall` arranges by setting
/// `SERVICE_DISABLED` first: a restart queued before that, or after it, does not start (P21).
/// The arm after the STOP returns the status too: the shim can be `STOPPED` before it, which
/// confirms, while any other state waits (a `simple` service reads `RUNNING` until its shim takes the
/// stop). So the wait cannot miss a `STOPPED` that a restart follows: the STOP is sent only on a
/// `RUNNING` state, which an attempt in flight at the disable has already reached.
/// A disable that lands while SCM's restart attempt is in flight does not stop it: the attempt runs
/// to `RUNNING` and the status read right after the disable shows `START_PENDING` or `RUNNING`
/// (P23), so the loop sees its `RUNNING` and stops it like any start.
pub fn settle_and_stop<A: ScmActor>(a: &mut A, deadline: Deadline) -> io::Result<Waited> {
    // The first arm on a fresh actor fires at once for a current state in its mask (P20, P27), so
    // the status it returns is not needed, unless SCM refused to watch the service.
    if let Err(waited) = decided_or_status(
        a.arm(WantState::RunningPausedOrStopped, deadline)?,
        QueriedState::Stopped,
    )? {
        return Ok(waited);
    }
    // What the status returned by the latest re-arm showed, when it is a state to act on.
    let mut read = None;
    // The code of the refusal the latest STOP got, for the read that follows its re-arm.
    let mut refused = None;
    loop {
        let seen = match read.take() {
            Some(seen) => seen,
            None => {
                refused = None;
                match a.wait_callback(deadline)? {
                    None => return Ok(Waited::Expired),
                    Some(Observed::Stopped) => Seen::Stopped,
                    // Not in the mask; re-arm rather than act on it.
                    Some(Observed::Pending) => Seen::Pending,
                    Some(Observed::Running) => Seen::Settled {
                        state: "RUNNING",
                        accepts_stop: a.accepts_stop(),
                    },
                    Some(Observed::Paused) => Seen::Settled {
                        state: "PAUSED",
                        accepts_stop: a.accepts_stop(),
                    },
                }
            }
        };
        match seen {
            Seen::Stopped => return Ok(Waited::Confirmed),
            Seen::Pending => {
                let status = match decided_or_status(
                    a.arm(WantState::RunningPausedOrStopped, deadline)?,
                    QueriedState::Stopped,
                )? {
                    Ok(status) => status,
                    Err(waited) => return Ok(waited),
                };
                read = seen_in(status);
            }
            Seen::Settled { state, accepts_stop } => {
                if !accepts_stop {
                    return Err(io::Error::other(format!(
                        "the service is {state} and does not accept STOP"
                    )));
                }
                let resend = refused == Some(ERROR_SERVICE_CANNOT_ACCEPT_CTRL);
                match a.control_stop() {
                    Ok(()) => return wait_after_stop(a, deadline, ""),
                    Err(e) if resend && e.raw_os_error() == Some(ERROR_SERVICE_CANNOT_ACCEPT_CTRL) => {
                        return wait_after_stop(a, deadline, "");
                    }
                    Err(e) if is_stop_refusal(&e) => {
                        refused = e.raw_os_error();
                        let status = match decided_or_status(
                            a.arm(WantState::RunningPausedOrStopped, deadline)?,
                            QueriedState::Stopped,
                        )? {
                            Ok(status) => status,
                            Err(waited) => return Ok(waited),
                        };
                        read = seen_in(status);
                    }
                    Err(e) => return Err(e),
                }
            }
        }
    }
}

/// What [`settle_and_stop`] learned, from a notification or from the status an arm returned.
enum Seen {
    Stopped,
    /// A notification outside the mask.
    Pending,
    /// `RUNNING` or `PAUSED`, named as the errors name it.
    Settled {
        state: &'static str,
        accepts_stop: bool,
    },
}

/// The state to act on in `status`, if it is one: the settled states, and `None` for every other,
/// which is waited for.
fn seen_in(status: QueriedStatus) -> Option<Seen> {
    match status.state {
        QueriedState::Stopped => Some(Seen::Stopped),
        QueriedState::Running => Some(Seen::Settled {
            state: "RUNNING",
            accepts_stop: status.accepts_stop,
        }),
        QueriedState::Paused => Some(Seen::Settled {
            state: "PAUSED",
            accepts_stop: status.accepts_stop,
        }),
        QueriedState::StartPending
        | QueriedState::StopPending
        | QueriedState::ContinuePending
        | QueriedState::PausePending => None,
    }
}

/// The wait of a stop once the STOP is accepted or known to be under way: arm for `STOPPED`, which
/// cancels the settle registration if [`settle_and_stop`] acted on a read and left it unfired. The
/// status the arm returns confirms a service already `STOPPED`; any other waits, and `advice` ends the
/// error of a service that is running again.
///
/// Decided on the arm's status and the notifications after it only: this arm follows the STOP.
fn wait_after_stop<A: ScmActor>(a: &mut A, deadline: Deadline, advice: &str) -> io::Result<Waited> {
    let status = match decided_or_status(a.arm(WantState::Stopped, deadline)?, QueriedState::Stopped)? {
        Ok(status) => status,
        Err(waited) => return Ok(waited),
    };
    if status.state == QueriedState::Stopped {
        return Ok(Waited::Confirmed);
    }
    await_stopped(a, deadline, advice)
}

/// What a wait makes of an [`ScmActor::arm`] before it looks at any status: the registered arm's
/// status is `Ok`, and every other outcome ends the wait, as `Err(Waited)`. An expired arm is
/// [`Waited::Expired`]. SCM refuses to watch a service marked for deletion ([`Armed::Unwatchable`]),
/// so nothing can be waited for and the status read in the arm's place decides: the `wanted` state is
/// [`Waited::Confirmed`], and any other is the error [`marked_for_deletion`] says.
fn decided_or_status(armed: Armed, wanted: QueriedState) -> io::Result<Result<QueriedStatus, Waited>> {
    match armed {
        Armed::Registered(status) => Ok(Ok(status)),
        Armed::Expired => Ok(Err(Waited::Expired)),
        Armed::Unwatchable(status) if status.state == wanted => Ok(Err(Waited::Confirmed)),
        Armed::Unwatchable(status) => Err(marked_for_deletion(status.state)),
    }
}

/// What `StartServiceW` answered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StartReply {
    /// The SCM accepted the start.
    Accepted,
    /// `ERROR_SERVICE_ALREADY_RUNNING` (1056), with the state a follow-up
    /// query reported.
    AlreadyRunning(QueriedState),
}

/// `reply` to a caller for whom a service already on its way to running
/// counts as started — the plain `start` verb, idempotent per its trait doc
/// comment. See [`already_running_is_recoverable`].
fn accept_as_started(reply: StartReply) -> io::Result<()> {
    match reply {
        StartReply::Accepted => Ok(()),
        StartReply::AlreadyRunning(queried) if already_running_is_recoverable(queried) => Ok(()),
        StartReply::AlreadyRunning(QueriedState::StopPending) => Err(io::Error::other(STOPPING)),
        StartReply::AlreadyRunning(queried) => Err(io::Error::other(format!(
            "StartServiceW reported ERROR_SERVICE_ALREADY_RUNNING, but the service is actually \
             {queried:?} — neither Running nor StartPending, so nothing is on its way to running"
        ))),
    }
}

/// What a start over a `STOP_PENDING` service is told. SCM refuses the start (1056), so reporting it
/// as started would be false. What follows the stop is not known here: a failed daemon's teardown is
/// restarted by SCM under the service's restart policy (P18a, P26).
const STOPPING: &str = "the service is stopping (STOP_PENDING), so it cannot be started yet. SCM may start it again itself under its restart policy. Run the start again once it has stopped";

/// Start the service, gated strictly on a real `RUNNING` callback; re-arms
/// after a non-terminal callback, under the same `deadline` throughout —
/// which is what makes a service that chatters `START_PENDING` forever
/// terminate at the caller's budget instead of never.
///
/// A terminal `Stopped` before `Running` is read by `kind`. For `type: simple` it is a start when
/// the shim launched the daemon and the daemon then ended (the recorded exit is the shim's own
/// vocabulary, see [`RecordedExit::daemon_ran`]), except the incomplete-stop record: the daemon may
/// still be running, so that is a failed start; a shim failure is a failed start returning `Err`. A
/// `type: managed` service speaks SCM itself, and stopping with a service-specific code before
/// `Running` is how it reports a failed start, so for it any `Stopped` before `Running` is a failed
/// start. Neither blocks forever.
///
/// Critical ordering: arm `RUNNING` strictly AFTER issuing `start`, and decide on the status the arm
/// returns and the notifications after it. A registration made before the start would fire at once for
/// a `RUNNING` already current, and a run that then failed would have that stale `RUNNING` confirm a
/// start whose new run has reported nothing. A service that reached `RUNNING` (or stopped) before the
/// arm is seen in the status the arm returns, as is one a re-arm finds: a registration is not
/// delivered for a state already current, so the status is read before every wait.
///
/// An `ERROR_SERVICE_ALREADY_RUNNING` over a service the query reports
/// `Running` is the SCM's own answer that the goal is reached, so it is the
/// confirmation, whatever is left of the deadline — as launchd's `print` of a
/// running job is. Nothing is armed for it.
pub fn start_via_notify<A: ScmActor>(a: &mut A, deadline: Deadline, kind: Kind) -> io::Result<Waited> {
    // Issued whatever is left of the budget: only the *waiting* is bounded, and a service left
    // unstarted because goetia ran out of budget would be the worst of both answers.
    let reply = a.start()?;
    if reply == StartReply::AlreadyRunning(QueriedState::Running) {
        return Ok(Waited::Confirmed);
    }
    accept_as_started(reply)?;
    let joined_a_start = matches!(reply, StartReply::AlreadyRunning(_));
    let mut armed = a.arm(WantState::Running, deadline)?;
    loop {
        let status = match decided_or_status(armed, QueriedState::Running)? {
            Ok(status) => status,
            Err(waited) => return Ok(waited),
        };
        match status.state {
            QueriedState::Running => return Ok(Waited::Confirmed),
            QueriedState::Stopped => {
                return stopped_before_running(kind, joined_a_start, status.exit, status.wait_hint);
            }
            _ => {}
        }
        match a.wait_callback(deadline)? {
            None => return Ok(Waited::Expired),
            Some(Observed::Running) => return Ok(Waited::Confirmed),
            Some(Observed::Stopped) => {
                return stopped_before_running(kind, joined_a_start, a.stopped_exit(), a.stopped_wait_hint());
            }
            // `Paused` cannot arrive under the start mask; it is treated as pending. The re-arm is
            // not delivered for a state already current, so its status decides: the service may be
            // `RUNNING`, or have stopped, since the notification.
            Some(Observed::Paused | Observed::Pending) => armed = a.arm(WantState::Running, deadline)?,
        }
    }
}

/// What a terminal `Stopped` before `Running` means, from the `exit` record and `wait_hint` it carried:
/// those of the notification, or of the status read after a re-arm. See [`start_via_notify`] for
/// the reading of each record.
///
/// A terminal `Stopped` is not an expiry. For `type: simple` it is a start when the shim launched the
/// daemon and the daemon then ended before the wait saw Running, as `systemctl start` confirms the
/// exec. That holds for the daemon's own exit. A shim failure after the launch (a failed wait, an
/// unreadable exit status) is recorded as a shim failure, so the answer still depends on which the
/// wait sees first: a failed start for Stopped, a start for Running.
fn stopped_before_running(kind: Kind, joined_a_start: bool, exit: RecordedExit, wait_hint: u32) -> io::Result<Waited> {
    // An `AlreadyRunning` over `START_PENDING` joined a start that was not ours: SCM's own recovery
    // start. When that fails SCM reports `STOPPED (0, 0)` with a wait hint (P10), which the shim never
    // sets; no shim connected to SCM. Only `type: simple` has a shim: a managed host may leave a hint
    // itself, and it did run. This test runs before `daemon_ran`, which `(0, 0)` would satisfy.
    let failed_scm_start =
        kind == Kind::Simple && joined_a_start && (exit.win32, exit.specific) == (0, 0) && wait_hint != 0;
    if failed_scm_start {
        return Err(io::Error::other(
            "service stopped before reaching Running (failed start): the shim never connected to SCM, so no daemon ran",
        ));
    }
    // The shim's `(0, 1003)` says it could not show the daemon's tree gone: not a launch
    // to confirm. The notification may follow a `RUNNING` the wait never saw.
    if kind == Kind::Simple && exit.is_incomplete_stop() {
        return Err(io::Error::other(
            "service stopped with goetia-shim's incomplete-stop record (Win32 0, service-specific 1003; failed start): the shim could not show that the daemon's process tree was terminated, so the daemon may still be running; see the shim's log",
        ));
    }
    // `daemon_ran` is safe to ask here: a host that exits before it connects also
    // records `(0, 0)` with a wait hint (P25 c/d), and that is either our own start
    // failing with `StartServiceW` 1053, which never reaches this wait, or a joined
    // start, caught by `failed_scm_start` above.
    if kind == Kind::Simple && exit.daemon_ran() {
        return Ok(Waited::Confirmed);
    }
    Err(io::Error::other(format!(
        "service stopped before reaching Running (failed start): it recorded {exit}"
    )))
}

/// Issue the start request and return, waiting for nothing — the
/// `Budget::Immediate` path, where the caller has no budget to watch the
/// service with. A named function rather than a bare [`ScmActor::start`] at
/// the call site so that what happens when we do *not* wait is covered by
/// the same fake-actor tests as everything else.
pub fn request_start<A: ScmActor>(a: &mut A) -> io::Result<()> {
    accept_as_started(a.start()?)
}

/// `daemon restart --timeout 0`'s start: [`request_start`], except `ERROR_SERVICE_ALREADY_RUNNING`
/// is always a refusal: the queried `Running` may be the instance the unconfirmed stop just before
/// it is taking down, before the shim has reported `STOP_PENDING`.
pub fn request_start_after_stop<A: ScmActor>(a: &mut A) -> io::Result<()> {
    match a.start()? {
        StartReply::Accepted => Ok(()),
        StartReply::AlreadyRunning(QueriedState::StopPending) => Err(io::Error::other(STOPPING)),
        StartReply::AlreadyRunning(queried) => Err(io::Error::other(format!(
            "StartServiceW reported ERROR_SERVICE_ALREADY_RUNNING with the service {queried:?}: \
             the stop issued just before it has not taken effect, so the start was refused"
        ))),
    }
}

/// [`request_start`]'s mirror: issue the stop control and return.
pub fn request_stop<A: ScmActor>(a: &mut A) -> io::Result<()> {
    control_stop_or_underway(a)
}

// handle teardown =====================================================================================================

/// The teardown half of [`system::SystemScmActor`], behind a trait for the
/// same reason [`ScmActor`] is one: the thing that matters here is an
/// *order*, and an order is only assertable if a fake can record it.
pub trait ScmHandles {
    /// Close the per-service handle and the SCM connection.
    fn close_handles(&mut self);
    /// Whether an `arm` succeeded that no `wait_callback` has consumed.
    fn registration_outstanding(&self) -> bool;
    /// Run a notification the SCM has already queued to this thread, via a
    /// zero-duration alertable wait.
    fn drain_queued_apc(&mut self);
}

/// Close both handles, THEN drain a notification the SCM may already have
/// queued.
///
/// An `arm()` that immediate-fired (the service already matched the
/// requested mask at the moment of the call) queues an APC to THIS thread
/// regardless of what happens next, and closing the handles only stops
/// *future* notifications from being queued (per
/// `NotifyServiceStatusChangeW`'s documented remarks) — it does not un-queue
/// one already pending. Left undrained — a `control_stop`/`start` that
/// errored before the wait ever ran, or a wait that expired before the APC
/// arrived — it fires later, on some unrelated alertable wait elsewhere in
/// the process, against the `LastStatus`/`SERVICE_NOTIFY_2W` boxes the actor
/// is about to free: memory corruption, and a `fired` flag set on whatever
/// occupies that address by then.
///
/// Close first: draining first leaves a window in which SCM queues a fresh notification.
pub fn close_then_drain<H: ScmHandles>(h: &mut H) {
    h.close_handles();
    if h.registration_outstanding() {
        h.drain_queued_apc();
    }
}

/// The state a follow-up `query_status` reported — one variant per
/// `windows_service::service::ServiceState`, mirrored here so what an
/// `ERROR_SERVICE_ALREADY_RUNNING` means is a decision about a plain enum and
/// can be tested off Windows.
///
/// Deliberately not [`Observed`], which collapses `StartPending` and
/// `StopPending` into a single `Pending`: telling those two apart is the
/// entire content of the decision below.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QueriedState {
    Stopped,
    StartPending,
    StopPending,
    Running,
    ContinuePending,
    PausePending,
    Paused,
}

/// What `QueryServiceStatus` reported: the state, whether `SERVICE_ACCEPT_STOP` was among the
/// controls accepted, and the exit record and wait hint that a `Stopped` notification carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct QueriedStatus {
    pub state: QueriedState,
    pub accepts_stop: bool,
    /// The recorded exit codes, meaningful in `Stopped`.
    pub exit: RecordedExit,
    /// `dwWaitHint` in milliseconds.
    pub wait_hint: u32,
}

/// Whether 1056 still counts as a start, for the plain `start` verb: `Running` and `StartPending`
/// are states the service is heading to running from; every other state is not, and accepting it
/// would report a start that did not happen. `restart --timeout 0` never consults it
/// ([`request_start_after_stop`]).
#[must_use]
pub fn already_running_is_recoverable(queried: QueriedState) -> bool {
    matches!(queried, QueriedState::Running | QueriedState::StartPending)
}

/// `NotifyServiceStatusChangeW`'s "you are too far behind" return. Not among
/// `windows-sys`'s `Foundation` exports, so it is spelled out.
pub const ERROR_SERVICE_NOTIFY_CLIENT_LAGGING: u32 = 1294;

/// `ERROR_SERVICE_MARKED_FOR_DELETE`.
pub const ERROR_SERVICE_MARKED_FOR_DELETE: u32 = 1072;

/// The error for a wait over a service in `state` that SCM refused to watch (1072), when `state` is
/// not the one the wait is for.
pub fn marked_for_deletion(state: QueriedState) -> io::Error {
    io::Error::other(format!(
        "the service is marked for deletion, so SCM no longer reports its changes; it is {state:?}, not \
         the state waited for"
    ))
}

/// The two steps [`register_until_deadline`] drives: one attempt to register
/// the notification, and the lag remedy. Behind a trait so that the loop's
/// bound — the thing that is easy to lose — is assertable off Windows.
pub trait NotifyRegistrar {
    /// Register the notification, reporting `NotifyServiceStatusChangeW`'s
    /// raw return code (`0` on success).
    fn register(&mut self) -> io::Result<u32>;
    /// `ERROR_SERVICE_NOTIFY_CLIENT_LAGGING`'s documented remedy: "close the
    /// handle to the SCM, open a new handle, and call this function again".
    fn reopen(&mut self) -> io::Result<()>;
}

/// What [`register_until_deadline`] came to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Registration {
    Registered,
    /// The deadline ran out while SCM kept answering `ERROR_SERVICE_NOTIFY_CLIENT_LAGGING`.
    Expired,
    /// SCM answered `ERROR_SERVICE_MARKED_FOR_DELETE`.
    MarkedForDelete,
}

/// Register the notification, following the lag remedy for as long as
/// `deadline` allows.
///
/// This is the one loop in this module that is not the wait itself, and it
/// sits inside an operation the caller bounded — a lagging SCM would
/// otherwise make `--timeout 5s` block forever. The deadline is checked at
/// the TOP of every iteration, including the first, and is the only thing
/// that ends the loop: deliberately not a retry counter, which would be a
/// bound nobody asked for, where this one the caller did ask for.
pub fn register_until_deadline<R: NotifyRegistrar>(r: &mut R, deadline: Deadline) -> io::Result<Registration> {
    loop {
        if deadline.expired() {
            return Ok(Registration::Expired);
        }
        match r.register()? {
            0 => return Ok(Registration::Registered),
            ERROR_SERVICE_MARKED_FOR_DELETE => return Ok(Registration::MarkedForDelete),
            ERROR_SERVICE_NOTIFY_CLIENT_LAGGING => r.reopen()?, // re-arm against the fresh handle
            rc => return Err(io::Error::from_raw_os_error(rc as i32)),
        }
    }
}

#[cfg(windows)]
pub mod system;

#[cfg(windows)]
pub use system::SystemScmActor;

#[cfg(test)]
#[path = "wait_tests.rs"]
mod wait_tests;
