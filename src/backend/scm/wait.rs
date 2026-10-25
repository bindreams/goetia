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
use crate::backend::scm::state_file::{ShimState, Survivor};
use crate::manager::budget::Deadline;
use crate::spec::Kind;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WantState {
    Stopped,
    Running,
    /// The service is `START_PENDING`: wait for the start to succeed (`RUNNING`) or fail
    /// (`STOPPED`).
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
    /// The service's state now, the exit codes SCM holds for it (meaningful when `Stopped`) and
    /// whether it accepts `STOP`.
    fn query_status(&mut self) -> io::Result<QueriedStatus>;
    /// How the service's failure actions are configured.
    fn recovery(&mut self) -> io::Result<Recovery>;
    /// What the shim's state file and the clock say, for a `type: simple` service; `None` for one
    /// with no shim (`type: managed`), whose pending restarts are judged by its status alone.
    /// The file is read afresh by every call.
    fn shim_state(&mut self) -> io::Result<Option<ShimState>>;
    /// The daemon an earlier incomplete stop could not kill, if the state file names one that is
    /// still running (or cannot be told not to be): its pid, and which of the two it is.
    fn survivor(&mut self) -> io::Result<Option<(u32, Survivor)>>;
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

/// What a status query read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct QueriedStatus {
    pub state: QueriedState,
    /// The raw `(dwWin32ExitCode, dwServiceSpecificExitCode)` pair.
    pub exit: RecordedExit,
    /// `dwControlsAccepted` includes `SERVICE_ACCEPT_STOP`.
    pub accepts_stop: bool,
}

/// Whether a service in `state` (`STOPPED` with `exit` recorded, when it is stopped) has a restart
/// pending. The one place that decides it, for every stop path.
///
/// A `type: simple` service is judged by the shim's state file ([`ShimState::restart_pending`]),
/// whatever its state: a restart queued before a manual start is still to fire. The one thing the
/// file cannot hold is a failure the shim never got to report: it died (killed, crashed), leaving
/// the `ERROR_PROCESS_ABORTED` it never reports itself, and a record that does not call a restart
/// pending does not cover that. Such a service is judged by its status, as any other service is.
///
/// A service with no shim is judged by its status alone. SCM queues recovery for a non-zero
/// `dwWin32ExitCode`, whatever the process exit code or the service-specific code, and for a stop
/// it commanded as much as for a crash. That rule reads the record and the failure actions as they
/// are now; it cannot see a restart queued under other actions or hidden by a later start (see the
/// README).
fn restart_pending<A: ScmActor>(a: &mut A, state: QueriedState, exit: RecordedExit) -> io::Result<bool> {
    let stopped = state == QueriedState::Stopped;
    if let Some(shim) = a.shim_state()? {
        if shim.restart_pending(stopped && exit.win32 != 0) {
            return Ok(true);
        }
        if stopped && exit.win32 == ERROR_PROCESS_ABORTED && !shim.disabled {
            return status_rule(a, exit);
        }
        return Ok(false);
    }
    // A clean stop is the common case and has nothing pending whatever the configuration says.
    if !stopped || exit.win32 == 0 {
        return Ok(false);
    }
    status_rule(a, exit)
}

/// Whether the failure actions, as they are now, queue a restart for a `STOPPED` with `exit`.
fn status_rule<A: ScmActor>(a: &mut A, exit: RecordedExit) -> io::Result<bool> {
    let recovery = a.recovery()?;
    Ok(recovery.restarts && (recovery.on_non_crash_failures || exit.win32 == ERROR_PROCESS_ABORTED))
}

/// Whether a daemon the shim could not kill may still be running after a `STOPPED` with `exit`.
/// A `type: simple` service is judged by the survivor its shim recorded, which ends with the
/// daemon; the status record of the incomplete stop outlives it. The shim records the survivor
/// before it reports that status, but may fail to: with the status and no survivor in the file,
/// nothing says the daemon is gone, so it may be running. A service with no shim is judged by the
/// status alone.
fn daemon_may_still_run<A: ScmActor>(a: &mut A, exit: RecordedExit) -> io::Result<bool> {
    let Some(shim) = a.shim_state()? else {
        return Ok(exit.incomplete_stop());
    };
    if a.survivor()?.is_some() {
        return Ok(true);
    }
    Ok(exit.incomplete_stop() && !shim.names_a_survivor())
}

