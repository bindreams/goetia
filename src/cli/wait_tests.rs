use std::time::Duration;

use super::{WaitArgs, parse_timeout};
use crate::error::Error;
use crate::manager::Budget;
use crate::spec::{StopAction, StopSignal};

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
fn a_refused_duration_is_quoted_with_the_parser_reason() {
    let message = parse_timeout("1").unwrap_err();

    assert_eq!(
        message,
        "`1` is not a duration (e.g. `30s`, `2m30s`): time unit needed, for example 1sec or 1ms"
    );
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

// The default wait of a stop ==========================================================================================

fn after(signal: StopSignal, timeout: Option<u64>) -> Budget {
    args(None, false).stop_budget(|| {
        Ok(StopAction {
            send_signal: signal,
            timeout: timeout.map(Duration::from_secs),
        })
    })
}

#[skuld::test]
fn a_stop_waits_for_its_timeout_and_the_teardown_margin() {
    assert_eq!(
        after(StopSignal::Terminate, Some(30)),
        Budget::Bounded(Duration::from_secs(40))
    );
    assert_eq!(
        after(StopSignal::Interrupt, Some(1)),
        Budget::Bounded(Duration::from_secs(11))
    );
    assert_eq!(
        after(StopSignal::Quit, Some(86_400)),
        Budget::Bounded(Duration::from_secs(86_410))
    );
    assert_eq!(StopAction::TEARDOWN_MARGIN, Duration::from_secs(10));
}

#[skuld::test]
fn no_timeout_is_an_unbounded_wait_and_kill_keeps_the_previous_default() {
    assert_eq!(after(StopSignal::Terminate, None), Budget::Unbounded);
    assert_eq!(after(StopSignal::Kill, None), Budget::DEFAULT);
}

#[skuld::test]
fn a_flag_decides_without_reading_the_stop_action() {
    for (given, expected) in [
        (
            args(Some(Duration::from_secs(3)), false),
            Budget::Bounded(Duration::from_secs(3)),
        ),
        (args(Some(Duration::ZERO), false), Budget::Immediate),
        (args(None, true), Budget::Unbounded),
    ] {
        let budget = given.stop_budget(|| panic!("a flag was given: the daemon's stop action is not read"));
        assert_eq!(budget, expected);
    }
}

#[skuld::test]
fn a_stop_action_that_cannot_be_read_gets_the_previous_default_whatever_the_reason() {
    // The verb meets the same gate itself, and says why.
    let kept = Error::Kept(std::sync::Arc::new(Error::blob("unreadable".to_string())));
    let absent = Error::NotInstalled { id: "frpc".to_string() };
    for error in [kept, absent] {
        assert_eq!(args(None, false).stop_budget(|| Err(error)), Budget::DEFAULT);
    }
}
