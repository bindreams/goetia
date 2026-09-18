use std::cell::Cell;
use std::collections::BTreeMap;
use std::time::Duration;

use super::{Args, Leg, WaitArgs, leg, run_with};
use crate::error::Result;
use crate::manager::fake::Fake;
use crate::manager::{Budget, ServiceManager};
use crate::spec::{DaemonSpec, Id, Kind, Restart, User};

const THIRTY: Budget = Budget::Bounded(Duration::from_secs(30));

#[skuld::test]
fn a_bounded_budget_that_ran_out_is_spent() {
    assert_eq!(leg(THIRTY, Budget::Immediate), Leg::Spent);
    assert_eq!(leg(THIRTY, Budget::Bounded(Duration::ZERO)), Leg::Spent);
}

#[skuld::test]
fn what_is_left_of_a_bounded_budget_is_what_the_leg_gets() {
    let left = Budget::Bounded(Duration::from_secs(5));
    assert_eq!(leg(THIRTY, left), Leg::Under(left));
}

#[skuld::test]
fn a_budget_that_never_waited_still_issues_both_legs() {
    // `--timeout 0` is "do the steps, confirm nothing", and its deadline is expired from the first
    // instant — so a rule phrased on what is left alone would drop both legs.
    assert_eq!(leg(Budget::Immediate, Budget::Immediate), Leg::Under(Budget::Immediate));
    assert_eq!(
        leg(Budget::Bounded(Duration::ZERO), Budget::Immediate),
        Leg::Under(Budget::Immediate)
    );
}

#[skuld::test]
fn an_unbounded_budget_is_never_spent() {
    assert_eq!(leg(Budget::Unbounded, Budget::Unbounded), Leg::Under(Budget::Unbounded));
}

// deadline threading ==================================================================================================

fn mk(id: &str) -> DaemonSpec {
    DaemonSpec {
        id: Id::try_from(id).unwrap(),
        name: id.to_string(),
        command: vec!["daemon".to_owned()],
        cwd: None,
        env: BTreeMap::new(),
        user: User::Root,
        restart: Restart::OnFailure,
        restart_delay: None,
        logs: None,
        kind: Kind::Simple,
    }
}

/// How many deadlines `daemon restart --no-timeout <ids>` derives against a
/// `Fake` where every id is installed and every leg succeeds.
///
/// `--no-timeout` rather than a bounded budget on purpose: `Budget::Unbounded`
/// resolves to `Deadline(None)`, so nothing in the run consults a clock and
/// nothing about the result depends on how long the process takes. A bounded
/// budget would make a sufficiently stalled machine abandon the restart and
/// turn this into a bet on wall-clock time. The count is the same either way —
/// it is a property of the call graph, not of the budget's kind.
fn deadlines_derived(ids: &[&str]) -> usize {
    let fake = Fake::new();
    for id in ids {
        fake.install(&mk(id), false).expect("seed an installed daemon");
    }
    let args = Args {
        ids: ids.iter().map(|id| (*id).to_string()).collect(),
        wait: WaitArgs {
            timeout: None,
            no_timeout: true,
        },
    };

    let derived = Cell::new(0_usize);
    let start_clock = |budget: Budget| {
        derived.set(derived.get() + 1);
        budget.start()
    };
    let mut out: Vec<u8> = Vec::new();
    let mut err: Vec<u8> = Vec::new();
    let code = run_with(
        &args,
        &|| -> Result<Box<dyn ServiceManager>> { Ok(Box::new(fake.clone())) },
        &|| true,
        &start_clock,
        &mut out,
        &mut err,
    );

    assert_eq!(
        code,
        0,
        "the fixture must restart cleanly, or the count is of a path nobody takes: {}",
        String::from_utf8_lossy(&err)
    );
    derived.get()
}

/// One budget covers the whole restart, so one deadline is derived for both
/// legs. A fresh one per leg would let a `--timeout 30s` restart take 60s —
/// twice what the user named, because the operation happens to have two steps.
#[skuld::test]
fn one_restart_derives_one_deadline_for_both_legs() {
    assert_eq!(deadlines_derived(&["frpc"]), 1);
}

/// Per id, not per invocation: `restart a b --timeout 30s` promises each of
/// the two 30s rather than leaving `b` whatever `a` did not spend.
#[skuld::test]
fn every_id_derives_a_deadline_of_its_own() {
    assert_eq!(deadlines_derived(&["frpc", "websocat"]), 2);
}

// a spent budget ======================================================================================================

/// A bounded budget already spent when the stop leg is issued is a timeout, not `--timeout 0`: the
/// stop that goes out under it confirms nothing, so the restart must neither report it stopped nor
/// start into it. The clock is injected already spent, so nothing here depends on how long the
/// run takes.
#[skuld::test]
fn a_bounded_budget_spent_before_the_stop_leg_is_a_timeout_and_starts_nothing() {
    let fake = Fake::new();
    fake.install(&mk("frpc"), false).expect("seed an installed daemon");
    let args = Args {
        ids: vec!["frpc".to_string()],
        wait: WaitArgs {
            timeout: Some(Duration::from_secs(30)),
            no_timeout: false,
        },
    };
    let mut out: Vec<u8> = Vec::new();
    let mut err: Vec<u8> = Vec::new();

    let code = run_with(
        &args,
        &|| -> Result<Box<dyn ServiceManager>> { Ok(Box::new(fake.clone())) },
        &|| true,
        &|_| Budget::Immediate.start(),
        &mut out,
        &mut err,
    );

    let err = String::from_utf8_lossy(&err);
    assert_eq!(code, 4, "{err}");
    assert_eq!(
        fake.calls(),
        vec![("stop", "frpc".to_string())],
        "the stop is still issued, and the start never is"
    );
    assert!(err.contains("did not report stopped within 30s"), "{err}");
    assert!(err.contains("no start was issued"), "{err}");
    assert!(err.contains("may be left stopped"), "{err}");
    assert!(!err.contains("was stopped"), "nothing confirmed the stop: {err}");
    assert!(out.is_empty(), "{}", String::from_utf8_lossy(&out));
}