/// What a `STOPPED` with `exit` recorded settles a stop as: a daemon that may still be running, a
/// restart that nothing cancels, or a stopped service.
fn settle<A: ScmActor>(a: &mut A, exit: RecordedExit) -> io::Result<StopWaited> {
    if daemon_may_still_run(a, exit)? {
        return Ok(StopWaited::Incomplete);
    }
    Ok(if restart_pending(a, QueriedState::Stopped, exit)? {
        StopWaited::RestartPending
    } else {
        StopWaited::Confirmed
    })
}

/// `uninstall` stops on any `STOPPED`, whatever follows it: the delete cancels a restart.
fn stopped_result<A: ScmActor>(a: &mut A, exit: RecordedExit) -> io::Result<StopWaited> {
    Ok(if daemon_may_still_run(a, exit)? {
        StopWaited::Incomplete
    } else {
        StopWaited::Confirmed
    })
}

/// What `ControlService(STOP)` answered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ControlReply {
    /// The service accepted the control.
    Sent,
    /// `ERROR_SERVICE_CANNOT_ACCEPT_CTRL` (1061) or `ERROR_SERVICE_NOT_ACTIVE` (1062): the control
    /// met a state that refuses it. Whether the service moved to that state is read from the
    /// service afterwards (see [`control`]).
    StateMoved,
    /// `ERROR_INVALID_SERVICE_CONTROL` (1052): what a `START_PENDING` service answers (measured),
    /// and also what a running service that does not accept `STOP` answers. Read like
    /// [`Self::StateMoved`].
    NotAccepted,
}

/// How [`stop_via_notify`] ended.
#[must_use = "an expiry did not confirm the stop; decide what to report rather than discarding it"]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopWaited {
    /// `STOPPED` with nothing pending after it.
    Confirmed,
    /// The deadline ran out while waiting for the stop it issued.
    Expired,
    /// SCM is starting the service, or has a restart queued that nothing cancels. Nothing was
    /// stopped.
    RestartPending,
    /// `STOPPED`, but the shim could not kill the daemon, which may still be running.
    Incomplete,
}

/// What [`request_stop`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RequestedStop {
    Issued,
    /// SCM is starting the service, or has a restart queued that nothing cancels: nothing was
    /// issued.
    RestartPending,
    /// The service is stopped, but the shim could not kill the daemon, which may still be running.
    Incomplete,
}

/// What the loop in [`stop_via_notify`] is waiting for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Watching {
    /// The `STOPPED` of the stop it issued, or of one already under way.
    Stop,
    /// A start that has begun, still to succeed or fail. The shim accepts no control before
    /// `RUNNING`, so a stop waits here rather than send one.
    Starting,
}

/// What [`issue_stop`] did.
enum Stopping {
    /// The control was accepted. Carries whether the `STOPPED` arm that preceded it was registered.
    Sent(Waited),
    /// The control met a state that refuses it.
    Moved,
}

/// What [`arm_stopped_then_reread`] found.
enum Rearmed {
    /// The deadline ran out before the arm was registered.
    Expired,
    /// The service is `STOPPED` with this record.
    Stopped(RecordedExit),
    /// The service is still `STOP_PENDING`, and the arm will hear it stop.
    Stopping,
    /// The service is neither: it stopped and started again, which the arm cannot hear.
    Moved,
}

/// Whether a service in `status` refuses `STOP` whatever it was last doing: a state that takes
/// controls (not starting, stopping or stopped) that does not accept `STOP`.
fn refuses_stop(status: &QueriedStatus) -> bool {
    !matches!(
        status.state,
        QueriedState::StartPending | QueriedState::StopPending | QueriedState::Stopped
    ) && !status.accepts_stop
}

