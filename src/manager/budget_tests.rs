use std::time::Duration;

use super::*;

// Budget ==============================================================================================================

#[skuld::test]
fn default_is_ten_seconds() {
    assert_eq!(Budget::DEFAULT, Budget::Bounded(Duration::from_secs(10)));
}

#[skuld::test]
fn waits_is_false_for_immediate_and_true_for_bounded_and_unbounded() {
    assert!(!Budget::Immediate.waits());
    assert!(Budget::Bounded(Duration::from_secs(1)).waits());
    assert!(Budget::Unbounded.waits());
}

#[skuld::test]
fn waits_is_false_for_a_zero_length_bounded_budget() {
    // Against the natural `!matches!(self, Immediate)` this fails, which is
    // the point: a zero-length bound is not a fourth state that waits.
    assert!(!Budget::Bounded(Duration::ZERO).waits());
    assert!(Budget::Bounded(Duration::ZERO).start().expired());
}

#[skuld::test]
fn unbounded_never_expires() {
    let deadline = Budget::Unbounded.start();
    assert_eq!(deadline.remaining(), None);
    assert!(!deadline.expired());
}

#[skuld::test]
fn immediate_starts_already_expired() {
    assert!(Budget::Immediate.start().expired());
}

#[skuld::test]
fn a_bounded_deadline_reports_a_remaining_shorter_than_the_budget() {
    let budget = Duration::from_secs(60);
    let remaining = Budget::Bounded(budget).start().remaining().unwrap();
    assert!(remaining <= budget);
    assert!(remaining > Duration::ZERO);
}

#[skuld::test]
fn remaining_millis_saturates_below_sleepex_infinity() {
    // This test cannot catch a `start()` overflow: `Instant::now()
    // .checked_add(Duration::from_secs(u64::MAX / 1000))` is `Some`,
    // measured. `a_budget_too_large_to_represent_is_unbounded_not_a_panic`
    // is the test that catches that.
    let deadline = Budget::Bounded(Duration::from_secs(u64::MAX / 1000)).start();
    assert_eq!(deadline.remaining_millis_capped(), Some(u32::MAX - 1));
    assert_ne!(deadline.remaining_millis_capped(), Some(u32::MAX));
}

#[skuld::test]
fn a_budget_too_large_to_represent_is_unbounded_not_a_panic() {
    // The largest duration `humantime` will actually parse, since that is
    // the value a user can actually type into `--timeout`.
    let d = humantime::parse_duration("18446744073709551615s").unwrap();
    let deadline = Budget::Bounded(d).start();
    assert_eq!(deadline.remaining(), None);
    assert!(!deadline.expired());
    assert_eq!(deadline.remaining_millis_capped(), None);
}

// budget_for ==========================================================================================================

#[skuld::test]
fn a_spent_deadline_yields_an_immediate_budget() {
    assert_eq!(budget_for(Budget::Immediate.start()), Budget::Immediate);
    assert_eq!(budget_for(Budget::Unbounded.start()), Budget::Unbounded);

    match budget_for(Budget::DEFAULT.start()) {
        Budget::Bounded(d) => {
            assert!(d > Duration::ZERO);
            assert!(d <= Duration::from_secs(10));
        }
        other => panic!("expected Budget::Bounded, got {other:?}"),
    }
}

// timed_out ===========================================================================================================

#[skuld::test]
fn timed_out_names_the_id_the_state_and_how_long_it_waited() {
    let e = timed_out("frpc", "running", Budget::Bounded(Duration::from_secs(10)));

    let message = e.to_string();
    assert!(message.contains("frpc"), "{message}");
    assert!(message.contains("running"), "{message}");
    assert!(
        message.contains("10s"),
        "the waited duration must be rendered: {message}"
    );
}

