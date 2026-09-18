use std::cell::Cell;
use std::collections::BTreeMap;
use std::time::Duration;

use super::{Args, Leg, SPENT, WaitArgs, leg, run_with, stop_leg};
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

/// The stop leg always gets a budget, and a spent bounded one is handed on as the least bound there
/// is — never `Immediate`, which does not wait, and on launchd runs `bootout` to completion
/// unbounded.
#[skuld::test]
fn a_stop_leg_whose_bounded_budget_is_spent_still_gets_a_bound() {
    assert_eq!(SPENT, Budget::Bounded(Duration::from_nanos(1)));
    assert!(SPENT.waits());
    assert_eq!(stop_leg(THIRTY, Budget::Immediate), SPENT);
    let left = Budget::Bounded(Duration::from_secs(5));
    assert_eq!(stop_leg(THIRTY, left), left);
    assert_eq!(stop_leg(Budget::Immediate, Budget::Immediate), Budget::Immediate);
    assert_eq!(stop_leg(Budget::Unbounded, Budget::Unbounded), Budget::Unbounded);
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

/// `daemon restart frpc --timeout 30s` with the clock injected already spent, so nothing here
/// depends on how long the run takes. Returns the exit code and stderr.
fn restart_on_a_spent_clock(fake: &Fake) -> (i32, String, String) {
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
    (
        code,
        String::from_utf8_lossy(&out).into_owned(),
        String::from_utf8_lossy(&err).into_owned(),
    )
}

/// A bounded budget already spent when the stop leg is issued takes the path of a bounded stop that
/// ran out: the stop goes out, nothing confirms it, and the restart neither reports it stopped nor
/// starts into it. `Immediate` in its place would confirm nothing and return `Ok` — which the
/// stalled `Fake` models exactly — and this would then read "was stopped".
#[skuld::test]
fn a_bounded_budget_spent_before_an_unconfirmed_stop_is_a_timeout_and_starts_nothing() {
    let fake = Fake::new();
    fake.install(&mk("frpc"), false).expect("seed an installed daemon");
    fake.seed_stop_stalls("frpc");

    let (code, out, err) = restart_on_a_spent_clock(&fake);

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
    assert!(out.is_empty(), "{out}");
}

/// The same spent budget over a stop the manager does confirm: the message reports what was
/// established — stopped, and no start issued — rather than a stop that "did not report stopped".
#[skuld::test]
fn a_bounded_budget_spent_before_a_confirmed_stop_reports_it_stopped_and_starts_nothing() {
    let fake = Fake::new();
    fake.install(&mk("frpc"), false).expect("seed an installed daemon");
    fake.start(&Id::try_from("frpc").unwrap(), Budget::Unbounded)
        .expect("seed a running daemon");

    let (code, out, err) = restart_on_a_spent_clock(&fake);

    assert_eq!(code, 4, "{err}");
    assert_eq!(
        fake.calls(),
        vec![("start", "frpc".to_string()), ("stop", "frpc".to_string())],
        "after the seeding start, the stop is issued and the start never is"
    );
    assert!(err.contains("`frpc` was stopped"), "{err}");
    assert!(err.contains("did not report running within 30s"), "{err}");
    assert!(err.contains("no start was issued"), "{err}");
    assert!(!err.contains("did not report stopped"), "the stop was confirmed: {err}");
    assert!(out.is_empty(), "{out}");
}
