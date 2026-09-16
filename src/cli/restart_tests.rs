use std::time::Duration;

use super::{NextLeg, after_stop};
use crate::manager::Budget;

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