/// The error for a control the service refused with nothing to explain it.
fn stop_refused() -> io::Error {
    io::Error::other("the service refused the stop control, and no change of its state explains the refusal")
}

/// Send the stop control to a service last read in state `decided`. `true`: it was accepted.
///
/// A refusal is read against the service as it is now. A service in another state than `decided`
/// has moved, and the control met that state: `false`, for the caller to read it again. One still in
/// `decided`, or one in a state that takes controls and does not accept `STOP`, has refused the
/// control itself, and sending it again would be refused again: an error. That also bounds every
/// path with no counter, since each repeat needs the service to have changed.
fn control<A: ScmActor>(a: &mut A, decided: QueriedState) -> io::Result<bool> {
    if a.control_stop()? == ControlReply::Sent {
        return Ok(true);
    }
    let status = a.query_status()?;
    if status.state == decided || refuses_stop(&status) {
        return Err(stop_refused());
    }
    Ok(false)
}

/// Arm `STOPPED`, then issue the stop to a service last read in state `decided`: the arm comes
/// first, so a `STOPPED` reached before the request is delivered still fires it. Issued even when
/// the arm expired: only the *waiting* is bounded.
fn issue_stop<A: ScmActor>(a: &mut A, decided: QueriedState, deadline: Deadline) -> io::Result<Stopping> {
    let armed = a.arm(WantState::Stopped, deadline)?;
    Ok(if control(a, decided)? {
        Stopping::Sent(armed)
    } else {
        Stopping::Moved
    })
}

/// Arm `STOPPED` for a `STOP_PENDING` service, then read it again: the arm hears nothing that
/// happened before it, and a service that stopped and was restarted in between is on its way up,
/// which this arm's mask does not cover. A `STOPPED` read here drops the registration (the handle is
/// reopened), so the handle can arm again.
fn arm_stopped_then_reread<A: ScmActor>(a: &mut A, deadline: Deadline) -> io::Result<Rearmed> {
    if a.arm(WantState::Stopped, deadline)? == Waited::Expired {
        return Ok(Rearmed::Expired);
    }
    let status = a.query_status()?;
    Ok(match status.state {
        QueriedState::StopPending => Rearmed::Stopping,
        QueriedState::Stopped => {
            a.reopen_service()?;
            Rearmed::Stopped(status.exit)
        }
        _ => Rearmed::Moved,
    })
}

/// Run `once` until it settles. `None` from it means the service moved on after being read, so the
/// stop must read it again: close the handle, which drops the registration `once` left behind, and
/// start from the top, under the same `deadline`. A rerun is a fresh reading of a service that
/// changed, never a retry of the same request; `expired` is what a deadline that has run out by
/// then settles as.
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
/// A control is sent only to a service in a state that accepts it. One found `START_PENDING` is
/// waited on for `RUNNING` (then stopped) or `STOPPED`; one found `STOP_PENDING` is already
/// stopping and is waited on for `STOPPED`; every arm of `STOPPED` is followed by a read of the
/// state, since an arm hears nothing that happened before it. If the service moves between the read
/// and the control, so that the control is refused, the stop starts again from a fresh read (see
/// `rerun_if_moved`); a refusal from a service that has not moved is an error (see [`control`]).
///
/// Every `STOPPED` is then judged by [`settle`]. A restart pending after it, whether it is the
/// failure of a daemon that was already down or of one that failed as the stop arrived, is reported
/// and never waited for: nothing cancels it, and a crash-looping daemon would keep such a wait from
/// ever ending.
pub fn stop_via_notify<A: ScmActor>(a: &mut A, deadline: Deadline) -> io::Result<StopWaited> {
    rerun_if_moved(a, deadline, StopWaited::Expired, |a| stop_once(a, deadline))
}

