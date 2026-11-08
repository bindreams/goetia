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
use std::ops::ControlFlow;
use std::sync::Arc;

use crate::backend::scm::exit_status::{ERROR_SERVICE_DOES_NOT_EXIST, RecordedExit, SHIM_REFUSED};
use crate::backend::scm::kill::HoldOutcome;
use crate::manager::budget::Deadline;
use crate::spec::Kind;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WantState {
    Stopped,
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
    pub(crate) fn states(self) -> &'static [QueriedState] {
        use QueriedState::{Paused, Running, StopPending, Stopped};
        match self {
            WantState::Stopped => &[Stopped, StopPending],
            // No `START_PENDING`: a service starting at the arm must not fire it.
            WantState::RunningPausedOrStopped => &[Running, Paused, Stopped],
            WantState::StoppedOnly => &[Stopped],
        }
    }
}

/// The service state a `wait_callback` observed. Distinct from [`WantState`]:
/// a callback can report a state that is neither what the caller wants nor
/// its opposite. `Running`, `Paused` and `Stopped` are settled; `Pending` is any intermediate
/// (`StartPending`/`StopPending`).
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
/// `#[must_use]`, like `Waited`: dropping it reads an expiry as a registration.
#[must_use = "an expired arm registered nothing; the status of a registered one is what the caller decides on"]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Armed {
    /// The deadline ran out while SCM kept answering `ERROR_SERVICE_NOTIFY_CLIENT_LAGGING`.
    Expired,
    /// The notification is registered. The status was read after the registration, so a state that
    /// is already current is seen here and not waited for.
    Registered(QueriedStatus),
    /// SCM refused the registration with `ERROR_SERVICE_MARKED_FOR_DELETE`. Nothing is registered, and
    /// the status read in its place is all there is to decide on.
    Unwatchable(QueriedStatus),
    /// Cancelling the earlier registration found the service deleted (`ERROR_SERVICE_DOES_NOT_EXIST`
    /// opening the fresh handle). Nothing is registered and no status can be read.
    Gone,
}

/// The granular SCM operations `stop_via_notify`/`start_via_notify` need,
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
    /// SCM refused a re-arm whose mask excludes `STOPPED`, on a marked `STOPPED` service, with
    /// `ERROR_SERVICE_MARKED_FOR_DELETE` (round 8); arms on marked `RUNNING` or pending services, and
    /// an arm whose mask includes `STOPPED` on a marked `STOPPED` one, were accepted (P13, P22a-d).
    /// That refusal is [`Armed::Unwatchable`] with the status read in its place.
    ///
    /// An `arm` over a registration no [`Self::wait_callback`] has consumed cancels that one first,
    /// as a caller that acts on the status leaves one behind. The registration reports into a
    /// buffer that cannot be reused until it has fired or been cancelled. The cancel opens the fresh
    /// handles before it closes the old: closing the last handle to a marked, `STOPPED` service
    /// deletes it (P12a, P13). If the service is gone anyway, the arm is [`Armed::Gone`].
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
    /// The exit code SCM recorded with the notification the last [`Self::wait_callback`] returned,
    /// for any state; the one that matters is a `Stopped`'s. Read from that notification, not queried
    /// afterwards: a recovery action may have restarted the service by then.
    fn delivered_exit(&self) -> RecordedExit;
    /// The `dwWaitHint` of the same notification, in milliseconds, for any state; the one that
    /// matters is a `Stopped`'s. Read from that notification, as [`Self::delivered_exit`] is. A shim
    /// reports `STOPPED` with 0; SCM's own failed recovery start leaves 2000 on its `STOPPED`
    /// (measured).
    fn delivered_wait_hint(&self) -> u32;
    /// The whole `dwControlsAccepted` of the notification the last [`Self::wait_callback`] returned.
    /// Read from that notification, as [`Self::delivered_exit`] is.
    fn delivered_controls_accepted(&self) -> u32;
    /// Whether the notification the last [`Self::wait_callback`] returned had
    /// `SERVICE_ACCEPT_STOP` in its `dwControlsAccepted`. Read from that notification, not queried
    /// afterwards, for the reason [`Self::delivered_exit`] is.
    fn accepts_stop(&self) -> bool {
        self.delivered_controls_accepted() & SERVICE_ACCEPT_STOP != 0
    }
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
pub(crate) fn stop_via_notify<A: ScmActor>(a: &mut A, deadline: Deadline) -> io::Result<Waited> {
    // Issued whatever is left of the budget: only the *waiting* is bounded.
    control_stop_or_underway(a)?;
    wait_after_stop(a, deadline, " Run `stop` again.", StopSeen::Requested)
}

