use std::sync::Arc;
use std::time::Duration;

use super::{Leg, SPENT, abandoned_before_start, after_start_failed, leg, restart_until, stop_leg};
use crate::error::Error;
use crate::manager::budget::process_timed_out;
use crate::manager::fake::Fake;
use crate::manager::fake::fake_tests::mk;
use crate::manager::{Budget, ServiceManager};
use crate::spec::Id;

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

// a spent budget ======================================================================================================

/// `frpc`'s restart under a 30s budget whose deadline is already spent, so nothing depends on how
/// long the run takes.
fn restart_with_a_spent_deadline(fake: &Fake) -> String {
    let id = Id::try_from("frpc").unwrap();
    let e = restart_until(fake, &id, THIRTY, Budget::Immediate.start()).expect_err("a spent budget is a timeout");
    assert!(e.is_indeterminate(), "{}", e.report());
    e.report().to_string()
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

    let err = restart_with_a_spent_deadline(&fake);

    assert_eq!(
        fake.calls(),
        vec![("stop", "frpc".to_string())],
        "the stop is still issued, and the start never is"
    );
    assert!(err.contains("did not report stopped within 30s"), "{err}");
    assert!(err.contains("no start was issued"), "{err}");
    assert!(err.contains("may be left stopped"), "{err}");
    assert!(!err.contains("was stopped"), "nothing confirmed the stop: {err}");
}

/// The same spent budget over a stop the manager does confirm: the message reports what was
/// established — stopped, and no start issued — rather than a stop that "did not report stopped".
#[skuld::test]
fn a_bounded_budget_spent_before_a_confirmed_stop_reports_it_stopped_and_starts_nothing() {
    let fake = Fake::new();
    fake.install(&mk("frpc"), false).expect("seed an installed daemon");
    fake.start(&Id::try_from("frpc").unwrap(), Budget::Unbounded)
        .expect("seed a running daemon");

    let err = restart_with_a_spent_deadline(&fake);

    assert_eq!(
        fake.calls(),
        vec![("start", "frpc".to_string()), ("stop", "frpc".to_string())],
        "after the seeding start, the stop is issued and the start never is"
    );
    assert!(err.contains("`frpc` was stopped"), "{err}");
    assert!(err.contains("did not report running within 30s"), "{err}");
    assert!(err.contains("no start was issued"), "{err}");
    assert!(!err.contains("did not report stopped"), "the stop was confirmed: {err}");
}

// wording =============================================================================================================

fn in_doubt() -> Error {
    Error::RequestInDoubt {
        request: "systemctl start x.service".to_string(),
        reached: true,
        note: None,
        source: std::io::Error::other("lost").into(),
    }
}

/// A leg in doubt stays in doubt, whatever the budget, and keeps what goetia knows of how far it
/// got — never flattened into the refused start of a restart that does not wait, or into a
/// failure.
#[skuld::test]
fn a_leg_in_doubt_stays_in_doubt_under_every_budget() {
    let id = Id::try_from("frpc").unwrap();
    for budget in [Budget::Immediate, Budget::Unbounded, Budget::DEFAULT] {
        for e in [
            after_start_failed(&id, budget, in_doubt()),
            abandoned_before_start(&id, in_doubt(), budget),
        ] {
            assert!(
                matches!(e, Error::RequestInDoubt { reached: true, .. }),
                "{budget:?}: {e:?}"
            );
        }
    }
}

/// A stop leg that SCM confirmed, whose process outlived the leg's budget: the restart says it was
/// abandoned with no start issued, that the daemon is stopped, and how to bring it back; it reports
/// the `--timeout` as given, not the leg's remainder; and the variant stays, so the exit is `4`.
#[skuld::test]
fn a_stop_leg_whose_process_outlasted_the_budget_abandons_the_restart_under_the_given_timeout() {
    let id = Id::try_from("frpc").unwrap();
    let leg_budget = Budget::Bounded(Duration::from_secs(5));
    let e = abandoned_before_start(&id, process_timed_out("frpc", 4242, leg_budget, None), THIRTY);

    assert!(e.is_indeterminate(), "{e:?}");
    let Error::ProcessWaitTimeout {
        pid, waited, recovery, ..
    } = &e
    else {
        panic!("the variant is kept: {e:?}");
    };
    assert_eq!(*pid, 4242);
    assert_eq!(
        *waited,
        Duration::from_secs(30),
        "the user's --timeout, not the leg's remainder"
    );
    assert!(recovery.contains("The restart was abandoned"), "{recovery}");
    assert!(recovery.contains("no start was issued"), "{recovery}");
    assert!(recovery.contains("`goetia daemon start frpc`"), "{recovery}");
    assert!(!recovery.contains("may or may not have reached stopped"), "{recovery}");
    assert!(e.to_string().contains("is stopped, but its process (pid 4242)"), "{e}");
}

/// The abandoned restart re-words the recovery, not the fact that goetia could not end the process.
#[skuld::test]
fn an_abandoned_restart_keeps_the_failed_terminate_of_the_process() {
    let id = Id::try_from("frpc").unwrap();
    let leg_budget = Budget::Bounded(Duration::from_secs(5));
    let failed = Arc::new(std::io::Error::other("The handle is invalid. (os error 6)"));
    let timed_out = process_timed_out("frpc", 4242, leg_budget, Some(failed));

    let e = abandoned_before_start(&id, timed_out, THIRTY);

    let report = e.report().to_string();
    assert!(
        report.contains(
            "had not exited within 30s, and terminating it failed: The handle is invalid. (os error 6). The \
             restart was abandoned"
        ),
        "{report}"
    );
    assert_eq!(report, e.to_string(), "the whole sentence is the error's text");
    assert!(
        std::error::Error::source(&e).is_none(),
        "the failed terminate is named in the text, not the source"
    );
}