/// One pass of [`stop_via_notify`]; `None` when the service moved and must be read again.
fn stop_once<A: ScmActor>(a: &mut A, deadline: Deadline) -> io::Result<Option<StopWaited>> {
    let status = a.query_status()?;
    let mut watching = match status.state {
        QueriedState::StartPending => {
            if a.arm(WantState::RestartResolves, deadline)? == Waited::Expired {
                return Ok(Some(StopWaited::RestartPending));
            }
            Watching::Starting
        }
        QueriedState::StopPending => match arm_stopped_then_reread(a, deadline)? {
            Rearmed::Expired => return Ok(Some(StopWaited::Expired)),
            Rearmed::Stopped(exit) => return Ok(Some(settle(a, exit)?)),
            Rearmed::Stopping => Watching::Stop,
            Rearmed::Moved => return Ok(None),
        },
        QueriedState::Stopped => return Ok(Some(settle(a, status.exit)?)),
        state => {
            if restart_pending(a, state, status.exit)? {
                return Ok(Some(StopWaited::RestartPending));
            }
            match issue_stop(a, state, deadline)? {
                Stopping::Moved => return Ok(None),
                Stopping::Sent(Waited::Expired) => return Ok(Some(StopWaited::Expired)),
                Stopping::Sent(Waited::Confirmed) => Watching::Stop,
            }
        }
    };
    loop {
        let Some(observed) = a.wait_callback(deadline)? else {
            return Ok(Some(match watching {
                Watching::Stop => StopWaited::Expired,
                Watching::Starting => StopWaited::RestartPending,
            }));
        };
        match (watching, observed) {
            (_, Observed::Stopped) => {
                let exit = a.stopped_exit();
                return Ok(Some(settle(a, exit)?));
            }
            // Running/Pending are non-terminal for a stop wait: arm again, and read the service
            // again, since the arm hears nothing that happened before it.
            (Watching::Stop, Observed::Running | Observed::Pending) => match arm_stopped_then_reread(a, deadline)? {
                Rearmed::Expired => return Ok(Some(StopWaited::Expired)),
                Rearmed::Stopped(exit) => return Ok(Some(settle(a, exit)?)),
                Rearmed::Stopping => {}
                Rearmed::Moved => return Ok(None),
            },
            (Watching::Starting, Observed::Pending) => {
                if a.arm(WantState::RestartResolves, deadline)? == Waited::Expired {
                    return Ok(Some(StopWaited::RestartPending));
                }
            }
            (Watching::Starting, Observed::Running) => {
                if restart_pending(a, QueriedState::Running, NO_FAILURE)? {
                    return Ok(Some(StopWaited::RestartPending));
                }
                watching = Watching::Stop;
                match issue_stop(a, QueriedState::Running, deadline)? {
                    Stopping::Moved => return Ok(None),
                    Stopping::Sent(Waited::Expired) => return Ok(Some(StopWaited::Expired)),
                    Stopping::Sent(Waited::Confirmed) => {}
                }
            }
        }
    }
}

/// The record of a service that is not stopped: nothing was recorded.
const NO_FAILURE: RecordedExit = RecordedExit { win32: 0, specific: 0 };

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
/// delete had happened. A daemon the stop could not kill ([`StopWaited::Incomplete`]) does not
/// hold up the delete, since SCM counts the service stopped; it is reported after it.
pub fn uninstall_teardown<A: ScmActor>(a: &mut A, deadline: Deadline) -> io::Result<StopWaited> {
    let first = stop_unless_stopped(a, deadline).map_err(|e| {
        io::Error::new(
            e.kind(),
            format!(
                "the service did not confirm stopped ({e}) and was NOT deleted: deleting a running \
                 service only marks it for deletion, which the next install meets as \
                 ERROR_SERVICE_MARKED_FOR_DELETE. Stop it, or wait for it to stop, and uninstall again"
            ),
        )
    })?;
    if first == StopWaited::Expired {
        return Ok(StopWaited::Expired);
    }
    a.delete()
        .map_err(|e| io::Error::new(e.kind(), format!("the service was NOT deleted: {e}")))?;
    let second = stop_unless_stopped(a, deadline).map_err(|e| {
        io::Error::new(
            e.kind(),
            format!(
                "the service is deleted but not confirmed stopped ({e}); it goes when its process ends, \
                 and until then the next install meets ERROR_SERVICE_MARKED_FOR_DELETE"
            ),
        )
    })?;
    Ok(match (first, second) {
        (StopWaited::Incomplete, StopWaited::Confirmed) => StopWaited::Incomplete,
        _ => second,
    })
}

