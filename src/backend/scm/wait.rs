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
    /// The service is `STOPPED` with a restart queued: wait for the attempt to begin
    /// (`START_PENDING`) or to have succeeded (`RUNNING`). Not `STOPPED`, which the service
    /// already is, so the wait would fire at once.
    RestartBegins,
    /// The service is `START_PENDING` on a restart attempt: wait for it to succeed (`RUNNING`) or
    /// fail (`STOPPED`).
    RestartResolves,
}

/// The service state a `wait_callback` observed. Distinct from [`WantState`]:
/// a callback can report a state that is neither what the caller wants nor
/// its opposite. `Running`/`Stopped` are terminal; `Pending` is any
/// intermediate (`StartPending`/`StopPending`) and re-arms.
/// [`start_via_notify`] reads a terminal `Stopped` of a `type: simple` service through the exit
/// codes SCM recorded: the daemon having run is a start, the shim failing is a *failed* start
/// returning `Err`, and neither blocks forever.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Observed {
    Running,
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
    fn control_stop(&mut self) -> io::Result<ControlReply>;
    /// Close the service handle, which cancels its outstanding notification registration, and open
    /// a fresh one with the same access. One handle carries one registration, so this is the way to
    /// arm again after a registration that no callback consumed.
    fn reopen_service(&mut self) -> io::Result<()>;
    /// `DeleteService` on the open handle. The record goes when the last handle to the service
    /// closes, and SCM starts nothing for the service from this call on.
    fn delete(&mut self) -> io::Result<()>;
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
    /// The service's state now and the exit codes SCM holds for it (meaningful when `Stopped`).
    fn query_status(&mut self) -> io::Result<(QueriedState, RecordedExit)>;
    /// How the service's failure actions are configured.
    fn recovery(&mut self) -> io::Result<Recovery>;
}

/// The part of a service's failure-action configuration that decides whether a failure queues a
/// restart.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Recovery {
    /// Some failure action is `restart`.
    pub restarts: bool,
    /// The failure-actions flag: actions run for a stop with a non-zero exit code, and not only for
    /// a process that ended without reporting.
    pub on_non_crash_failures: bool,
}

/// `ERROR_PROCESS_ABORTED`: what SCM records for a process that ended without reporting `STOPPED`.
const ERROR_PROCESS_ABORTED: u32 = 1067;

/// Whether a service that reached `STOPPED` with `exit` has a restart queued: SCM queues recovery
/// for a non-zero `dwWin32ExitCode` alone, whatever the process exit code or the service-specific
/// code, and for a stop it commanded as much as for a crash.
#[must_use]
pub fn restart_queued(exit: RecordedExit, recovery: Recovery) -> bool {
    exit.win32 != 0 && recovery.restarts && (recovery.on_non_crash_failures || exit.win32 == ERROR_PROCESS_ABORTED)
}

/// [`restart_queued`], reading the configuration only for a failure: a clean stop is the common
/// case and has nothing queued whatever the configuration says.
fn queued_after<A: ScmActor>(a: &mut A, exit: RecordedExit) -> io::Result<bool> {
    if exit.win32 == 0 {
        return Ok(false);
    }
    Ok(restart_queued(exit, a.recovery()?))
}

/// What `ControlService(STOP)` answered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ControlReply {
    /// The service accepted the control.
    Sent,
    /// `ERROR_SERVICE_CANNOT_ACCEPT_CTRL` (1061) or `ERROR_SERVICE_NOT_ACTIVE` (1062): the service
    /// changed state after the caller read it, so the control met a state that refuses it.
    StateMoved,
    /// `ERROR_INVALID_SERVICE_CONTROL` (1052): what a `START_PENDING` service answers (measured),
    /// and also what a running service that does not accept `STOP` answers. Only the state read
    /// afterwards tells which.
    NotAccepted,
}