/// What the stop leg established is said ahead of the cause, and the cause stays the typed source.
#[skuld::test]
fn a_start_leg_in_doubt_says_what_the_stop_established_and_keeps_its_cause() {
    let id = Id::try_from("frpc").unwrap();

    let e = after_start_failed(&id, Budget::DEFAULT, in_doubt());

    assert!(
        e.report()
            .to_string()
            .ends_with("`frpc` was stopped, and this start was attempted after it: lost"),
        "{}",
        e.report()
    );
    assert!(std::error::Error::source(&e).is_some());
}

/// A start leg whose read of the id failed after the stop is still `Undetermined`, with the stop's
/// clause ahead of the failed read, and the remedy last.
#[skuld::test]
fn an_undetermined_start_leg_keeps_its_read_under_the_stop_clause() {
    let id = Id::try_from("frpc").unwrap();
    let undetermined = Error::Undetermined {
        id: "frpc".to_string(),
        reason: Box::new(Error::os("read", std::io::Error::other("denied"))),
        recovery: "re-run as root".to_string(),
    };

    let e = after_start_failed(&id, Budget::DEFAULT, undetermined);

    assert!(matches!(e, Error::Undetermined { .. }), "{e:?}");
    assert_eq!(
        e.report().to_string(),
        "cannot determine whether daemon `frpc` is installed: stopped but failed to restart: failed to read: \
         denied. re-run as root"
    );
}

/// Under a budget that does not wait, the clause is the stop-leg fact the user most needs: it comes
/// before the failed read, and the remedy after it.
#[skuld::test]
fn an_undetermined_start_leg_after_an_unwaited_stop_says_so_before_the_read_and_the_remedy_last() {
    let id = Id::try_from("frpc").unwrap();
    let undetermined = Error::Undetermined {
        id: "frpc".to_string(),
        reason: Box::new(Error::os("read", std::io::Error::other("denied"))),
        recovery: "re-run as root".to_string(),
    };

    let e = after_start_failed(&id, Budget::Immediate, undetermined);

    assert_eq!(
        e.report().to_string(),
        "cannot determine whether daemon `frpc` is installed: the stop was issued without waiting for it, and the \
         start that followed was not determined: failed to read: denied. re-run as root"
    );
}

/// A refused start after a stop nobody waited for keeps the refusal as the cause.
#[skuld::test]
fn a_refused_start_after_an_unwaited_stop_is_unestablished_with_the_refusal_as_its_cause() {
    let id = Id::try_from("frpc").unwrap();
    let refused = Error::os("start `frpc`", std::io::Error::other("refused"));

    let e = after_start_failed(&id, Budget::Immediate, refused);

    assert!(matches!(e, Error::Unestablished { .. }), "{e:?}");
    assert!(e.is_indeterminate());
    assert!(
        e.report().to_string().ends_with(": failed to start `frpc`: refused"),
        "{}",
        e.report()
    );
}

/// A leg that failed as a step over a request in doubt is in doubt as the bare request is: the
/// re-wording reaches the request under the step, and the step stays outermost in the report.
#[skuld::test]
fn a_step_over_a_leg_in_doubt_is_reworded_under_the_step() {
    let id = Id::try_from("frpc").unwrap();
    let step = || Error::Step {
        what: "start".to_string(),
        source: Box::new(in_doubt()),
    };

    for budget in [Budget::Immediate, Budget::Unbounded, Budget::DEFAULT] {
        let started = after_start_failed(&id, budget, step());
        assert!(matches!(started, Error::Step { .. }), "{budget:?}: {started:?}");
        assert!(
            matches!(started.cause(), Error::RequestInDoubt { .. }),
            "{budget:?}: {started:?}"
        );
        let text = started.report().to_string();
        assert!(text.starts_with("start: `systemctl start x.service` reached"), "{text}");
        assert!(text.contains("this start was attempted after it"), "{text}");

        let stopped = abandoned_before_start(&id, step(), budget);
        assert!(
            matches!(stopped.cause(), Error::RequestInDoubt { .. }),
            "{budget:?}: {stopped:?}"
        );
        assert!(
            stopped.report().to_string().contains("the restart was abandoned"),
            "{}",
            stopped.report()
        );
    }
}

/// A start judged as a managed service's is its failed start for the wording too: the request in
/// doubt under it is reworded, and the unreadable blob stays in the report.
#[skuld::test]
fn a_start_judged_as_managed_over_a_leg_in_doubt_is_reworded_under_it() {
    let id = Id::try_from("frpc").unwrap();
    let judged = || Error::StartAsManaged {
        blob: std::sync::Arc::new(Error::blob("corrupt")),
        source: Box::new(in_doubt()),
    };

    for budget in [Budget::Immediate, Budget::Unbounded, Budget::DEFAULT] {
        let started = after_start_failed(&id, budget, judged());
        assert!(
            matches!(started, Error::StartAsManaged { .. }),
            "{budget:?}: {started:?}"
        );
        assert!(
            matches!(started.cause(), Error::RequestInDoubt { .. }),
            "{budget:?}: {started:?}"
        );
        let text = started.report().to_string();
        assert!(text.contains("this start was attempted after it"), "{budget:?}: {text}");
        assert!(text.contains("(blob: corrupt)"), "{budget:?}: {text}");
    }
}
