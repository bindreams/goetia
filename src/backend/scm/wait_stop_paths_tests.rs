//! One table over every stop path and every interleaving of the service's state with the stop's
//! two steps (read the state, send the control), run against a small model of the SCM.
//!
//! Paths: `stop` (which is also `restart`'s stop: the manager's `restart` calls `stop`), `stop` on
//! a spent budget, `stop --timeout 0`, and `uninstall`'s steps 1 and 3. Each is crossed with the
//! state at the first read, a change of state between that observation and the stop's next step,
//! the result of the first control, how a start in progress ends, and how a stop that reaches a
//! running daemon ends.
//!
//! Every row asserts: no control is sent while the latest observation says `START_PENDING` or
//! `STOPPED`; no second notification is registered on a handle; every wait runs on the caller's
//! deadline; the outcome is the one the spec gives for the path and the state the stop acted on.

use super::*;
use crate::backend::scm::exit_status::{RecordedExit, Reported};
use crate::manager::budget::{Budget, Deadline};
use crate::spec::Kind;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Path {
    Stop,
    StopOnSpentBudget,
    StopTimeoutZero,
    UninstallStep1,
    UninstallStep3,
}

const PATHS: [Path; 5] = [
    Path::Stop,
    Path::StopOnSpentBudget,
    Path::StopTimeoutZero,
    Path::UninstallStep1,
    Path::UninstallStep3,
];

/// The service at the first read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum First {
    StartPending,
    StopPending,
    Running,
    StoppedClean,
    /// SCM's initial record for a service that has not run since it was created or since boot:
    /// `ERROR_SERVICE_NEVER_STARTED`, which is not a failure and queues nothing (measured, P-lines).
    StoppedNeverStarted,
    /// Daemon failed under `restart: on-failure`: SCM has a restart queued.
    StoppedFailed,
    /// Daemon failed with no recovery actions configured: nothing is queued.
    StoppedFailedNoRecovery,
}

const FIRSTS: [First; 7] = [
    First::StartPending,
    First::StopPending,
    First::Running,
    First::StoppedClean,
    First::StoppedNeverStarted,
    First::StoppedFailed,
    First::StoppedFailedNoRecovery,
];

/// What the service becomes between the observation and the stop's next step: the first control
/// for a running service, the arm for one that is starting or stopping.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Change {
    None,
    ToStartPending,
    ToRunning,
    /// The service stopped and was started again: `RUNNING`, in a new process.
    ToRunningAgain,
    ToStoppedClean,
    ToStoppedFailed,
    /// The daemon crashed and SCM restarted it: the failure's `STOPPED` is delivered to a
    /// notification armed before, and the service is `RUNNING` again in a new process.
    ToCrashedAndRestarted,
}

const CHANGES: [Change; 7] = [
    Change::None,
    Change::ToStartPending,
    Change::ToRunning,
    Change::ToRunningAgain,
    Change::ToStoppedClean,
    Change::ToStoppedFailed,
    Change::ToCrashedAndRestarted,
];

/// What the control returns, whatever the state: `Ok` leaves it to the state. Unless it says
/// `Always`, only the first control gets the answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Forced {
    Ok,
    /// `ERROR_SERVICE_CANNOT_ACCEPT_CTRL`. From a service that still reads as it did, in the same
    /// run, it is the shim's: the failure is decided and its `STOPPED` is on its way.
    Busy,
    /// `ERROR_SERVICE_NOT_ACTIVE`
    NotActive,
    /// `ERROR_INVALID_SERVICE_CONTROL`, once, from a service that does accept `STOP`
    Invalid,
    /// `Busy`, on every control.
    AlwaysBusy,
    /// The shim refuses the first control with `Busy` because it has decided a failure, and has
    /// reported the `STOPPED` with that failure by the time the stop reads the service again.
    FailureDecided,
    /// The first control meets a crashed-and-restarted run that reports `RUNNING` before it sets
    /// `ACCEPT_STOP`: `ERROR_INVALID_SERVICE_CONTROL`, in a new process that accepts `STOP` a moment
    /// later.
    RestartedNotAccepting,
    /// `Invalid`, on every control: a handler that answers `STOP` with 1052 though the service
    /// reports `RUNNING` and `ACCEPT_STOP`.
    AlwaysInvalid,
    /// `ERROR_INVALID_SERVICE_CONTROL` for good: a running service that does not accept `STOP`.
    NeverAccepts,
    /// Every control meets a service that has crashed and been restarted since it was read: it
    /// answers `ERROR_SERVICE_NOT_ACTIVE`, and is `RUNNING` again in a new process. A service that
    /// never settles.
    AlwaysMoved,
}