/// How [`stop_via_notify`] ended.
#[must_use = "an expiry did not confirm the stop; decide what to report rather than discarding it"]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopWaited {
    /// `STOPPED` with nothing queued after it.
    Confirmed,
    /// The deadline ran out while waiting for the stop it issued.
    Expired,
    /// The deadline ran out while SCM was starting the service, or a restart it had queued was
    /// still to arrive. Nothing was stopped.
    RestartPending,
}

/// What [`request_stop`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RequestedStop {
    Issued,
    /// SCM is starting the service, or has a restart queued, and a stop that waits for nothing
    /// cannot settle either: nothing was issued.
    RestartPending,
}

/// What the loop in [`stop_via_notify`] is waiting for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Watching {
    /// The `STOPPED` of the stop it issued.
    Stop,
    /// A restart SCM has queued, still to begin.
    RestartQueued,
    /// A start that has begun (a restart SCM made, or one requested), still to succeed or fail. The
    /// shim accepts no control before `RUNNING`, so a stop waits here rather than send one.
    RestartStarting,
}

/// What [`issue_stop`] did.
enum Stopping {
    /// The control was accepted. Carries whether the `STOPPED` arm that preceded it was registered.
    Sent(Waited),
    /// The control met a state that refuses it.
    Moved,
}

/// Whether a control the service answered with `ERROR_INVALID_SERVICE_CONTROL` was refused by its
/// state. SCM gives that answer to a service that is starting, stopping or stopped, and also to a
/// running one that does not accept `STOP` at all; only the state tells which. The first is a
/// service that moved, the second a refusal for good that no new read will change.
fn refused_by_state<A: ScmActor>(a: &mut A) -> io::Result<bool> {
    Ok(matches!(
        a.query_status()?.0,
        QueriedState::StartPending | QueriedState::StopPending | QueriedState::Stopped
    ))
}

/// The error for a service that is accepting controls and refuses `STOP`.
fn stop_not_accepted() -> io::Error {
    io::Error::other("the service does not accept the stop control (ERROR_INVALID_SERVICE_CONTROL)")
}

/// Send the stop control, reading what a refusal means. `None`: the control was accepted.
fn control<A: ScmActor>(a: &mut A) -> io::Result<Option<ControlReply>> {
    Ok(match a.control_stop()? {
        ControlReply::Sent => None,
        ControlReply::StateMoved => Some(ControlReply::StateMoved),
        ControlReply::NotAccepted if refused_by_state(a)? => Some(ControlReply::StateMoved),
        ControlReply::NotAccepted => return Err(stop_not_accepted()),
    })
}

/// Arm `STOPPED`, then issue the stop: the arm comes first, so a `STOPPED` reached before the
/// request is delivered still fires it. Issued even when the arm expired: only the *waiting* is
/// bounded.
fn issue_stop<A: ScmActor>(a: &mut A, deadline: Deadline) -> io::Result<Stopping> {
    let armed = a.arm(WantState::Stopped, deadline)?;
    Ok(match control(a)? {
        None => Stopping::Sent(armed),
        Some(_) => Stopping::Moved,
    })
}

/// Run `once` until it settles. `None` from it means the stop's control met a state that refuses
/// it because the service moved on after being read: close the handle, which drops the
/// registration `once` left behind, and read the service again from the top, under the same
/// `deadline`. A rerun is a fresh reading of a service that changed, never a retry of the same
/// request; `expired` is what a deadline that has run out by then settles as.
fn rerun_if_moved<A: ScmActor, T>(
    a: &mut A,
    deadline: Deadline,
    expired: T,
    mut once: impl FnMut(&mut A) -> io::Result<Option<T>>,
) -> io::Result<T> {
    loop {
        if let Some(settled) = once(a)? {
            return Ok(settled);
        }
        if deadline.expired() {
            return Ok(expired);
        }
        a.reopen_service()?;
    }
}

