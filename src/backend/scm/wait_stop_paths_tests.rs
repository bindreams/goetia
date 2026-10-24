//! One table over every stop path and every interleaving of the service's state with the stop's
//! two steps (read the state, send the control), run against a small model of the SCM.
//!
//! Paths: `stop` (which is also `restart`'s stop: the manager's `restart` calls `stop`), `stop` on
//! a spent budget, `stop --timeout 0`, and `uninstall`'s steps 1 and 3. Each is crossed with the
//! state at the first read, a change of state between that observation and the first control, and
//! the result of that control.
//!
//! Every row asserts: no control is sent while the latest observation says `START_PENDING` or
//! `STOPPED`; no second notification is registered on a handle; every wait runs on the caller's
//! deadline; the outcome is the one the spec gives for the path and the state the stop acted on.

use super::*;
use crate::backend::scm::exit_status::{RecordedExit, Reported};
use crate::manager::budget::{Budget, Deadline};

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
    /// Daemon failed under `restart: on-failure`: SCM has a restart queued.
    StoppedFailed,
    /// Daemon failed with no recovery actions configured: nothing is queued.
    StoppedFailedNoRecovery,
}

const FIRSTS: [First; 6] = [
    First::StartPending,
    First::StopPending,
    First::Running,
    First::StoppedClean,
    First::StoppedFailed,
    First::StoppedFailedNoRecovery,
];

/// What the service becomes between the observation and the first control.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Change {
    None,
    ToStartPending,
    ToRunning,
    ToStoppedClean,
    ToStoppedFailed,
}

const CHANGES: [Change; 5] = [
    Change::None,
    Change::ToStartPending,
    Change::ToRunning,
    Change::ToStoppedClean,
    Change::ToStoppedFailed,
];

/// What the first control returns, whatever the state: `Ok` leaves it to the state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Forced {
    Ok,
    /// `ERROR_SERVICE_CANNOT_ACCEPT_CTRL`
    Busy,
    /// `ERROR_SERVICE_NOT_ACTIVE`
    NotActive,
    /// `ERROR_INVALID_SERVICE_CONTROL`
    Invalid,
}

const FORCED: [Forced; 4] = [Forced::Ok, Forced::Busy, Forced::NotActive, Forced::Invalid];

fn daemon_failed() -> RecordedExit {
    let (win32, specific) = Reported::from_daemon_code(7).scm_codes();
    RecordedExit { win32, specific }
}

fn clean() -> RecordedExit {
    RecordedExit { win32: 0, specific: 0 }
}

