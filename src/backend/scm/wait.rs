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

/// The granular SCM operations [`stop_via_notify`]/[`start_via_notify`] need,
/// isolated so the ordering can be unit-tested with a fake rather than a real
/// service.
pub trait ScmActor {
    /// Register a status-change notification for `want`.
    /// `NotifyServiceStatusChangeW` is single-shot, so the sequence re-arms
    /// after every non-terminal callback.
    ///
    /// `deadline` bounds the one loop here that is not the wait itself: the
    /// `ERROR_SERVICE_NOTIFY_CLIENT_LAGGING` remedy, which reopens both
    /// handles and arms again. `Waited::Expired` iff the deadline ran out
    /// while the SCM kept answering that.
    fn arm(&mut self, want: WantState, deadline: Deadline) -> io::Result<Waited>;
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
}

/// Stop the service, gated strictly on a real `STOPPED` callback from
/// `NotifyServiceStatusChangeW`; re-arms after a non-terminal (pending)
/// callback, under the same `deadline` throughout.
pub fn stop_via_notify<A: ScmActor>(a: &mut A, deadline: Deadline) -> io::Result<Waited> {
    let armed = a.arm(WantState::Stopped, deadline)?;
    // Issued even when the arm expired: only the *waiting* is bounded.
    control_stop_or_underway(a)?;
    if armed == Waited::Expired {
        return Ok(Waited::Expired);
    }
    await_stopped(a, deadline)
}

/// [`ScmActor::control_stop`], where a stop already under way counts as sent. A service in
/// `STOP_PENDING` (a shim tearing the daemon down) is refused by SCM with 1061 before it reaches the
/// service (P18c), and the stop being asked for is the one in progress. Any other refusal is an error.
fn control_stop_or_underway<A: ScmActor>(a: &mut A) -> io::Result<()> {
    match a.control_stop() {
        Err(e) if e.raw_os_error() == Some(ERROR_SERVICE_CANNOT_ACCEPT_CTRL) => Ok(()),
        other => other,
    }
}

/// The wait half of a stop: the `Stopped`-mask arm is already in place. Re-arms after every
/// non-terminal callback.
fn await_stopped<A: ScmActor>(a: &mut A, deadline: Deadline) -> io::Result<Waited> {
    loop {
        match a.wait_callback(deadline)? {
            None => return Ok(Waited::Expired),
            Some(Observed::Stopped) => return Ok(Waited::Confirmed),
            // Running/Pending are non-terminal for a stop wait — re-arm and wait. `Paused` cannot
            // arrive under the stop mask; it is treated as pending.
            Some(Observed::Running | Observed::Paused | Observed::Pending) => {
                if a.arm(WantState::Stopped, deadline)? == Waited::Expired {
                    return Ok(Waited::Expired);
                }
            }
        }
    }
}

/// `ControlService(STOP)` answered `ERROR_INVALID_SERVICE_CONTROL`: the service is in a state that
/// does not accept it, `START_PENDING` among them (measured).
const ERROR_INVALID_SERVICE_CONTROL: i32 = 1052;
/// `ERROR_SERVICE_CANNOT_ACCEPT_CTRL`: a STOP is already delivered and the service has not yet
/// reported `STOP_PENDING`, or reports it (measured).
const ERROR_SERVICE_CANNOT_ACCEPT_CTRL: i32 = 1061;