/// Stop the service, gated strictly on a real `STOPPED` callback from
/// `NotifyServiceStatusChangeW`; re-arms after a non-terminal (pending)
/// callback, under the same `deadline` throughout.
///
/// A control is sent only to a service in a state that accepts it. One found `START_PENDING` (a
/// restart beginning) is waited on for `RUNNING` (then stopped) or `STOPPED` (then judged as below);
/// one found `STOP_PENDING` is already stopping and is waited on for `STOPPED`; one found `STOPPED`
/// is judged as below. If the service moves between the read and the control, so that the control is
/// refused, the stop starts again from a fresh read (see `rerun_if_moved`).
///
/// A `STOPPED` is final only if SCM queued nothing after it. A non-zero Win32 code with recovery
/// actions configured queues a restart that no request cancels, whether the code is a daemon's
/// failure, or a stop's that met one as it happened. The stop then waits for the restart, within
/// the same `deadline`, and stops what it starts:
///
/// - `RUNNING`: stop it as for any running service; its clean exit queues nothing.
/// - `STOPPED` with Win32 0: SCM's own attempt to start the service failed, which is never retried.
/// - `STOPPED` with a non-zero code: the restarted process failed, and another restart is queued.
pub fn stop_via_notify<A: ScmActor>(a: &mut A, deadline: Deadline) -> io::Result<StopWaited> {
    rerun_if_moved(a, deadline, StopWaited::Expired, |a| stop_once(a, deadline))
}

/// A `STOPPED` reached, with `exit` recorded: `Some` if that settles the stop, `None` once a queued
/// restart is armed for and still to be waited on.
fn after_stopped<A: ScmActor>(a: &mut A, exit: RecordedExit, deadline: Deadline) -> io::Result<Option<StopWaited>> {
    if !queued_after(a, exit)? {
        return Ok(Some(StopWaited::Confirmed));
    }
    if a.arm(WantState::RestartBegins, deadline)? == Waited::Expired {
        return Ok(Some(StopWaited::RestartPending));
    }
    // The arm cannot hear of an attempt that began and failed before it: that ends in `STOPPED`,
    // the state the service is already in. Reading the status after the arm sees it, and anything
    // later is delivered to the arm.
    let (state, exit) = a.query_status()?;
    if state == QueriedState::Stopped && !queued_after(a, exit)? {
        return Ok(Some(StopWaited::Confirmed));
    }
    Ok(None)
}

/// One pass of [`stop_via_notify`]; `None` when the control was refused and the service must be
/// read again.
fn stop_once<A: ScmActor>(a: &mut A, deadline: Deadline) -> io::Result<Option<StopWaited>> {
    let (state, exit) = a.query_status()?;
    let mut watching = match state {
        QueriedState::StartPending => {
            if a.arm(WantState::RestartResolves, deadline)? == Waited::Expired {
                return Ok(Some(StopWaited::RestartPending));
            }
            Watching::RestartStarting
        }
        QueriedState::StopPending => {
            if a.arm(WantState::Stopped, deadline)? == Waited::Expired {
                return Ok(Some(StopWaited::Expired));
            }
            Watching::Stop
        }
        QueriedState::Stopped => {
            if let Some(settled) = after_stopped(a, exit, deadline)? {
                return Ok(Some(settled));
            }
            Watching::RestartQueued
        }
        _ => match issue_stop(a, deadline)? {
            Stopping::Moved => return Ok(None),
            Stopping::Sent(Waited::Expired) => return Ok(Some(StopWaited::Expired)),
            Stopping::Sent(Waited::Confirmed) => Watching::Stop,
        },
    };
    loop {
        let Some(observed) = a.wait_callback(deadline)? else {
            return Ok(Some(match watching {
                Watching::Stop => StopWaited::Expired,
                Watching::RestartQueued | Watching::RestartStarting => StopWaited::RestartPending,
            }));
        };
        match (watching, observed) {
            (_, Observed::Stopped) => {
                let exit = a.stopped_exit();
                if let Some(settled) = after_stopped(a, exit, deadline)? {
                    return Ok(Some(settled));
                }
                watching = Watching::RestartQueued;
            }
            // Running/Pending are non-terminal for a stop wait — re-arm and wait.
            (Watching::Stop, Observed::Running | Observed::Pending) => {
                if a.arm(WantState::Stopped, deadline)? == Waited::Expired {
                    return Ok(Some(StopWaited::Expired));
                }
            }
            (Watching::RestartQueued | Watching::RestartStarting, Observed::Pending) => {
                watching = Watching::RestartStarting;
                if a.arm(WantState::RestartResolves, deadline)? == Waited::Expired {
                    return Ok(Some(StopWaited::RestartPending));
                }
            }
            (Watching::RestartQueued | Watching::RestartStarting, Observed::Running) => {
                watching = Watching::Stop;
                match issue_stop(a, deadline)? {
                    Stopping::Moved => return Ok(None),
                    Stopping::Sent(Waited::Expired) => return Ok(Some(StopWaited::Expired)),
                    Stopping::Sent(Waited::Confirmed) => {}
                }
            }
        }
    }
}