const FORCED: [Forced; 10] = [
    Forced::Ok,
    Forced::Busy,
    Forced::NotActive,
    Forced::Invalid,
    Forced::AlwaysBusy,
    Forced::FailureDecided,
    Forced::RestartedNotAccepting,
    Forced::AlwaysInvalid,
    Forced::NeverAccepts,
    Forced::AlwaysMoved,
];

/// How a start in progress (a restart SCM made, or one a user asked for) ends.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Starts {
    Runs,
    /// The daemon fails: Win32 1066 beside its code.
    DaemonFails,
    /// The shim fails: a non-zero Win32 code of its own.
    ShimFails,
    /// SCM's own attempt fails (P10): `STOPPED` with Win32 0, and nothing follows.
    AttemptFailsClean,
}

const STARTS: [Starts; 4] = [
    Starts::Runs,
    Starts::DaemonFails,
    Starts::ShimFails,
    Starts::AttemptFailsClean,
];

/// How a stop that reaches a running daemon ends.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StopMeets {
    /// The stop kills the daemon: Win32 0.
    Clean,
    /// The daemon fails as the stop arrives: the `STOPPED` is the failure's.
    Failure,
}

const STOP_MEETS: [StopMeets; 2] = [StopMeets::Clean, StopMeets::Failure];

fn daemon_failed() -> RecordedExit {
    let (win32, specific) = Reported::from_daemon_code(7).scm_codes();
    RecordedExit { win32, specific }
}

fn shim_failed() -> RecordedExit {
    RecordedExit { win32: 5, specific: 0 }
}

fn never_started() -> RecordedExit {
    RecordedExit {
        win32: 1077,
        specific: 0,
    }
}

fn clean() -> RecordedExit {
    RecordedExit { win32: 0, specific: 0 }
}

fn in_mask(want: WantState, state: QueriedState) -> bool {
    match want {
        WantState::Stopped => matches!(state, QueriedState::Stopped | QueriedState::StopPending),
        WantState::Running => matches!(state, QueriedState::Running | QueriedState::StartPending),
        WantState::RestartResolves => matches!(state, QueriedState::Running | QueriedState::Stopped),
    }
}

/// What the stop under test last learnt of the service, from a query or a delivered callback.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Known {
    Nothing,
    Is(QueriedState),
    /// A callback for a pending state: it does not say which.
    SomePending,
}

/// The SCM as one service sees it. Time passes only when the stop blocks (`wait_callback`), and
/// then the next thing that was due happens: a queued restart begins, a starting service runs or
/// fails, a stopping one stops.
struct Scm {
    state: QueriedState,
    exit: RecordedExit,
    /// The process of the current run: a new one for every start.
    pid: u32,
    recovery: Recovery,
    restart_queued: bool,
    marked_for_deletion: bool,
    /// Applied at the first control for a running service, and at the first arm for any other
    /// (`late`: at the first arm after a pending callback).
    change: Change,
    change_at_control: bool,
    change_applied: bool,
    late: bool,
    pending_heard: bool,
    forced: Forced,
    forced_used: bool,
    starts: Starts,
    stop_meets: StopMeets,
    /// A control was refused by a service that was `RUNNING` when the stop last looked, and still
    /// is: nothing moved, so the refusal is the answer.
    unexplained_refusal: bool,
    /// The shim refused a control with `Busy` while the service was `RUNNING` in the run the stop last
    /// saw: its failure is decided, and it reports the `STOPPED` the next time anything is due.
    failing: bool,
    /// A control was refused that way at least once.
    shim_refused: bool,
    /// The service reports `ACCEPT_STOP`; a run that has just started does not, until it is read.
    accepting: bool,
    /// `uninstall` step 3: what `delete` finds the service in, a restart having fired meanwhile.
    state_after_delete: Option<(QueriedState, RecordedExit)>,
    armed: Option<WantState>,
    delivered: Option<(Observed, RecordedExit)>,
    delivered_by_arm: bool,
    last_stopped: RecordedExit,
    known: Known,
    /// The process id the last query read; `None` after a callback, which carries none.
    known_pid: Option<u32>,
    deadline: Deadline,
    calls: usize,
    controls: usize,
    deletes: usize,
    controls_before_delete: usize,
    violations: Vec<String>,
}

