use std::time::Duration;

use super::{WaitArgs, parse_timeout};
use crate::manager::Budget;

fn args(timeout: Option<Duration>, no_timeout: bool) -> WaitArgs {
    WaitArgs { timeout, no_timeout }
}

#[skuld::test]
fn parses_the_manifest_duration_grammar() {
    assert_eq!(parse_timeout("10s").unwrap(), Duration::from_secs(10));
    assert_eq!(parse_timeout("500ms").unwrap(), Duration::from_millis(500));
    // Both spellings of one duration. argv splitting does not apply here, which is the only
    // reason the spaced form is testable at all — see
    // `a_spaced_duration_after_a_variadic_positional_becomes_an_id`.
    assert_eq!(parse_timeout("2m 30s").unwrap(), Duration::from_secs(150));
    assert_eq!(parse_timeout("2m30s").unwrap(), Duration::from_secs(150));
}

#[skuld::test]
fn parses_a_bare_zero() {
    assert_eq!(parse_timeout("0").unwrap(), Duration::ZERO);
}

#[skuld::test]
fn rejects_a_number_with_no_unit() {
    assert!(parse_timeout("1").is_err(), "an unadorned number names no unit");
}

#[skuld::test]
fn budget_defaults_to_ten_seconds_when_no_flag_is_given() {
    assert_eq!(args(None, false).budget(), Budget::Bounded(Duration::from_secs(10)));
    assert_eq!(args(None, false).budget(), Budget::DEFAULT);
}

#[skuld::test]
fn zero_is_immediate() {
    assert_eq!(args(Some(Duration::ZERO), false).budget(), Budget::Immediate);
}

#[skuld::test]
fn no_timeout_is_unbounded() {
    assert_eq!(args(None, true).budget(), Budget::Unbounded);
}

#[skuld::test]
fn was_given_is_false_only_when_neither_flag_appears() {
    assert!(!args(None, false).was_given());
    assert!(args(Some(Duration::from_secs(30)), false).was_given());
    assert!(args(None, true).was_given());
}