fn in_mask(want: WantState, state: QueriedState) -> bool {
    match want {
        WantState::Stopped => matches!(state, QueriedState::Stopped | QueriedState::StopPending),
        WantState::Running => matches!(state, QueriedState::Running | QueriedState::StartPending),
        WantState::RestartBegins => matches!(state, QueriedState::StartPending | QueriedState::Running),
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
/// then the next thing that was due happens: a queued restart begins, a starting service runs, a
/// stopping one stops.
struct Scm {
    state: QueriedState,
    exit: RecordedExit,
    recovery: Recovery,
    restart_queued: bool,
    marked_for_deletion: bool,
    /// Applied when the first control arrives.
    change: Change,
    change_applied: bool,
    forced: Forced,
    forced_used: bool,
    /// The state the first control found.
    state_at_first_control: Option<QueriedState>,
    /// `uninstall` step 3: what `delete` finds the service in, a restart having fired meanwhile.
    state_after_delete: Option<(QueriedState, RecordedExit)>,
    armed: Option<WantState>,
    delivered: Option<(Observed, RecordedExit)>,
    delivered_by_arm: bool,
    last_stopped: RecordedExit,
    known: Known,
    deadline: Deadline,
    calls: usize,
    controls: usize,
    deletes: usize,
    controls_before_delete: usize,
    violations: Vec<String>,
}

impl Scm {
    fn new(first: First, change: Change, forced: Forced, deadline: Deadline, step3: bool) -> Self {
        let (state, exit) = match first {
            First::StartPending => (QueriedState::StartPending, clean()),
            First::StopPending => (QueriedState::StopPending, clean()),
            First::Running => (QueriedState::Running, clean()),
            First::StoppedClean => (QueriedState::Stopped, clean()),
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
            recovery,
            restart_queued: false,
            marked_for_deletion: false,
            change,
            change_applied: false,
            forced,
            forced_used: false,
            state_at_first_control: None,
            state_after_delete: None,
            armed: None,
            delivered: None,
            delivered_by_arm: false,
            last_stopped: clean(),
            known: Known::Nothing,
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
        if self.restart_queued {
            self.set(QueriedState::StartPending, clean());
        } else if self.state == QueriedState::StartPending {
            self.set(QueriedState::Running, clean());
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
            Change::ToStoppedClean => self.set(QueriedState::Stopped, clean()),
            Change::ToStoppedFailed => self.set(QueriedState::Stopped, daemon_failed()),
        }
    }
}

/// What a control did, before the trait decides how to carry it.
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
        if !self.change_applied {
            self.apply_change();
        }
        self.state_at_first_control.get_or_insert(self.state);
        if !self.forced_used {
            self.forced_used = true;
            match self.forced {
                Forced::Ok => {}
                Forced::Busy => return Ok(Reply::Busy),
                Forced::NotActive => return Ok(Reply::NotActive),
                Forced::Invalid => return Ok(Reply::Invalid),
            }
        }
        Ok(match self.state {
            // Measured: a starting service answers `ERROR_INVALID_SERVICE_CONTROL`.
            QueriedState::StartPending => Reply::Invalid,
            QueriedState::Stopped => Reply::NotActive,
            QueriedState::Running => {
                self.set(QueriedState::Stopped, clean());
                Reply::Sent
            }
            _ => Reply::Sent,
        })
    }
}

impl ScmActor for Scm {
    fn arm(&mut self, want: WantState, deadline: Deadline) -> io::Result<Waited> {
        self.count_call()?;
        self.check_deadline(deadline, "arm");
        if self.armed.is_some() || self.delivered.is_some() {
            self.violations
                .push("a second notification was registered on one handle".to_string());
        }
        if deadline.expired() {
            return Ok(Waited::Expired);
        }
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
            Reply::Busy | Reply::NotActive => ControlReply::StateMoved,
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
                self.known = match observed {
                    Observed::Running => Known::Is(QueriedState::Running),
                    Observed::Stopped => Known::Is(QueriedState::Stopped),
                    Observed::Pending => Known::SomePending,
                };
                if observed == Observed::Stopped {
                    self.last_stopped = exit;
                }
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

    fn query_status(&mut self) -> io::Result<(QueriedState, RecordedExit)> {
        self.count_call()?;
        self.known = Known::Is(self.state);
        Ok((self.state, self.exit))
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
}

fn run(path: Path, scm: &mut Scm, deadline: Deadline) -> io::Result<Outcome> {
    Ok(match path {
        Path::Stop | Path::StopOnSpentBudget => match stop_via_notify(scm, deadline)? {
            StopWaited::Confirmed => Outcome::Stopped,
            StopWaited::Expired => Outcome::Expired,
            StopWaited::RestartPending => Outcome::RestartPending,
        },
        Path::StopTimeoutZero => match request_stop(scm)? {
            RequestedStop::Issued => Outcome::Issued,
            RequestedStop::RestartPending => Outcome::RestartPending,
        },
        Path::UninstallStep1 | Path::UninstallStep3 => match uninstall_teardown(scm, deadline)? {
            Waited::Confirmed => Outcome::Stopped,
            Waited::Expired => Outcome::Expired,
        },
    })
}

/// The state a stop acts on after the change: the first state, or what it became.
fn acted_on(first: First, change: Change) -> First {
    match change {
        Change::None => first,
        Change::ToStartPending => First::StartPending,
        Change::ToRunning => First::Running,
        Change::ToStoppedClean => First::StoppedClean,
        Change::ToStoppedFailed => First::StoppedFailed,
    }
}

/// The outcome the spec gives for `path` when the service is first read as `first`, and a change
/// and a refused control then move it to `acted_on`.
fn expected(path: Path, first: First, change: Change) -> Outcome {
    match path {
        // Waits out a queued restart, stops what it starts, and ends stopped whatever happens meanwhile.
        Path::Stop | Path::UninstallStep1 | Path::UninstallStep3 => Outcome::Stopped,
        // A spent budget waits for nothing: a restart still to come is reported, a stop in progress
        // or a control just sent is left unconfirmed, and a service with nothing queued is stopped.
        Path::StopOnSpentBudget => match first {
            First::StartPending | First::StoppedFailed => Outcome::RestartPending,
            First::StopPending | First::Running => Outcome::Expired,
            First::StoppedClean | First::StoppedFailedNoRecovery => Outcome::Stopped,
        },
        // Waits for nothing and sends a control only to a running service: a service that is
        // starting, or has a restart queued, is reported.
        Path::StopTimeoutZero => {
            let seen = if first == First::Running {
                acted_on(first, change)
            } else {
                first
            };
            match seen {
                First::StartPending | First::StoppedFailed => Outcome::RestartPending,
                _ => Outcome::Issued,
            }
        }
    }
}

#[skuld::test]
fn every_stop_path_survives_every_interleaving_of_the_service_state() {
    let mut rows = 0;
    let mut reached_a_control = 0;
    let mut failures = vec![];
    for path in PATHS {
        for first in FIRSTS {
            for change in CHANGES {
                for forced in FORCED {
                    rows += 1;
                    let deadline = if path == Path::StopOnSpentBudget {
                        Budget::Immediate.start()
                    } else {
                        Budget::Unbounded.start()
                    };
                    let mut scm = Scm::new(first, change, forced, deadline, path == Path::UninstallStep3);
                    let row = format!("{path:?} / first read {first:?} / then {change:?} / control {forced:?}");
                    let outcome = run(path, &mut scm, deadline);
                    if scm.controls > 0 {
                        reached_a_control += 1;
                    }
                    let mut problems = std::mem::take(&mut scm.violations);
                    // A running service that refuses `STOP` for good is an error, not a state to
                    // read again: rereading it would refuse again, for ever.
                    let refuses_for_good =
                        forced == Forced::Invalid && scm.state_at_first_control == Some(QueriedState::Running);
                    if refuses_for_good {
                        match outcome {
                            Err(e) if e.to_string().contains("does not accept the stop control") => {}
                            other => problems.push(format!("a refusal for good gave {other:?}")),
                        }
                        if path == Path::UninstallStep1 && scm.deletes != 0 {
                            problems.push("deleted a service it could not stop".to_string());
                        }
                        if !problems.is_empty() {
                            failures.push(format!("{row}: {}", problems.join("; ")));
                        }
                        continue;
                    }
                    let want = expected(path, first, change);
                    match outcome {
                        Ok(got) if got == want => {}
                        Ok(got) => problems.push(format!("outcome {got:?}, wanted {want:?}")),
                        Err(e) => problems.push(format!("errored: {e}")),
                    }
                    if matches!(path, Path::Stop | Path::UninstallStep1 | Path::UninstallStep3)
                        && (scm.state != QueriedState::Stopped || scm.restart_queued)
                    {
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
                    if !problems.is_empty() {
                        failures.push(format!("{row}: {}", problems.join("; ")));
                    }
                }
            }
        }
    }
    eprintln!(
        "{rows} rows (5 paths x 6 first states x 5 changes x 4 control results); {reached_a_control} send a control"
    );
    assert!(
        failures.is_empty(),
        "{} rows failed:\n{}",
        failures.len(),
        failures.join("\n")
    );
}