impl Scm {
    fn new(first: First, row: &Row, deadline: Deadline, step3: bool) -> Self {
        let (state, exit) = match first {
            First::StartPending => (QueriedState::StartPending, clean()),
            First::StopPending => (QueriedState::StopPending, clean()),
            First::Running => (QueriedState::Running, clean()),
            First::StoppedClean => (QueriedState::Stopped, clean()),
            First::StoppedNeverStarted => (QueriedState::Stopped, never_started()),
            First::StoppedFailed | First::StoppedFailedNoRecovery => (QueriedState::Stopped, daemon_failed()),
        };
        let recovery = if first == First::StoppedFailedNoRecovery {
            Recovery::default()
        } else {
            Recovery {
                restarts: true,
                on_non_crash_failures: true,
            }
        };
        let mut scm = Self {
            state,
            exit,
            pid: 1,
            recovery,
            restart_queued: false,
            marked_for_deletion: false,
            change: row.change,
            change_at_control: first == First::Running,
            change_applied: false,
            late: row.late,
            pending_heard: false,
            forced: row.forced,
            forced_used: false,
            starts: row.starts,
            stop_meets: row.stop_meets,
            unexplained_refusal: false,
            failing: false,
            shim_refused: false,
            accepting: true,
            state_after_delete: None,
            armed: None,
            delivered: None,
            delivered_by_arm: false,
            last_stopped: clean(),
            known: Known::Nothing,
            known_pid: None,
            deadline,
            calls: 0,
            controls: 0,
            deletes: 0,
            controls_before_delete: 0,
            violations: vec![],
        };
        if step3 {
            // Step 1 finds a stopped service and sends nothing; the restart fires before the delete.
            scm.state_after_delete = Some((state, exit));
            scm.state = QueriedState::Stopped;
            scm.exit = clean();
        } else {
            scm.restart_queued = scm.queues_restart(state, exit);
        }
        scm
    }

    fn queues_restart(&self, state: QueriedState, exit: RecordedExit) -> bool {
        state == QueriedState::Stopped
            && exit.win32 != 0
            && exit.win32 != 1077
            && self.recovery.restarts
            && self.recovery.on_non_crash_failures
            && !self.marked_for_deletion
    }

    fn observe(state: QueriedState) -> Observed {
        match state {
            QueriedState::Running => Observed::Running,
            QueriedState::Stopped => Observed::Stopped,
            _ => Observed::Pending,
        }
    }

    fn set(&mut self, state: QueriedState, exit: RecordedExit) {
        self.state = state;
        self.exit = exit;
        if state == QueriedState::StartPending {
            self.pid += 1;
        }
        self.restart_queued = self.queues_restart(state, exit);
        if let Some(want) = self.armed
            && in_mask(want, state)
        {
            self.armed = None;
            self.delivered = Some((Self::observe(state), exit));
        }
    }

    /// The next thing that was due happens. `false` when nothing is due: the service stays as it is.
    fn time_passes(&mut self) -> bool {
        if self.failing && self.state == QueriedState::Running {
            self.failing = false;
            self.set(QueriedState::Stopped, daemon_failed());
        } else if self.restart_queued {
            self.set(QueriedState::StartPending, clean());
        } else if self.state == QueriedState::StartPending {
            match self.starts {
                Starts::Runs => self.set(QueriedState::Running, clean()),
                Starts::DaemonFails => self.set(QueriedState::Stopped, daemon_failed()),
                Starts::ShimFails => self.set(QueriedState::Stopped, shim_failed()),
                Starts::AttemptFailsClean => self.set(QueriedState::Stopped, clean()),
            }
        } else if self.state == QueriedState::StopPending {
            self.set(QueriedState::Stopped, clean());
        } else {
            return false;
        }
        true
    }