/// A `STOPPED` service is left alone. A `START_PENDING` one refuses controls, so it is waited on
/// until it is `RUNNING` (then stopped) or `STOPPED`; a `STOP_PENDING` one is waited on for
/// `STOPPED`. Any other state is stopped. Either way the wait ends on any `STOPPED`, whatever
/// follows it. A control refused because the service moved starts again from a fresh read; one
/// refused by a service that has not moved is an error. Ends
/// [`StopWaited::Confirmed`], [`StopWaited::Incomplete`] or [`StopWaited::Expired`].
fn stop_unless_stopped<A: ScmActor>(a: &mut A, deadline: Deadline) -> io::Result<StopWaited> {
    rerun_if_moved(a, deadline, StopWaited::Expired, |a| {
        stop_unless_stopped_once(a, deadline)
    })
}

/// One pass of [`stop_unless_stopped`]; `None` when the service moved and must be read again.
fn stop_unless_stopped_once<A: ScmActor>(a: &mut A, deadline: Deadline) -> io::Result<Option<StopWaited>> {
    let status = a.query_status()?;
    let mut watching = match status.state {
        QueriedState::Stopped => return Ok(Some(stopped_result(a, status.exit)?)),
        QueriedState::StartPending => {
            if a.arm(WantState::RestartResolves, deadline)? == Waited::Expired {
                return Ok(Some(StopWaited::Expired));
            }
            Watching::Starting
        }
        QueriedState::StopPending => match arm_stopped_then_reread(a, deadline)? {
            Rearmed::Expired => return Ok(Some(StopWaited::Expired)),
            Rearmed::Stopped(exit) => return Ok(Some(stopped_result(a, exit)?)),
            Rearmed::Stopping => Watching::Stop,
            Rearmed::Moved => return Ok(None),
        },
        state => match issue_stop(a, state, deadline)? {
            Stopping::Moved => return Ok(None),
            Stopping::Sent(Waited::Expired) => return Ok(Some(StopWaited::Expired)),
            Stopping::Sent(Waited::Confirmed) => Watching::Stop,
        },
    };
    loop {
        let rearm = match (watching, a.wait_callback(deadline)?) {
            (_, None) => return Ok(Some(StopWaited::Expired)),
            (_, Some(Observed::Stopped)) => {
                let exit = a.stopped_exit();
                return Ok(Some(stopped_result(a, exit)?));
            }
            (Watching::Starting, Some(Observed::Running)) => {
                watching = Watching::Stop;
                match issue_stop(a, QueriedState::Running, deadline)? {
                    Stopping::Moved => return Ok(None),
                    Stopping::Sent(Waited::Expired) => return Ok(Some(StopWaited::Expired)),
                    Stopping::Sent(Waited::Confirmed) => continue,
                }
            }
            (Watching::Starting, Some(Observed::Pending)) => WantState::RestartResolves,
            (_, Some(Observed::Running | Observed::Pending)) => WantState::Stopped,
        };
        if rearm == WantState::Stopped {
            match arm_stopped_then_reread(a, deadline)? {
                Rearmed::Expired => return Ok(Some(StopWaited::Expired)),
                Rearmed::Stopped(exit) => return Ok(Some(stopped_result(a, exit)?)),
                Rearmed::Stopping => {}
                Rearmed::Moved => return Ok(None),
            }
        } else if a.arm(rearm, deadline)? == Waited::Expired {
            return Ok(Some(StopWaited::Expired));
        }
    }
}