/// How [`end_process`] ended.
#[must_use = "a process that outlasted the deadline is not gone; decide what to report rather than discarding it"]
#[derive(Debug)]
pub(crate) enum EndProcessOutcome<P> {
    /// The process has exited.
    Exited,
    /// The process had not exited when the deadline ran out. `terminate_failed` is why the attempt
    /// to end it failed, if it did.
    Lingers {
        process: P,
        terminate_failed: Option<Arc<io::Error>>,
    },
}

/// Ends `process`, the service's process held before its STOP, once SCM has confirmed `STOPPED`: the
/// one end of a held process, for `stop` and `uninstall` alike. SCM records `STOPPED` before the
/// process exits (a shim reports it and then exits), so `STOPPED` alone leaves a file the process
/// runs in use.
///
/// `terminate` ends the process, as the documentation of `SetServiceStatus` allows once
/// `SERVICE_STOPPED` is reported, and `exit` then waits for its exit under `deadline` and says whether
/// it did. Whatever `terminate` returns, the wait follows: SCM has confirmed the stop, so a failure
/// to end the process is no failure of the end, and the process may be on its way out by itself. The
/// failure is kept: `exit` is handed it for its notice, and [`Lingers`](EndProcessOutcome::Lingers)
/// carries it. An `exit` that fails is the error.
pub(crate) fn end_process<P>(
    process: P,
    deadline: Deadline,
    terminate: impl FnOnce(&P) -> io::Result<()>,
    exit: impl FnOnce(&P, Deadline, Option<&io::Error>) -> io::Result<Waited>,
) -> io::Result<EndProcessOutcome<P>> {
    let terminate_failed = terminate(&process).err().map(Arc::new);
    Ok(match exit(&process, deadline, terminate_failed.as_deref())? {
        Waited::Confirmed => EndProcessOutcome::Exited,
        Waited::Expired => EndProcessOutcome::Lingers {
            process,
            terminate_failed,
        },
    })
}

/// How [`stop_and_await_exit`] ended.
#[must_use = "an expiry is not a stop; decide what to report rather than discarding it"]
#[derive(Debug)]
pub(crate) enum StopAndAwaitExitOutcome<P> {
    /// SCM confirmed `STOPPED`, and the process held before the STOP, if any, has exited.
    Stopped,
    /// SCM had not confirmed `STOPPED` when the deadline ran out.
    ServiceExpired,
    /// SCM confirmed `STOPPED`, and this held process had not exited when the deadline ran out.
    /// `terminate_failed` is why the attempt to end it failed, if it did.
    ProcessExpired {
        process: P,
        terminate_failed: Option<Arc<io::Error>>,
    },
    /// The hold expired, with SCM still naming process `pid`, which was gone, so the STOP was sent with
    /// no process held, and nothing says that it took effect or which process it reached: not a stop,
    /// whatever SCM answers afterwards.
    Unconfirmed { pid: u32 },
}