    fn count_call(&mut self) -> io::Result<()> {
        self.calls += 1;
        if self.calls > 10_000 {
            return Err(io::Error::other("model: the stop is spinning"));
        }
        Ok(())
    }

    fn check_deadline(&mut self, got: Deadline, what: &str) {
        if got != self.deadline {
            self.violations.push(format!("{what} ran on a different deadline"));
        }
    }

    fn apply_change(&mut self) {
        self.change_applied = true;
        match self.change {
            Change::None => {}
            Change::ToStartPending => self.set(QueriedState::StartPending, clean()),
            Change::ToRunning => self.set(QueriedState::Running, clean()),
            Change::ToRunningAgain => {
                self.pid += 1;
                self.set(QueriedState::Running, clean());
            }
            Change::ToStoppedClean => self.set(QueriedState::Stopped, clean()),
            Change::ToStoppedFailed => self.set(QueriedState::Stopped, daemon_failed()),
            Change::ToCrashedAndRestarted => {
                self.set(QueriedState::Stopped, daemon_failed());
                self.set(QueriedState::StartPending, clean());
                self.set(QueriedState::Running, clean());
            }
        }
    }
}

/// What a control did, before the trait decides how to carry it.
#[derive(Clone, Copy)]
enum Reply {
    Sent,
    Busy,
    NotActive,
    Invalid,
}

impl Scm {
    fn control(&mut self) -> io::Result<Reply> {
        self.count_call()?;
        if matches!(
            self.known,
            Known::Is(QueriedState::StartPending | QueriedState::Stopped)
        ) {
            self.violations
                .push(format!("a control was sent after observing {:?}", self.known));
        }
        self.controls += 1;
        if self.deletes == 0 {
            self.controls_before_delete += 1;
        }
        if self.change_at_control && !self.change_applied {
            self.apply_change();
        }
        let forced = match self.forced {
            Forced::Ok
            | Forced::NeverAccepts
            | Forced::AlwaysMoved
            | Forced::FailureDecided
            | Forced::RestartedNotAccepting => None,
            Forced::Busy | Forced::AlwaysBusy => Some(Reply::Busy),
            Forced::NotActive => Some(Reply::NotActive),
            Forced::Invalid | Forced::AlwaysInvalid => Some(Reply::Invalid),
        };
        let every_time = matches!(self.forced, Forced::AlwaysBusy | Forced::AlwaysInvalid);
        if let Some(reply) = forced
            && (every_time || !self.forced_used)
        {
            self.forced_used = true;
            return Ok(self.refuse(reply));
        }
        Ok(match self.state {
            // Measured: a starting service answers `ERROR_INVALID_SERVICE_CONTROL`.
            QueriedState::StartPending => Reply::Invalid,
            QueriedState::Stopped => Reply::NotActive,
            QueriedState::Running if self.forced == Forced::NeverAccepts => self.refuse(Reply::Invalid),
            QueriedState::Running if self.forced == Forced::FailureDecided && !self.forced_used => {
                self.forced_used = true;
                self.set(QueriedState::Stopped, daemon_failed());
                Reply::Busy
            }
            QueriedState::Running if self.forced == Forced::RestartedNotAccepting && !self.forced_used => {
                self.forced_used = true;
                self.accepting = false;
                self.pid += 1;
                self.set(QueriedState::Running, clean());
                self.refuse(Reply::Invalid)
            }
            QueriedState::Running if self.forced == Forced::AlwaysMoved => {
                self.pid += 1;
                self.set(QueriedState::Running, clean());
                self.refuse(Reply::NotActive)
            }
            QueriedState::Running => {
                let exit = match self.stop_meets {
                    StopMeets::Clean => clean(),
                    StopMeets::Failure => daemon_failed(),
                };
                self.set(QueriedState::Stopped, exit);
                Reply::Sent
            }
            _ => Reply::Sent,
        })
    }