/// `uninstall`'s teardown on one handle: stop the service, delete it, then stop whatever started
/// in between.
///
/// 1. Stop first, so a service that will not stop is left intact rather than marked for deletion.
///    A `START_PENDING` service (a restart beginning) refuses controls, so it is waited on until
///    it runs, and then stopped, or stops. A `STOPPED` service is not stopped, and no stop waits
///    for a queued restart: the delete in step 2
///    cancels the one SCM has queued, and a daemon that fails as soon as it is spawned (when
///    `uninstall` is used) would keep such a wait from ever ending. A failure here leaves the
///    service as it was.
/// 2. Delete. From then on SCM starts nothing for the service, a queued restart included
///    (measured, P12).
/// 3. Read the state again. A restart that fired between 1 and 2 is stopped now; nothing can start
///    again, so this too ignores restarts. Only this step can leave a running service marked for
///    deletion, and its error says so.
///
/// The service goes when the caller closes the handle afterwards. Every error says whether the
/// delete had happened.
pub fn uninstall_teardown<A: ScmActor>(a: &mut A, deadline: Deadline) -> io::Result<Waited> {
    let stopped = stop_unless_stopped(a, deadline).map_err(|e| {
        io::Error::new(
            e.kind(),
            format!(
                "the service did not confirm stopped ({e}) and was NOT deleted: deleting a running \
                 service only marks it for deletion, which the next install meets as \
                 ERROR_SERVICE_MARKED_FOR_DELETE. Stop it, or wait for it to stop, and uninstall again"
            ),
        )
    })?;
    if stopped == Waited::Expired {
        return Ok(Waited::Expired);
    }
    a.delete()
        .map_err(|e| io::Error::new(e.kind(), format!("the service was NOT deleted: {e}")))?;
    stop_unless_stopped(a, deadline).map_err(|e| {
        io::Error::new(
            e.kind(),
            format!(
                "the service is deleted but not confirmed stopped ({e}); it goes when its process ends, \
                 and until then the next install meets ERROR_SERVICE_MARKED_FOR_DELETE"
            ),
        )
    })
}

/// A `STOPPED` service is left alone. A `START_PENDING` one refuses controls, so it is waited on
/// until it is `RUNNING` (then stopped) or `STOPPED`; a `STOP_PENDING` one is waited on for
/// `STOPPED`. Any other state is stopped. Either way the wait ends on any `STOPPED`, whatever
/// follows it. A control refused because the service moved starts again from a fresh read.
fn stop_unless_stopped<A: ScmActor>(a: &mut A, deadline: Deadline) -> io::Result<Waited> {
    rerun_if_moved(a, deadline, Waited::Expired, |a| stop_unless_stopped_once(a, deadline))
}