/// [`stop_via_notify`], then, once SCM has confirmed `STOPPED`, the end of `held`, the service's
/// process as it was before the STOP was sent. `end` is [`end_process`] over the real process, under
/// the same `deadline`; a process that outlasts it is
/// [`ProcessExpired`](StopAndAwaitExitOutcome::ProcessExpired), not an unconfirmed stop, and carries
/// the failed terminate, if any. No process held (none reported, or none that could be opened) is the
/// stop alone.
///
/// An expired hold is sent the STOP all the same, and it ends there, whatever SCM answers: the held
/// process died (a crash), SCM may have a recovery restart pending, and a restarted process may have
/// reported `STOPPED` and still be exiting, so the STOP may have reached a process that nothing
/// holds, or none, and a `STOPPED` after it says nothing of any one's exit. That is
/// [`Unconfirmed`](StopAndAwaitExitOutcome::Unconfirmed), and nothing waits, for a STOP that finds
/// the service already stopped (`ERROR_SERVICE_NOT_ACTIVE`, or a refusal over a `STOPPED` read)
/// as for one that is delivered.
pub(crate) fn stop_and_await_exit<A: ScmActor, P>(
    a: &mut A,
    deadline: Deadline,
    held: HoldOutcome<P>,
    end: impl FnOnce(P, Deadline) -> io::Result<EndProcessOutcome<P>>,
) -> io::Result<StopAndAwaitExitOutcome<P>> {
    let held = match held {
        HoldOutcome::Held(process) => Some(process),
        // Why a refused process is not held is the caller's to report: it knows where.
        HoldOutcome::NoProcess | HoldOutcome::Refused(_) => None,
        HoldOutcome::Expired { pid } => {
            control_stop_or_underway(a)?;
            return Ok(StopAndAwaitExitOutcome::Unconfirmed { pid });
        }
    };
    if stop_via_notify(a, deadline)? == Waited::Expired {
        return Ok(StopAndAwaitExitOutcome::ServiceExpired);
    }
    let Some(process) = held else {
        return Ok(StopAndAwaitExitOutcome::Stopped);
    };
    Ok(match end(process, deadline)? {
        EndProcessOutcome::Exited => StopAndAwaitExitOutcome::Stopped,
        EndProcessOutcome::Lingers {
            process,
            terminate_failed,
        } => StopAndAwaitExitOutcome::ProcessExpired {
            process,
            terminate_failed,
        },
    })
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
///
/// It does not use [`wait_until`]: this loop ignores its first arm's status on purpose, and its
/// `RUNNING`/`PAUSED` action needs the actor, which `wait_until` holds.
///
/// `before_stop` runs on the actor just before each STOP is sent, once the state it is sent on has
/// settled: where a caller takes hold of what the service runs now, which no earlier read can say
/// (a start in flight at the disable replaces it). A STOP that is refused and sent again runs it
/// again. Its error ends the wait, with no STOP sent.
pub(crate) fn settle_and_stop<A: ScmActor>(
    a: &mut A,
    deadline: Deadline,
    mut before_stop: impl FnMut(&mut A) -> io::Result<()>,
) -> io::Result<Waited> {
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
                    Some(observed) => notified_state(observed)
                        .and_then(|state| seen_in(delivered_status(a, state)))
                        .unwrap_or(Seen::Pending),
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
                before_stop(a)?;
                match a.control_stop() {
                    Ok(()) => return wait_after_stop(a, deadline, "", StopSeen::Requested),
                    Err(e) if resend && e.raw_os_error() == Some(ERROR_SERVICE_CANNOT_ACCEPT_CTRL) => {
                        return wait_after_stop(a, deadline, "", StopSeen::Requested);
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
    /// A pending state.
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

/// What the caller of [`wait_after_stop`] has seen of the stop.
#[derive(Debug, Clone, Copy)]
pub enum StopSeen {
    /// The STOP was sent and nothing has shown yet: a first read other than `STOPPED` waits.
    Requested,
    /// `STOP_PENDING` was already seen: any first read other than `STOP_PENDING` or `STOPPED` fails.
    Pending,
}

/// The wait of a stop once the STOP is accepted or known to be under way: arm for `STOPPED` (and
/// `STOP_PENDING`), which cancels the settle registration if [`settle_and_stop`] acted on a read and left
/// it unfired.
///
/// After [`StopSeen::Requested`], what the first arm reads other than `STOPPED` waits: the STOP may not
/// have taken yet, and a `simple` service reads `RUNNING` until its shim takes it. Once a notification was delivered, the service had
/// begun to stop, and a re-arm that reads anything but `STOP_PENDING` means it did not stay stopped: a
/// failure that races the stop goes `STOPPED (1066)` and SCM restarts the service under its restart
/// policy (P18a), which can reach `RUNNING` before the re-arm. An error says so, ending in `advice`; it
/// does not wait for the restart and does not send a second STOP.
///
/// The re-arm is `STOPPED` alone, not the first mask again: after a lag reopen the new handle delivers
/// the current state at once (P27 c, f), which over an already-reported `STOP_PENDING` would wake the
/// wait with a state it has seen. [`wait_until`] documents the same reason for `rearm`.
///
/// `seen` is what the caller has already seen of the stop: after [`StopSeen::Requested`] the first arm
/// is `STOPPED` and `STOP_PENDING`, and a first read of anything else waits; after [`StopSeen::Pending`]
/// the first arm is `STOPPED` alone, and a first read of anything but `STOP_PENDING` or `STOPPED` fails.
pub fn wait_after_stop<A: ScmActor>(a: &mut A, deadline: Deadline, advice: &str, seen: StopSeen) -> io::Result<Waited> {
    wait_until(
        a,
        match seen {
            StopSeen::Requested => WantState::Stopped,
            StopSeen::Pending => WantState::StoppedOnly,
        },
        WantState::StoppedOnly,
        QueriedState::Stopped,
        deadline,
        |sighting| match sighting.status.state {
            _ if matches!(seen, StopSeen::Requested) && !sighting.pending_delivered => Ok(ControlFlow::Continue(())),
            QueriedState::StopPending => Ok(ControlFlow::Continue(())),
            state => Err(io::Error::other(not_left_stopped(state, advice))),
        },
    )
}

/// A state off the target that [`wait_until`] found, and what the wait knew of it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Sighting {
    /// After an arm, the status the arm returned. After a notification, the state it reported with the
    /// actor's buffered exit, wait hint and STOP acceptance for that delivery; after one of `PAUSED`,
    /// the status read in its place.
    pub status: QueriedStatus,
    /// Whether a notification of this wait was already delivered, and the wait re-armed after it.
    /// The first arm reads whatever state the service was in; after a delivery the service is on its
    /// way, and a re-arm that reads anything else says it moved on.
    pub pending_delivered: bool,
}

/// Wait for the service to be `target`: arm for `first`, decide on the status the arm returns, wait
/// for a notification, and on one that is not the target arm for `rearm` and decide again, under the
/// same `deadline` throughout and with no iteration limit. [`ScmActor::arm`] says why the status is
/// decided on before every wait: a registration is not delivered for a state already current.
///
/// `target`, in a status or a notification, is [`Waited::Confirmed`]. An arm that expired and a wait
/// that ran out of `deadline` are [`Waited::Expired`]. A service marked for deletion or gone is decided
/// as `decided_or_status` says. A notification of a pending state re-arms without asking `other`, and
/// so does one the armed mask excludes: SCM does not deliver a state its mask excludes; if one arrives
/// it is treated as pending. A notification of `PAUSED` is replaced by [`ScmActor::query_status`]: the
/// delivery may report the state at registration, and a paused service may have resumed since. Every
/// other state, from a status or a notification, goes to
/// `other` as a [`Sighting`]: `Break` ends the wait with that answer, an error ends it with the error,
/// and `Continue` keeps waiting (after a notification, by re-arming).
///
/// `rearm` is separate from `first` because a mask that suits the first arm can wake a re-arm for a
/// state already seen: the stop wait's re-arm is [`WantState::StoppedOnly`], as a new handle
/// delivers the current state at once (P27 c, f). A wait with no such state passes `first` twice.
pub fn wait_until<A: ScmActor>(
    a: &mut A,
    first: WantState,
    rearm: WantState,
    target: QueriedState,
    deadline: Deadline,
    mut other: impl FnMut(Sighting) -> io::Result<ControlFlow<Waited>>,
) -> io::Result<Waited> {
    let mut armed = Some(a.arm(first, deadline)?);
    // The states the latest arm registered for.
    let mut mask = first;
    let mut pending_delivered = false;
    loop {
        // An arm's status and a notification's are the same value from here on.
        let after_callback = armed.is_none();
        let status = match armed.take() {
            Some(armed) => match decided_or_status(armed, target)? {
                Ok(status) => Some(status),
                Err(waited) => return Ok(waited),
            },
            None => match a.wait_callback(deadline)? {
                None => return Ok(Waited::Expired),
                Some(observed) => match notified_state(observed).filter(|state| mask.states().contains(state)) {
                    // A delivery reports the state at registration, which the service can have left by now.
                    Some(QueriedState::Paused) => Some(a.query_status()?),
                    Some(state) => Some(delivered_status(a, state)),
                    None => None,
                },
            },
        };
        if let Some(status) = status {
            if status.state == target {
                return Ok(Waited::Confirmed);
            }
            if let ControlFlow::Break(waited) = other(Sighting {
                status,
                pending_delivered,
            })? {
                return Ok(waited);
            }
        }
        if after_callback {
            pending_delivered = true;
            mask = rearm;
            armed = Some(a.arm(rearm, deadline)?);
        }
    }
}

/// The state a notification reported, or `None` for a pending one.
fn notified_state(observed: Observed) -> Option<QueriedState> {
    match observed {
        Observed::Pending => None,
        Observed::Running => Some(QueriedState::Running),
        Observed::Paused => Some(QueriedState::Paused),
        Observed::Stopped => Some(QueriedState::Stopped),
    }
}

/// The status of a notification of `state`, as `a` buffered it: the whole record of that notification,
/// whatever its state; see [`ScmActor::delivered_exit`].
fn delivered_status<A: ScmActor>(a: &A, state: QueriedState) -> QueriedStatus {
    QueriedStatus {
        state,
        accepts_stop: a.accepts_stop(),
        exit: a.delivered_exit(),
        wait_hint: a.delivered_wait_hint(),
    }
}

/// What a wait makes of an [`ScmActor::arm`] before it looks at any status: the registered arm's
/// status is `Ok`, and every other outcome ends the wait, as `Err(Waited)`. An expired arm is
/// [`Waited::Expired`]. SCM refused to watch a service marked for deletion ([`Armed::Unwatchable`]),
/// so nothing can be waited for and the status read in the arm's place decides: the `wanted` state is
/// [`Waited::Confirmed`], and any other is the error [`marked_for_deletion`] says. A service that is gone
/// ([`Armed::Gone`]) is stopped, so it confirms a wait for `STOPPED` and is [`service_gone`] to any other.
fn decided_or_status(armed: Armed, wanted: QueriedState) -> io::Result<Result<QueriedStatus, Waited>> {
    match armed {
        Armed::Registered(status) => Ok(Ok(status)),
        Armed::Expired => Ok(Err(Waited::Expired)),
        Armed::Unwatchable(status) if status.state == wanted => Ok(Err(Waited::Confirmed)),
        Armed::Unwatchable(status) => Err(marked_for_deletion(status.state)),
        Armed::Gone if wanted == QueriedState::Stopped => Ok(Err(Waited::Confirmed)),
        Armed::Gone => Err(service_gone()),
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
        StartReply::AlreadyRunning(QueriedState::Paused) => Err(io::Error::other(PAUSED)),
        StartReply::AlreadyRunning(queried) => Err(io::Error::other(format!(
            "StartServiceW reported ERROR_SERVICE_ALREADY_RUNNING, but the service is actually \
             {queried:?}, so nothing is on its way to running"
        ))),
    }
}

/// What a start is told when the service is `PAUSED` instead of `RUNNING`, whether it settled so after
/// the start or SCM answered 1056 over it.
const PAUSED: &str = "the service is paused, not running: `sc continue` resumes it";

/// What a start over a `STOP_PENDING` service is told. SCM refuses the start (1056), so reporting it
/// as started would be false. What follows the stop is not known here: a failed daemon's teardown is
/// restarted by SCM under the service's restart policy (P18a, P26).
const STOPPING: &str = "the service is stopping (STOP_PENDING), so it cannot be started yet. SCM may start it again itself under its restart policy. Run the start again once it has stopped";

/// Start the service and wait for a settled state, from an arm's status or a notification, under the
/// same `deadline` throughout, so that a service that stays `START_PENDING` forever ends at the caller's
/// budget instead of never. `RUNNING` confirms, `STOPPED` is read by `kind` (below), and `PAUSED` is an
/// error: a paused service does not reach `RUNNING` by itself. A notified `PAUSED` is not decided on
/// alone, as the first arm may queue a delivery of the state at registration, which the service can have
/// left; the status read decides, and a state that is pending keeps the wait going.
///
/// A terminal `Stopped` before `Running` is read by `kind`. For `type: simple` it is a start when
/// the shim launched the daemon and the daemon then ended (the recorded exit is the shim's own
/// vocabulary, see [`RecordedExit::daemon_ran`]), except the incomplete-stop record: the daemon may
/// still be running, so that is a failed start; a shim failure or refusal is a failed start returning
/// `Err`. A `type: managed` service speaks SCM itself, and stopping with a service-specific code
/// before `Running` is how it reports a failed start, so for it any `Stopped` before `Running` is a
/// failed start. Neither blocks forever.
///
/// Critical ordering: arm the settled mask ([`WantState::RunningPausedOrStopped`]) strictly AFTER
/// issuing `start`, and decide on the status the arm returns and the notifications after it. A
/// registration made before the start would fire at once for a `RUNNING` already current, and a run
/// that then failed would have that stale `RUNNING` confirm a start whose new run has reported
/// nothing. The status is read after the registration and decides: a service already `RUNNING`,
/// `PAUSED` or `STOPPED` is seen there. The first arm on a fresh handle may also queue an at-once
/// delivery of the state at registration (P20, P27), which can be older than that status.
///
/// An `ERROR_SERVICE_ALREADY_RUNNING` over a service the query reports
/// `Running` is the SCM's own answer that the goal is reached, so it is the
/// confirmation, whatever is left of the deadline — as launchd's `print` of a
/// running job is. Nothing is armed for it.
pub(crate) fn start_via_notify<A: ScmActor>(a: &mut A, deadline: Deadline, kind: Kind) -> io::Result<Waited> {
    // Issued whatever is left of the budget: only the *waiting* is bounded, and a service left
    // unstarted because goetia ran out of budget would be the worst of both answers.
    let reply = a.start()?;
    if reply == StartReply::AlreadyRunning(QueriedState::Running) {
        return Ok(Waited::Confirmed);
    }
    accept_as_started(reply)?;
    let joined_a_start = matches!(reply, StartReply::AlreadyRunning(_));
    wait_until(
        a,
        WantState::RunningPausedOrStopped,
        WantState::RunningPausedOrStopped,
        QueriedState::Running,
        deadline,
        |sighting| match sighting.status.state {
            QueriedState::Stopped => {
                let status = sighting.status;
                stopped_before_running(kind, joined_a_start, status.exit, status.wait_hint).map(ControlFlow::Break)
            }
            QueriedState::Paused => Err(io::Error::other(PAUSED)),
            // Any other state waits: a pending one is what the wait is for.
            _ => Ok(ControlFlow::Continue(())),
        },
    )
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
    // The shim's `(0, SHIM_REFUSED)` says it started no daemon, though its Win32 code is 0.
    if kind == Kind::Simple && exit.is_refusal() {
        return Err(io::Error::other(format!(
            "service stopped with goetia-shim's refusal record (Win32 0, service-specific {SHIM_REFUSED}; failed start): the shim refused to start the daemon, so no daemon ran; see the shim's log"
        )));
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
pub(crate) fn request_start<A: ScmActor>(a: &mut A) -> io::Result<()> {
    accept_as_started(a.start()?)
}

/// `daemon restart --timeout 0`'s start: [`request_start`], except `ERROR_SERVICE_ALREADY_RUNNING`
/// is always a refusal: the queried `Running` may be the instance the unconfirmed stop just before
/// it is taking down, before the shim has reported `STOP_PENDING`.
pub(crate) fn request_start_after_stop<A: ScmActor>(a: &mut A) -> io::Result<()> {
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
pub(crate) fn request_stop<A: ScmActor>(a: &mut A) -> io::Result<()> {
    control_stop_or_underway(a)
}

// handle teardown =====================================================================================================

/// The teardown half of [`system::SystemScmActor`], behind a trait for the
/// same reason [`ScmActor`] is one: the thing that matters here is an
/// *order*, and an order is only assertable if a fake can record it.
pub(crate) trait ScmHandles {
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
pub(crate) fn close_then_drain<H: ScmHandles>(h: &mut H) {
    h.close_handles();
    if h.registration_outstanding() {
        h.drain_queued_apc();
    }
}

/// The fresh handles [`cancel_registration`] opens before it closes the old ones.
pub(crate) trait ReopenHandles: ScmHandles {
    /// What [`Self::open_fresh`] opens, held until [`Self::install`].
    type Fresh;
    /// Open handles to the service again. The actor's own handles stay open.
    fn open_fresh(&mut self) -> io::Result<Self::Fresh>;
    /// Take `fresh` as the actor's handles. The old ones are already closed.
    fn install(&mut self, fresh: Self::Fresh);
}

/// What [`cancel_registration`] came to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Cancelled {
    /// The old handles are closed and drained, and the fresh ones installed.
    Reopened,
    /// The service no longer exists. The old handles are closed and drained; nothing is installed.
    ServiceGone,
}

/// Cancel the registration `h` holds by replacing its handles: the registration ends with them.
///
/// The fresh handles are opened BEFORE the old are closed. Closing the last handle to a service marked
/// for deletion that is `STOPPED` deletes it (P12a, P13a/b), and the open after it would meet
/// `ERROR_SERVICE_DOES_NOT_EXIST`. The old handles then close and drain in [`close_then_drain`]'s order.
/// A 1060 from the open is [`Cancelled::ServiceGone`], after the same close and drain; any other error
/// leaves the old handles and their registration as they were, for the actor's teardown.
pub(crate) fn cancel_registration<H: ReopenHandles>(h: &mut H) -> io::Result<Cancelled> {
    let fresh = match h.open_fresh() {
        Ok(fresh) => fresh,
        Err(e) if e.raw_os_error() == Some(ERROR_SERVICE_DOES_NOT_EXIST as i32) => {
            close_then_drain(h);
            return Ok(Cancelled::ServiceGone);
        }
        Err(e) => return Err(e),
    };
    close_then_drain(h);
    h.install(fresh);
    Ok(Cancelled::Reopened)
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

/// Whether 1056 still counts as a start, for the plain `start` verb: `Running` and the states that are
/// not settled and may end in it (`StartPending`, `ContinuePending`, `PausePending`: a failing pause
/// returns to `Running`) count. Every other state does not, and accepting it would report a start that
/// did not happen. A wait over a recovered one tells `PAUSED` if it settles there. `restart --timeout 0`
/// never consults it ([`request_start_after_stop`]).
#[must_use]
pub(crate) fn already_running_is_recoverable(queried: QueriedState) -> bool {
    matches!(
        queried,
        QueriedState::Running | QueriedState::StartPending | QueriedState::ContinuePending | QueriedState::PausePending
    )
}

/// `NotifyServiceStatusChangeW`'s "you are too far behind" return. Not among
/// `windows-sys`'s `Foundation` exports, so it is spelled out.
pub(crate) const ERROR_SERVICE_NOTIFY_CLIENT_LAGGING: u32 = 1294;

/// `SERVICE_ACCEPT_STOP`, the `dwControlsAccepted` bit for STOP; spelled out as this module is built
/// off Windows too.
pub(crate) const SERVICE_ACCEPT_STOP: u32 = 0x1;

/// `ERROR_SERVICE_MARKED_FOR_DELETE`.
pub(crate) const ERROR_SERVICE_MARKED_FOR_DELETE: u32 = 1072;

/// The error for a wait over a service in `state` that SCM refused to watch (1072), when `state` is
/// not the one the wait is for.
pub(crate) fn marked_for_deletion(state: QueriedState) -> io::Error {
    io::Error::other(format!(
        "SCM refused to watch the service because it is marked for deletion; it is {state:?}, not the \
         state waited for"
    ))
}

/// The error for a wait for a state other than `STOPPED` over a service that was deleted meanwhile.
pub(crate) fn service_gone() -> io::Error {
    io::Error::other(
        "the service no longer exists (it was deleted while goetia waited), so it cannot reach the state waited for",
    )
}

/// The two steps [`register_until_deadline`] drives: one attempt to register
/// the notification, and the lag remedy. Behind a trait so that the loop's
/// bound — the thing that is easy to lose — is assertable off Windows.
pub(crate) trait NotifyRegistrar {
    /// Register the notification, reporting `NotifyServiceStatusChangeW`'s
    /// raw return code (`0` on success).
    fn register(&mut self) -> io::Result<u32>;
    /// `ERROR_SERVICE_NOTIFY_CLIENT_LAGGING`'s documented remedy: "close the
    /// handle to the SCM, open a new handle, and call this function again".
    fn reopen(&mut self) -> io::Result<()>;
}

/// What [`register_until_deadline`] came to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Registration {
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
pub(crate) fn register_until_deadline<R: NotifyRegistrar>(r: &mut R, deadline: Deadline) -> io::Result<Registration> {
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
pub(crate) mod system;

#[cfg(windows)]
pub use system::SystemScmActor;

#[cfg(test)]
#[path = "wait_tests.rs"]
mod wait_tests;