    /// A refusal, noting whether it is final: the service is the run the stop last saw running, or
    /// refuses `STOP` whatever run it is.
    fn refuse(&mut self, reply: Reply) -> Reply {
        let same_run = self.known_pid.is_none_or(|pid| pid == self.pid);
        let final_refusal = same_run || self.forced == Forced::NeverAccepts;
        if self.state == QueriedState::Running && self.known == Known::Is(QueriedState::Running) && final_refusal {
            if matches!(reply, Reply::Busy) {
                self.failing = true;
                self.shim_refused = true;
            } else {
                self.unexplained_refusal = true;
            }
        }
        reply
    }
}

impl ScmActor for Scm {
    fn arm(&mut self, want: WantState, deadline: Deadline) -> io::Result<Waited> {
        self.count_call()?;
        self.check_deadline(deadline, "arm");
        let change_due = if self.late {
            self.pending_heard
        } else {
            !self.change_at_control
        };
        if change_due && !self.change_applied {
            self.apply_change();
        }
        if self.armed.is_some() || self.delivered.is_some() {
            self.violations
                .push("a second notification was registered on one handle".to_string());
        }
        if deadline.expired() {
            return Ok(Waited::Expired);
        }
        // A registration made after `DeleteService` succeeds and delivers (measured, P13a/b), so
        // a service marked for deletion is armed like any other.
        self.armed = Some(want);
        if in_mask(want, self.state) {
            self.armed = None;
            self.delivered = Some((Self::observe(self.state), self.exit));
            self.delivered_by_arm = true;
        }
        Ok(Waited::Confirmed)
    }

    fn control_stop(&mut self) -> io::Result<ControlReply> {
        Ok(match self.control()? {
            Reply::Sent => ControlReply::Sent,
            Reply::Busy => ControlReply::CannotAccept,
            Reply::NotActive => ControlReply::StateMoved,
            Reply::Invalid => ControlReply::NotAccepted,
        })
    }

    fn reopen_service(&mut self) -> io::Result<()> {
        self.count_call()?;
        self.armed = None;
        self.delivered = None;
        self.delivered_by_arm = false;
        Ok(())
    }

    fn delete(&mut self) -> io::Result<()> {
        self.count_call()?;
        self.deletes += 1;
        self.marked_for_deletion = true;
        self.restart_queued = false;
        if let Some((state, exit)) = self.state_after_delete.take() {
            self.set(state, exit);
        }
        Ok(())
    }

    fn start(&mut self) -> io::Result<StartReply> {
        unreachable!("no stop path starts the service")
    }

    fn wait_callback(&mut self, deadline: Deadline) -> io::Result<Option<Observed>> {
        self.count_call()?;
        self.check_deadline(deadline, "wait");
        if deadline.expired() {
            return Ok(None);
        }
        loop {
            if let Some((observed, exit)) = self.delivered.take() {
                self.known_pid = None;
                self.known = match observed {
                    Observed::Running => Known::Is(QueriedState::Running),
                    Observed::Stopped => Known::Is(QueriedState::Stopped),
                    Observed::Pending => Known::SomePending,
                };
                if observed == Observed::Stopped {
                    self.last_stopped = exit;
                }
                self.pending_heard |= observed == Observed::Pending;
                if std::mem::take(&mut self.delivered_by_arm) {
                    // The wait did not block, but the time the stop took to get here passed.
                    self.time_passes();
                }
                return Ok(Some(observed));
            }
            if self.armed.is_none() {
                self.violations.push("a wait with nothing armed".to_string());
                return Err(io::Error::other("model: would block forever"));
            }
            if !self.time_passes() {
                return Err(io::Error::other("model: would block forever"));
            }
        }
    }

    fn stopped_exit(&self) -> RecordedExit {
        self.last_stopped
    }

    /// Only `start_via_notify` reads the hint, and no stop path starts the service.
    fn stopped_wait_hint(&self) -> u32 {
        0
    }

