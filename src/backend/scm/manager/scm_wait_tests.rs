//! Pure orchestration tests for [`super::stop_via_notify`]/
//! [`super::start_via_notify`] against a scripted [`super::ScmActor`] — no
//! real SCM involved, so these run on every platform and need no elevation.
//! Ported from `~/src/hole/crates/bridge/src/cutover/scm_wait_tests.rs`.

use std::cell::RefCell;
use std::collections::VecDeque;

use super::*;

/// Records the granular SCM steps in call order and replays a scripted
/// `Observed` for each `wait_callback`. The script asserts the pure ordering
/// of `stop_via_notify`/`start_via_notify` without touching the real SCM.
struct FakeScm<'a> {
    log: &'a RefCell<Vec<&'static str>>,
    /// `Observed`s the next `wait_callback` calls return, front to back.
    waits: VecDeque<Observed>,
}

impl<'a> FakeScm<'a> {
    fn new(log: &'a RefCell<Vec<&'static str>>, waits: VecDeque<Observed>) -> Self {
        Self { log, waits }
    }
}

impl ScmActor for FakeScm<'_> {
    fn arm(&mut self, want: WantState) -> std::io::Result<()> {
        self.log.borrow_mut().push(match want {
            WantState::Stopped => "arm_stopped",
            WantState::Running => "arm_running",
        });
        Ok(())
    }
    fn control_stop(&mut self) -> std::io::Result<()> {
        self.log.borrow_mut().push("control_stop");
        Ok(())
    }
    fn start(&mut self) -> std::io::Result<()> {
        self.log.borrow_mut().push("start");
        Ok(())
    }
    fn wait_callback(&mut self) -> std::io::Result<Observed> {
        self.log.borrow_mut().push("wait");
        let state = self.waits.pop_front().expect("script ran dry");
        self.log.borrow_mut().push(match state {
            Observed::Stopped => "got_stopped",
            Observed::Running => "got_running",
            Observed::Pending => "got_pending",
        });
        Ok(state)
    }
}

/// A minimal scripted actor (no call log) that counts `arm`s — for the
/// terminal-state and re-arm-count assertions.
struct ScriptedScm {
    waits: VecDeque<Observed>,
    arms: usize,
}

impl ScriptedScm {
    fn new(waits: Vec<Observed>) -> Self {
        Self {
            waits: waits.into(),
            arms: 0,
        }
    }

    fn arm_count(&self) -> usize {
        self.arms
    }
}

impl ScmActor for ScriptedScm {
    fn arm(&mut self, _want: WantState) -> std::io::Result<()> {
        self.arms += 1;
        Ok(())
    }
    fn control_stop(&mut self) -> std::io::Result<()> {
        Ok(())
    }
    fn start(&mut self) -> std::io::Result<()> {
        Ok(())
    }
    fn wait_callback(&mut self) -> std::io::Result<Observed> {
        Ok(self.waits.pop_front().expect("script ran dry"))
    }
}

#[skuld::test]
fn stop_via_notify_arms_stopped_then_gates_on_stopped() {
    let log: RefCell<Vec<&'static str>> = RefCell::new(vec![]);
    let mut fake = FakeScm::new(&log, [Observed::Stopped].into());
    stop_via_notify(&mut fake).unwrap();
    assert_eq!(
        *log.borrow(),
        vec!["arm_stopped", "control_stop", "wait", "got_stopped"]
    );
}

#[skuld::test]
fn stop_re_arms_after_a_non_terminal_callback() {
    let log: RefCell<Vec<&'static str>> = RefCell::new(vec![]);
    // A pending (intermediate) callback fires while waiting for STOPPED, then
    // STOPPED; the non-terminal callback must trigger a re-arm.
    let mut fake = FakeScm::new(&log, [Observed::Pending, Observed::Stopped].into());
    stop_via_notify(&mut fake).unwrap();
    assert_eq!(
        *log.borrow(),
        vec![
            "arm_stopped",
            "control_stop",
            "wait",
            "got_pending",
            "arm_stopped", // re-arm: still waiting for STOPPED
            "wait",
            "got_stopped",
        ]
    );
}

#[skuld::test]
fn start_via_notify_arms_running_before_start_then_gates_on_running() {
    let log: RefCell<Vec<&'static str>> = RefCell::new(vec![]);
    let mut fake = FakeScm::new(&log, [Observed::Running].into());
    start_via_notify(&mut fake).unwrap();
    // The critical ordering: arm RUNNING strictly BEFORE start, else a
    // RUNNING reached before the arm fires only on the NEXT entry — a hang.
    assert_eq!(*log.borrow(), vec!["arm_running", "start", "wait", "got_running"]);
}

#[skuld::test]
fn start_re_arms_after_a_non_terminal_callback() {
    let log: RefCell<Vec<&'static str>> = RefCell::new(vec![]);
    // A StartPending/StopPending intermediate (Pending) fires while waiting
    // for RUNNING, then RUNNING; the non-terminal callback must trigger a
    // re-arm.
    let mut fake = FakeScm::new(&log, [Observed::Pending, Observed::Running].into());
    start_via_notify(&mut fake).unwrap();
    assert_eq!(
        *log.borrow(),
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
fn start_via_notify_errs_when_service_stops_instead_of_running() {
    let mut fake = ScriptedScm::new(vec![Observed::Pending, Observed::Stopped]);
    assert!(start_via_notify(&mut fake).is_err());
}

#[skuld::test]
fn start_via_notify_ok_when_service_runs() {
    let mut fake = ScriptedScm::new(vec![Observed::Pending, Observed::Running]);
    assert!(start_via_notify(&mut fake).is_ok());
}

#[skuld::test]
fn start_via_notify_rearms_on_pending_not_errs() {
    // A StartPending/StopPending intermediate (Pending) must RE-ARM, not fail.
    let mut fake = ScriptedScm::new(vec![Observed::Pending, Observed::Pending, Observed::Running]);
    assert!(start_via_notify(&mut fake).is_ok());
    assert_eq!(fake.arm_count(), 3); // initial + 2 re-arms
}

#[skuld::test]
fn stop_via_notify_ok_when_already_stopped_on_first_callback() {
    let mut fake = ScriptedScm::new(vec![Observed::Stopped]);
    assert!(stop_via_notify(&mut fake).is_ok());
    assert_eq!(fake.arm_count(), 1);
}