/// One pass of [`stop_unless_stopped`]; `None` when the control was refused.
fn stop_unless_stopped_once<A: ScmActor>(a: &mut A, deadline: Deadline) -> io::Result<Option<Waited>> {
    let mut watching = match a.query_status()?.0 {
        QueriedState::Stopped => return Ok(Some(Waited::Confirmed)),
        QueriedState::StartPending => {
            if a.arm(WantState::RestartResolves, deadline)? == Waited::Expired {
                return Ok(Some(Waited::Expired));
            }
            Watching::RestartStarting
        }
        QueriedState::StopPending => {
            if a.arm(WantState::Stopped, deadline)? == Waited::Expired {
                return Ok(Some(Waited::Expired));
            }
            Watching::Stop
        }
        _ => match issue_stop(a, deadline)? {
            Stopping::Moved => return Ok(None),
            Stopping::Sent(Waited::Expired) => return Ok(Some(Waited::Expired)),
            Stopping::Sent(Waited::Confirmed) => Watching::Stop,
        },
    };
    loop {
        let rearm = match (watching, a.wait_callback(deadline)?) {
            (_, None) => return Ok(Some(Waited::Expired)),
            (_, Some(Observed::Stopped)) => return Ok(Some(Waited::Confirmed)),
            (Watching::RestartStarting, Some(Observed::Running)) => {
                watching = Watching::Stop;
                match issue_stop(a, deadline)? {
                    Stopping::Moved => return Ok(None),
                    Stopping::Sent(Waited::Expired) => return Ok(Some(Waited::Expired)),
                    Stopping::Sent(Waited::Confirmed) => continue,
                }
            }
            (Watching::RestartStarting, Some(Observed::Pending)) => WantState::RestartResolves,
            (_, Some(Observed::Running | Observed::Pending)) => WantState::Stopped,
        };
        if a.arm(rearm, deadline)? == Waited::Expired {
            return Ok(Some(Waited::Expired));
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
        StartReply::AlreadyRunning(queried) => Err(io::Error::other(format!(
            "StartServiceW reported ERROR_SERVICE_ALREADY_RUNNING, but the service is actually \
             {queried:?} — neither Running nor StartPending, so nothing is on its way to running"
        ))),
    }
}

/// Start the service, gated strictly on a real `RUNNING` callback; re-arms
/// after a non-terminal callback, under the same `deadline` throughout —
/// which is what makes a service that chatters `START_PENDING` forever
/// terminate at the caller's budget instead of never.
///
/// A terminal `Stopped` before `Running` is read by `kind`. For `type: simple` it is a start when
/// the shim launched the daemon and the daemon then ended (the recorded exit is the shim's own
/// vocabulary, see [`RecordedExit::daemon_ran`]). A `type: managed` service speaks SCM itself, and
/// stopping with a service-specific code before `Running` is how it reports a failed start, so for
/// it any `Stopped` before `Running` is a failed start.
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
    match a.start()? {
        StartReply::AlreadyRunning(QueriedState::Running) => return Ok(Waited::Confirmed),
        reply => accept_as_started(reply)?,
    }
    if armed == Waited::Expired {
        return Ok(Waited::Expired);
    }
    loop {
        match a.wait_callback(deadline)? {
            None => return Ok(Waited::Expired),
            Some(Observed::Running) => return Ok(Waited::Confirmed),
            // A terminal Stopped is not an expiry. For `type: simple` it is a start when the shim
            // launched the daemon and the daemon then ended before the wait saw Running:
            // confirming the launch, as `systemctl start` confirms the exec, keeps the answer
            // from depending on whether the wait saw Running or Stopped first. Otherwise the
            // daemon never ran.
            Some(Observed::Stopped) => {
                let exit = a.stopped_exit();
                if kind == Kind::Simple && exit.daemon_ran() {
                    return Ok(Waited::Confirmed);
                }
                return Err(io::Error::other(format!(
                    "service stopped before reaching Running (failed start): it recorded {exit}"
                )));
            }
            Some(Observed::Pending) => {
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

/// `daemon restart --timeout 0`'s start: [`request_start`], except that
/// `ERROR_SERVICE_ALREADY_RUNNING` is a refusal whatever the query says. The
/// stop just before it was issued and not waited for, and a `type: simple`
/// service reads `RUNNING` for its whole teardown — `goetia-shim` never
/// reports `STOP_PENDING` — so a queried `Running` here may be the very
/// instance that stop is taking down.
pub fn request_start_after_stop<A: ScmActor>(a: &mut A) -> io::Result<()> {
    match a.start()? {
        StartReply::Accepted => Ok(()),
        StartReply::AlreadyRunning(queried) => Err(io::Error::other(format!(
            "StartServiceW reported ERROR_SERVICE_ALREADY_RUNNING with the service {queried:?}: \
             the stop issued just before it has not taken effect, so the start was refused"
        ))),
    }
}

/// [`request_start`]'s mirror: issue the stop control and return, unless the service is `STOPPED`
/// with a restart queued, or `START_PENDING`. A stop cannot cancel that restart, one that waits for
/// nothing cannot stop what it starts, and a starting service refuses the control, so it reports
/// the restart and issues nothing. A `STOPPED` service with nothing queued, or a `STOP_PENDING`
/// one, is already as stopped as the request can make it and is sent nothing.
///
/// A control refused because the service moved after the read starts again from a fresh read.
pub fn request_stop<A: ScmActor>(a: &mut A) -> io::Result<RequestedStop> {
    loop {
        let (state, exit) = a.query_status()?;
        match state {
            QueriedState::StartPending => return Ok(RequestedStop::RestartPending),
            QueriedState::Stopped if queued_after(a, exit)? => return Ok(RequestedStop::RestartPending),
            QueriedState::Stopped | QueriedState::StopPending => return Ok(RequestedStop::Issued),
            _ => {}
        }
        if control(a)?.is_none() {
            return Ok(RequestedStop::Issued);
        }
    }
}

// hoisted from `mod system` ===========================================================================================

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
/// Closing first is what makes the drain final. Draining first leaves a
/// window in which the SCM can queue a fresh notification between the drain
/// returning and the handle closing — and on the expiry path, where the APC
/// has usually not been queued yet, that window is the likely case, not the
/// unlikely one.
///
/// What the fake in `wait_tests.rs` pins is exactly that SEQUENCE, and no
/// more. That `Service`'s `Drop` really closes the `SC_HANDLE`, that the SCM
/// really stops queuing once it is closed, and that `Drop for
/// SystemScmActor` reaches this function at all stay verifiable only on
/// Windows CI.
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

/// Whether a `StartServiceW` that failed with `ERROR_SERVICE_ALREADY_RUNNING`
/// (1056) still counts as a start, given the state a follow-up query
/// reported — for the plain `start` verb only: [`start_via_notify`], which
/// has a wait armed, and [`request_start`], which arms nothing.
/// [`request_start_after_stop`] never consults it.
///
/// 1056 means only "`dwCurrentState` is not `SERVICE_STOPPED`", which lumps
/// together states the service is on its way to running from and states it
/// is not. `Running` and `StartPending` are the two it is — for a caller that
/// did not just issue a stop. For `restart --timeout 0` a queried `Running`
/// may be the instance its own unconfirmed stop is taking down, which is why
/// that caller treats every 1056 as a refusal instead. For a plain start
/// nothing further is needed — which is the whole of what [`request_start`]
/// reports. The armed wait confirms `StartPending` (`want_to_mask` arms
/// `SERVICE_NOTIFY_START_PENDING` in *both* of its `WantState::Running`
/// branches, so it immediate-fires), and takes a queried `Running` as its
/// confirmation outright (see [`start_via_notify`]). `StartPending`
/// in particular is what a service is in when something else started it a
/// moment earlier — rejecting it fails a start that was going to succeed.
/// For every other state the service is not heading for running at all, so
/// accepting 1056 would report a start that did not happen — and on the
/// waiting path the registration holds no bit that can fire, so it would
/// also block until the deadline.
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

#[cfg(test)]
#[path = "wait_stop_paths_tests.rs"]
mod wait_stop_paths_tests;