    fn query_status(&mut self) -> io::Result<QueriedStatus> {
        self.count_call()?;
        self.known = Known::Is(self.state);
        let pid = if self.state == QueriedState::Stopped {
            0
        } else {
            self.pid
        };
        self.known_pid = Some(pid);
        let accepts_stop = self.accepting && self.forced != Forced::NeverAccepts;
        self.accepting = true;
        Ok(QueriedStatus {
            state: self.state,
            exit: self.exit,
            accepts_stop,
            pid,
        })
    }

    fn recovery(&mut self) -> io::Result<Recovery> {
        self.count_call()?;
        Ok(self.recovery)
    }
}

/// What a path returned, in one vocabulary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Outcome {
    Stopped,
    Expired,
    RestartPending,
    /// `--timeout 0` issued the stop (or found nothing to issue).
    Issued,
    /// `--timeout 0` met a service that kept changing state, and issued nothing.
    KeptMoving,
}

fn run(path: Path, scm: &mut Scm, deadline: Deadline) -> io::Result<Outcome> {
    Ok(match path {
        Path::Stop | Path::StopOnSpentBudget => match stop_via_notify(scm, deadline, Kind::Simple)? {
            StopWaited::Confirmed => Outcome::Stopped,
            StopWaited::Expired => Outcome::Expired,
            StopWaited::RestartPending => Outcome::RestartPending,
            StopWaited::Incomplete => unreachable!("the model never reports an incomplete stop"),
        },
        Path::StopTimeoutZero => match request_stop(scm, Kind::Simple)? {
            RequestedStop::Issued => Outcome::Issued,
            RequestedStop::RestartPending => Outcome::RestartPending,
            RequestedStop::KeptMoving => Outcome::KeptMoving,
            RequestedStop::Incomplete => unreachable!("the model never reports an incomplete stop"),
        },
        Path::UninstallStep1 | Path::UninstallStep3 => match uninstall_teardown(scm, deadline, Kind::Simple)? {
            StopWaited::Confirmed => Outcome::Stopped,
            StopWaited::Expired => Outcome::Expired,
            other => unreachable!("an uninstall cannot end {other:?} in the model"),
        },
    })
}

/// The dimensions of a row that are not the path or the first state.
#[derive(Debug, Clone, Copy)]
struct Row {
    change: Change,
    /// The change happens after the first pending callback, between it and the next arm, instead
    /// of at the stop's next step.
    late: bool,
    forced: Forced,
    starts: Starts,
    stop_meets: StopMeets,
}

/// The state a stop acts on: the first state, or what it became when the stop's next step found it
/// changed. A service that is already stopped is acted on as read, since no step follows.
fn acted_on(first: First, change: Change) -> First {
    if matches!(
        first,
        First::StoppedClean | First::StoppedNeverStarted | First::StoppedFailed | First::StoppedFailedNoRecovery
    ) {
        return first;
    }
    match change {
        Change::None => first,
        Change::ToStartPending => First::StartPending,
        Change::ToRunning | Change::ToRunningAgain => First::Running,
        Change::ToStoppedClean => First::StoppedClean,
        Change::ToStoppedFailed => First::StoppedFailed,
        Change::ToCrashedAndRestarted => First::Running,
    }
}

/// What a `STOPPED` with the daemon's failure recorded settles as: a restart is pending when recovery
/// actions are configured.
fn settled_failed(first: First) -> Outcome {
    if first == First::StoppedFailedNoRecovery {
        Outcome::Stopped
    } else {
        Outcome::RestartPending
    }
}

/// How a stop of a service that is starting ends: by how the start ends.
fn start_outcome(row: &Row, running: Outcome, first: First) -> Outcome {
    match row.starts {
        Starts::Runs => running,
        Starts::DaemonFails | Starts::ShimFails => settled_failed(first),
        Starts::AttemptFailsClean => Outcome::Stopped,
    }
}