/// Refuse a start while the daemon an earlier incomplete stop could not kill is still running: SCM
/// would accept the start, and a second daemon would run beside the first. The shim's state file
/// names that daemon, so the refusal ends with the daemon and not with the status record the stop
/// left, which outlives it. Read before every start, since a start cannot be told from the reply
/// alone. (A start through `sc start` meets the same check in the shim.)
fn refuse_over_a_survivor<A: ScmActor>(a: &mut A) -> io::Result<()> {
    if let Some((pid, survivor)) = a.survivor()? {
        return Err(io::Error::other(survivor_refusal(pid, survivor)));
    }
    // The shim failed to record the survivor: the status of the incomplete stop is all there is.
    if let Some(shim) = a.shim_state()?
        && !shim.names_a_survivor()
    {
        let status = a.query_status()?;
        if status.state == QueriedState::Stopped && status.exit.incomplete_stop() {
            return Err(io::Error::other(
                "goetia-shim could not kill the daemon at the last stop, which may still be running, and \
                 has not recorded which process it is; a start would run a second one. Stop the daemon \
                 yourself, then start the service with `sc start`",
            ));
        }
    }
    Ok(())
}

/// Why a start is refused over the survivor `pid`, and what to do.
fn survivor_refusal(pid: u32, survivor: Survivor) -> String {
    let state = match survivor {
        Survivor::Alive => format!("process {pid} is still running"),
        Survivor::Unknown | Survivor::Gone => format!("process {pid} could not be told not to be running"),
    };
    format!(
        "goetia-shim could not kill the daemon at the last stop, which may still be running: {state}, and a \
         start would run a second one. Stop that process yourself, then start the service"
    )
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
    refuse_over_a_survivor(a)?;
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
    refuse_over_a_survivor(a)?;
    accept_as_started(a.start()?)
}

/// `daemon restart --timeout 0`'s start: [`request_start`], except `ERROR_SERVICE_ALREADY_RUNNING`
/// is always a refusal: the queried `Running` may be the instance the unconfirmed stop just before
/// it is taking down.
pub fn request_start_after_stop<A: ScmActor>(a: &mut A) -> io::Result<()> {
    refuse_over_a_survivor(a)?;
    match a.start()? {
        StartReply::Accepted => Ok(()),
        StartReply::AlreadyRunning(queried) => Err(io::Error::other(format!(
            "StartServiceW reported ERROR_SERVICE_ALREADY_RUNNING with the service {queried:?}: \
             the stop issued just before it has not taken effect, so the start was refused"
        ))),
    }
}

/// [`request_start`]'s mirror: issue the stop control and return, unless the service is `STOPPED`
/// with a restart pending, or `START_PENDING`. A stop cannot cancel that restart, and a starting
/// service refuses the control, so it reports the restart and issues nothing. A `STOPPED` service
/// with nothing pending, or a `STOP_PENDING` one, is already as stopped as the request can make it
/// and is sent nothing.
///
/// A control refused because the service moved after the read starts again from a fresh read; one
/// refused by a service that has not moved is an error.
pub fn request_stop<A: ScmActor>(a: &mut A) -> io::Result<RequestedStop> {
    loop {
        let status = a.query_status()?;
        match status.state {
            QueriedState::StartPending => return Ok(RequestedStop::RestartPending),
            QueriedState::Stopped => {
                return Ok(match settle(a, status.exit)? {
                    StopWaited::Incomplete => RequestedStop::Incomplete,
                    StopWaited::RestartPending => RequestedStop::RestartPending,
                    StopWaited::Confirmed | StopWaited::Expired => RequestedStop::Issued,
                });
            }
            QueriedState::StopPending => return Ok(RequestedStop::Issued),
            _ => {}
        }
        if restart_pending(a, status.state, status.exit)? {
            return Ok(RequestedStop::RestartPending);
        }
        if control(a, status.state)? {
            return Ok(RequestedStop::Issued);
        }
    }
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

#[cfg(test)]
#[path = "wait_stop_paths_tests.rs"]
mod wait_stop_paths_tests;