/// Stop the service, whatever state it is in, and wait for `STOPPED`. A STOP is sent only on a
/// `RUNNING` or `PAUSED` notification that accepts it, never on an earlier observation.
///
/// Loops over notifications of `RUNNING`, `PAUSED` and `STOPPED`; a pending state is not in the mask,
/// so SCM does not report one until the service has settled. `STOPPED` ends the loop. On `RUNNING` or
/// `PAUSED` the STOP is sent; SCM may have changed the state since that notification, and then it
/// rejects the STOP with 1052 (a restart began) or 1061 (a STOP is already delivered). Either way the
/// loop re-arms and waits: a re-arm on a state that has not changed does not deliver it again (P20,
/// measured over an attended 5 s window), so after a 1061 the wait ends with the service's next
/// change, not with another STOP. No iteration limit and no timing; a settled state that refuses STOP
/// is an error. A STOP SCM accepts is followed by the same wait as [`stop_via_notify`]'s, armed after
/// the STOP; that holds once nothing can start the service again, which `uninstall` arranges by
/// setting `SERVICE_DISABLED` first: a restart queued before that, or after it, does not start (P21).
/// A disable that lands while SCM's restart attempt is in flight does not stop it: the attempt runs
/// to `RUNNING` and the status read right after the disable shows `START_PENDING` or `RUNNING`
/// (P23), so the loop sees its `RUNNING` and stops it like any start.
pub fn settle_and_stop<A: ScmActor>(a: &mut A, deadline: Deadline) -> io::Result<Waited> {
    loop {
        if a.arm(WantState::RunningPausedOrStopped, deadline)? == Waited::Expired {
            return Ok(Waited::Expired);
        }
        let state = match a.wait_callback(deadline)? {
            None => return Ok(Waited::Expired),
            Some(Observed::Stopped) => return Ok(Waited::Confirmed),
            // Not in the mask; go round rather than act on it.
            Some(Observed::Pending) => continue,
            Some(Observed::Running) => "RUNNING",
            Some(Observed::Paused) => "PAUSED",
        };
        if !a.accepts_stop() {
            return Err(io::Error::other(format!(
                "the service is {state} and does not accept STOP"
            )));
        }
        match a.control_stop() {
            Ok(()) => {
                if a.arm(WantState::Stopped, deadline)? == Waited::Expired {
                    return Ok(Waited::Expired);
                }
                return await_stopped(a, deadline);
            }
            Err(e)
                if matches!(
                    e.raw_os_error(),
                    Some(ERROR_INVALID_SERVICE_CONTROL | ERROR_SERVICE_CANNOT_ACCEPT_CTRL)
                ) => {}
            Err(e) => return Err(e),
        }
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

/// What a start over a `STOP_PENDING` service is told. SCM refuses the start (1056) and the service
/// then stops and stays down, so reporting it as started would be false.
const STOPPING: &str = "the service is stopping (STOP_PENDING), so it cannot be started yet: it is going down and will stay down. Run the start again once it has stopped";

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
/// Critical ordering: arm `RUNNING` strictly BEFORE issuing `start`, else the
/// service can reach `RUNNING` before the arm and the notification only
/// fires on the *next* entry into `RUNNING` — a hang.
///
/// An `ERROR_SERVICE_ALREADY_RUNNING` over a service the query reports
/// `Running` is the SCM's own answer that the goal is reached, so it is the
/// confirmation, whatever is left of the deadline — as launchd's `print` of a
/// running job is. The armed wait would only have immediate-fired on it.
pub fn start_via_notify<A: ScmActor>(a: &mut A, deadline: Deadline, kind: Kind) -> io::Result<Waited> {
    let armed = a.arm(WantState::Running, deadline)?;
    // Issued even when the arm expired: only the *waiting* is bounded, and a
    // service left unstarted because goetia ran out of budget while arming
    // would be the worst of both answers.
    let reply = a.start()?;
    if reply == StartReply::AlreadyRunning(QueriedState::Running) {
        return Ok(Waited::Confirmed);
    }
    accept_as_started(reply)?;
    let joined_a_start = matches!(reply, StartReply::AlreadyRunning(_));
    if armed == Waited::Expired {
        return Ok(Waited::Expired);
    }
    loop {
        match a.wait_callback(deadline)? {
            None => return Ok(Waited::Expired),
            Some(Observed::Running) => return Ok(Waited::Confirmed),
            // A terminal Stopped is not an expiry. For `type: simple` it is a start when the shim
            // launched the daemon and the daemon then ended before the wait saw Running, as
            // `systemctl start` confirms the exec. That holds for the daemon's own exit. A shim
            // failure after the launch (a failed wait, an unreadable exit status) is recorded as
            // a shim failure, so the answer still depends on which the wait sees first: a failed
            // start for Stopped, a start for Running.
            Some(Observed::Stopped) => {
                let exit = a.stopped_exit();
                // An `AlreadyRunning` over `START_PENDING` joined a start that was not ours: SCM's
                // own recovery start. When that fails SCM reports `STOPPED (0, 0)` with a wait
                // hint, which the shim never sets; no shim ran. Only `type: simple` has a shim: a
                // managed host may leave a hint itself, and it did run.
                let failed_scm_start = kind == Kind::Simple
                    && joined_a_start
                    && (exit.win32, exit.specific) == (0, 0)
                    && a.stopped_wait_hint() != 0;
                if failed_scm_start {
                    return Err(io::Error::other(
                        "service stopped before reaching Running (failed start): SCM's own start of it failed, so no shim or daemon ran",
                    ));
                }
                // The shim's `(0, 1003)` says it could not show the daemon's tree gone: not a launch
                // to confirm. The notification may follow a `RUNNING` the wait never saw.
                if kind == Kind::Simple && exit.is_incomplete_stop() {
                    return Err(io::Error::other(
                        "service stopped with goetia-shim's incomplete-stop record (Win32 0, service-specific 1003; failed start): the shim could not show that the daemon's process tree was terminated, so the daemon may still be running; see the shim's log",
                    ));
                }
                if kind == Kind::Simple && exit.daemon_ran() {
                    return Ok(Waited::Confirmed);
                }
                return Err(io::Error::other(format!(
                    "service stopped before reaching Running (failed start): it recorded {exit}"
                )));
            }
            // `Paused` cannot arrive under the start mask; it is treated as pending.
            Some(Observed::Paused | Observed::Pending) => {
                if a.arm(WantState::Running, deadline)? == Waited::Expired {
                    return Ok(Waited::Expired);
                }
            }
        }
    }
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

/// Register the notification, following the lag remedy for as long as
/// `deadline` allows.
///
/// This is the one loop in this module that is not the wait itself, and it
/// sits inside an operation the caller bounded — a lagging SCM would
/// otherwise make `--timeout 5s` block forever. The deadline is checked at
/// the TOP of every iteration, including the first, and is the only thing
/// that ends the loop: deliberately not a retry counter, which would be a
/// bound nobody asked for, where this one the caller did ask for.
pub fn register_until_deadline<R: NotifyRegistrar>(r: &mut R, deadline: Deadline) -> io::Result<Waited> {
    loop {
        if deadline.expired() {
            return Ok(Waited::Expired);
        }
        match r.register()? {
            0 => return Ok(Waited::Confirmed),
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
