use std::cell::Cell;
use std::collections::BTreeMap;
use std::time::Duration;

use super::{Args, NextLeg, WaitArgs, after_stop, run_with};
use crate::error::Result;
use crate::manager::fake::Fake;
use crate::manager::{Budget, ServiceManager};
use crate::spec::{DaemonSpec, Id, Kind, Restart, User};

const THIRTY: Budget = Budget::Bounded(Duration::from_secs(30));

#[skuld::test]
fn a_bounded_budget_the_stop_leg_spent_abandons_the_restart() {
    assert_eq!(after_stop(THIRTY, Budget::Immediate), NextLeg::Abandon);
    assert_eq!(after_stop(THIRTY, Budget::Bounded(Duration::ZERO)), NextLeg::Abandon);
}

#[skuld::test]
fn what_is_left_of_a_bounded_budget_is_what_the_start_leg_gets() {
    let left = Budget::Bounded(Duration::from_secs(5));
    assert_eq!(after_stop(THIRTY, left), NextLeg::Start(left));
}

#[skuld::test]
fn a_budget_that_never_waited_still_issues_the_start() {
    // `--timeout 0` is "do the steps, confirm nothing", and its deadline is expired from the first
    // instant — so an abandon rule phrased on what is left alone would drop the start leg.
    assert_eq!(
        after_stop(Budget::Immediate, Budget::Immediate),
        NextLeg::Start(Budget::Immediate)
    );
    assert_eq!(
        after_stop(Budget::Bounded(Duration::ZERO), Budget::Immediate),
        NextLeg::Start(Budget::Immediate)
    );
}

#[skuld::test]
fn an_unbounded_budget_never_abandons() {
    assert_eq!(
        after_stop(Budget::Unbounded, Budget::Unbounded),
        NextLeg::Start(Budget::Unbounded)
    );
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