#[skuld::test]
fn timed_out_is_a_wait_timeout_carrying_the_budget_it_was_given() {
    match timed_out("frpc", "stopped", Budget::Bounded(Duration::from_secs(3))) {
        crate::Error::WaitTimeout {
            id,
            awaited,
            waited,
            recovery,
        } => {
            assert_eq!(id, "frpc");
            assert_eq!(awaited, "stopped");
            assert_eq!(waited, Duration::from_secs(3));
            assert!(!recovery.is_empty());
        }
        other => panic!("expected Error::WaitTimeout, got {other:?}"),
    }
}

/// The three things `recovery` owes a reader, and the one it must not
/// imply: goetia stopped waiting, the request was **not** cancelled, and
/// both the way to look and the two ways to wait longer.
#[skuld::test]
fn the_recovery_says_the_request_was_not_cancelled_and_names_both_ways_to_wait_longer() {
    let crate::Error::WaitTimeout { recovery, .. } = timed_out("frpc", "running", Budget::DEFAULT) else {
        panic!("timed_out must produce Error::WaitTimeout");
    };

    assert!(recovery.contains("not cancelled"), "{recovery}");
    assert!(recovery.contains("goetia daemon status frpc"), "{recovery}");
    assert!(recovery.contains("--timeout"), "{recovery}");
    assert!(recovery.contains("--no-timeout"), "{recovery}");
}

/// Only a non-zero `Bounded` budget can expire: `Immediate` and
/// `Bounded(ZERO)` never waited, and `Unbounded` never ends. A backend that
/// reaches this constructor with one of those has reported an expiry that
/// provably cannot have happened.
#[skuld::test]
#[should_panic(expected = "only a non-zero Bounded budget can expire")]
fn timed_out_refuses_a_budget_that_cannot_expire() {
    let _ = timed_out("frpc", "running", Budget::Unbounded);
}

#[skuld::test]
#[should_panic(expected = "only a non-zero Bounded budget can expire")]
fn timed_out_refuses_a_zero_length_budget() {
    let _ = timed_out("frpc", "running", Budget::Bounded(Duration::ZERO));
}

// Deadline ============================================================================================================

#[skuld::test]
fn zero_millis_means_expired() {
    for deadline in [
        Budget::Immediate.start(),
        Budget::Bounded(Duration::from_nanos(1)).start(),
    ] {
        // Read `remaining_millis_capped()` first: the implication is
        // deterministic in this order because a deadline only ever moves
        // toward expiry, not away from it.
        if deadline.remaining_millis_capped() == Some(0) {
            assert!(deadline.expired());
        }
    }
}

#[skuld::test]
fn an_unbounded_deadline_has_no_millis() {
    assert_eq!(Budget::Unbounded.start().remaining_millis_capped(), None);
}

#[skuld::test]
fn at_is_some_exactly_when_the_deadline_is_bounded() {
    assert_eq!(Budget::Unbounded.start().at(), None);
    assert!(Budget::DEFAULT.start().at().is_some());

    let immediate = Budget::Immediate.start();
    let t = immediate.at().expect("Immediate deadline is bounded");
    assert!(t <= std::time::Instant::now());
}

// millis_ceil =========================================================================================================

#[skuld::test]
fn millis_ceil_rounds_a_sub_millisecond_remainder_up_to_one() {
    assert_eq!(millis_ceil(Duration::from_nanos(1)), 1);
    assert_eq!(millis_ceil(Duration::from_micros(1_001)), 2);
    assert_eq!(millis_ceil(Duration::from_millis(1)), 1);
    assert_eq!(millis_ceil(Duration::new(1, 999_999_999)), 2000);
}

#[skuld::test]
fn millis_ceil_of_zero_is_zero() {
    assert_eq!(millis_ceil(Duration::ZERO), 0);
}

#[skuld::test]
fn millis_ceil_of_the_largest_duration_saturates_without_panicking() {
    assert_eq!(millis_ceil(Duration::MAX), u32::MAX - 1);
    // Pins the saturation as a clamp at the top, not a blanket answer.
    assert_eq!(millis_ceil(Duration::from_millis(u32::MAX as u64 - 2)), u32::MAX - 2);
}