/// The outcome the spec gives for `path` when the service is first read as `first` and the row's
/// dimensions play out.
fn expected(path: Path, first: First, row: &Row) -> Outcome {
    let acts_on = acted_on(first, row.change);
    // The shim refuses the stop that reaches a running daemon, and what the service reads as then is
    // the failure it decided.
    let refused = row.forced == Forced::FailureDecided;
    // How a stop that reaches a running daemon ends.
    let running = match row.stop_meets {
        _ if refused => settled_failed(first),
        StopMeets::Clean => Outcome::Stopped,
        StopMeets::Failure => settled_failed(first),
    };
    match path {
        // Waits out a start or a stop in progress, stops a running daemon, and refuses a restart
        // that is pending: it never waits on one.
        Path::Stop => match acts_on {
            // The failure is heard as the stop arms for the start, and the restart it queued fires
            // before the stop reads the service again: the stop is of the new run, not judged by the
            // failure that is no longer pending.
            First::StoppedFailed if first == First::StartPending => start_outcome(row, running, first),
            First::StoppedClean | First::StoppedNeverStarted | First::StopPending => Outcome::Stopped,
            First::StoppedFailed | First::StoppedFailedNoRecovery => settled_failed(first),
            First::Running => running,
            First::StartPending => start_outcome(row, running, first),
        },
        // A spent budget waits for nothing: a start in progress is reported, a stop in progress or a
        // control just sent is left unconfirmed, and a stopped service is judged as it is. A control
        // refused because the service moved is not an expiry: the service is read again, and judged
        // as it is then.
        Path::StopOnSpentBudget => match first {
            First::StartPending => Outcome::RestartPending,
            First::StopPending => Outcome::Expired,
            First::Running if row.forced == Forced::AlwaysMoved => Outcome::Expired,
            First::Running => match acts_on {
                First::StartPending => Outcome::RestartPending,
                First::StoppedClean => Outcome::Stopped,
                First::StoppedFailed => settled_failed(first),
                First::Running if refused => settled_failed(first),
                _ => Outcome::Expired,
            },
            First::StoppedClean | First::StoppedNeverStarted => Outcome::Stopped,
            First::StoppedFailed | First::StoppedFailedNoRecovery => settled_failed(first),
        },
        // Waits for nothing and sends a control only to a running service: a service that is
        // starting, or has a restart pending, is reported.
        Path::StopTimeoutZero => {
            if row.forced == Forced::AlwaysMoved {
                return Outcome::KeptMoving;
            }
            let seen = if first == First::Running { acts_on } else { first };
            match seen {
                First::StartPending | First::StoppedFailed => Outcome::RestartPending,
                First::Running if refused => Outcome::RestartPending,
                _ => Outcome::Issued,
            }
        }
        // Stops what runs, waits for no restart, and deletes.
        Path::UninstallStep1 | Path::UninstallStep3 => Outcome::Stopped,
    }
}

/// What a path gives when the shim refused its control because it had decided a failure that was not
/// yet reported: the stop waits for the failure's `STOPPED` and judges it (a restart is configured in
/// every state that gets a control), or, with nothing to wait on, says so.
fn after_a_shim_refusal(path: Path) -> Outcome {
    match path {
        Path::Stop | Path::StopTimeoutZero => Outcome::RestartPending,
        Path::StopOnSpentBudget => Outcome::Expired,
        Path::UninstallStep1 | Path::UninstallStep3 => Outcome::Stopped,
    }
}

/// Whether a row can happen: the late change needs a pending callback to follow, which only a
/// stopping service the stop waits on gives.
fn possible(path: Path, first: First, change: Change, late: bool, forced: Forced) -> bool {
    // A service that never settles ends only on a deadline that has run out, or on a request that
    // looks twice, and is met by a control only when it starts out running.
    if forced == Forced::AlwaysMoved
        && (!matches!(path, Path::StopOnSpentBudget | Path::StopTimeoutZero)
            || first != First::Running
            || change != Change::None
            || late)
    {
        return false;
    }
    !late
        || (first == First::StopPending
            && change != Change::None
            && matches!(path, Path::Stop | Path::UninstallStep1 | Path::UninstallStep3))
}

#[skuld::test]
fn every_stop_path_survives_every_interleaving_of_the_service_state() {
    let mut rows = 0;
    let mut reached_a_control = 0;
    let mut refused = 0;
    let mut failures = vec![];
    for path in PATHS {
        for first in FIRSTS {
            for change in CHANGES {
                for late in [false, true] {
                    for forced in FORCED {
                        if !possible(path, first, change, late, forced) {
                            continue;
                        }
                        for starts in STARTS {
                            for stop_meets in STOP_MEETS {
                                rows += 1;
                                let row = Row {
                                    change,
                                    late,
                                    forced,
                                    starts,
                                    stop_meets,
                                };
                                let deadline = if path == Path::StopOnSpentBudget {
                                    Budget::Immediate.start()
                                } else {
                                    Budget::Unbounded.start()
                                };
                                let mut scm = Scm::new(first, &row, deadline, path == Path::UninstallStep3);
                                let label = format!("{path:?} / first read {first:?} / {row:?}");
                                let outcome = run(path, &mut scm, deadline);
                                if scm.controls > 0 {
                                    reached_a_control += 1;
                                }
                                let mut problems = std::mem::take(&mut scm.violations);
                                // A service that refuses `STOP` while it is in the state the stop
                                // saw it running in is refusing: rereading it would refuse again,
                                // for ever.
                                if scm.unexplained_refusal {
                                    refused += 1;
                                    match &outcome {
                                        Err(e) if e.to_string().contains("refused the stop control") => {}
                                        other => problems.push(format!("a refusal gave {other:?}")),
                                    }
                                    // The error says whether the delete happened, and it must be true.
                                    if let Err(e) = &outcome
                                        && matches!(path, Path::UninstallStep1 | Path::UninstallStep3)
                                    {
                                        let text = e.to_string();
                                        if text.contains("NOT deleted") != (scm.deletes == 0) {
                                            problems
                                                .push(format!("{} deletes, but the error says: {text}", scm.deletes));
                                        }
                                    }
                                } else {
                                    let want = if scm.shim_refused {
                                        after_a_shim_refusal(path)
                                    } else {
                                        expected(path, first, &row)
                                    };
                                    match outcome {
                                        Ok(got) if got == want => {}
                                        Ok(got) => problems.push(format!("outcome {got:?}, wanted {want:?}")),
                                        Err(e) => problems.push(format!("errored: {e}")),
                                    }
                                    let stopped_for_good = match path {
                                        Path::Stop => want == Outcome::Stopped,
                                        Path::UninstallStep1 | Path::UninstallStep3 => true,
                                        Path::StopOnSpentBudget | Path::StopTimeoutZero => false,
                                    };
                                    if stopped_for_good && (scm.state != QueriedState::Stopped || scm.restart_queued) {
                                        problems.push(format!(
                                            "ended {:?} with a restart queued: {}",
                                            scm.state, scm.restart_queued
                                        ));
                                    }
                                    if matches!(path, Path::UninstallStep1 | Path::UninstallStep3) {
                                        if scm.deletes != 1 {
                                            problems.push(format!("deleted {} times", scm.deletes));
                                        }
                                        if path == Path::UninstallStep3 && scm.controls_before_delete > 0 {
                                            problems.push("step 1 sent a control to a stopped service".to_string());
                                        }
                                    }
                                }
                                // Past the deadline, or on a request that waits for nothing, the stop reads
                                // the service once more and no further, however much it keeps moving:
                                // the first control, and one after it.
                                if forced == Forced::AlwaysMoved && scm.controls != 2 {
                                    problems.push(format!("{} controls past a spent deadline, wanted 2", scm.controls));
                                }
                                if !problems.is_empty() {
                                    failures.push(format!("{label}: {}", problems.join("; ")));
                                }
                            }
                        }
                    }
                }
            }
        }
    }
    eprintln!(
        "{rows} rows ({} paths x {} first states x {} changes, a late one too where it can happen x {} control results x {} start outcomes x {} stop outcomes); {reached_a_control} send a control, {refused} are refused with nothing moved",
        PATHS.len(),
        FIRSTS.len(),
        CHANGES.len(),
        FORCED.len(),
        STARTS.len(),
        STOP_MEETS.len(),
    );
    assert!(
        reached_a_control > 0 && refused > 0,
        "no row sent a control, or none was refused: a dimension that reaches the control step is not exercised"
    );
    assert!(
        failures.is_empty(),
        "{} rows failed:\n{}",
        failures.len(),
        failures.join("\n")
    );
}
