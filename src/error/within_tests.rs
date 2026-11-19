use std::error::Error as _;
use std::io;

use super::*;
use crate::error::Error;

fn denied() -> io::Error {
    io::Error::new(io::ErrorKind::PermissionDenied, "denied")
}

#[skuld::test]
fn a_failure_said_within_a_context_keeps_its_kind() {
    assert_eq!(within("read stdout", denied()).kind(), io::ErrorKind::PermissionDenied);
}

#[skuld::test]
fn a_failure_said_within_a_context_displays_only_the_context() {
    assert_eq!(within("read stdout", denied()).to_string(), "read stdout");
}

#[skuld::test]
fn the_failure_under_the_context_is_its_source() {
    let e = within("read stdout", denied());
    assert_eq!(e.source().expect("the cause is the source").to_string(), "denied");
}

#[skuld::test]
fn a_report_prints_the_context_and_then_the_cause_once() {
    let e = Error::os("capture", within("read stdout", denied()));
    assert_eq!(e.report().to_string(), "failed to capture: read stdout: denied");
}

/// A goetia error that a pair's primary carries comes back as itself, classified as it was, whatever
/// is told after it.
#[skuld::test]
fn a_goetia_error_carried_by_a_pairs_primary_comes_back_with_its_class() {
    let primary = io::Error::other(Error::RequestInDoubt {
        request: "read".to_string(),
        reached: false,
        note: None,
        source: "no answer".into(),
    });
    let pair = paired(primary, "and also", denied());

    let back = carried(pair).expect("the primary carries a goetia error");

    assert!(back.is_indeterminate(), "{back:?}");
    assert!(matches!(back, Error::RequestInDoubt { .. }), "{back:?}");
}

/// A pair whose primary carries no goetia error is not one: it stays the pair, whole.
#[skuld::test]
fn a_pair_whose_primary_carries_no_goetia_error_stays_a_pair() {
    let pair = paired(denied(), "and also", io::Error::other("second"));

    let back = carried(pair).expect_err("no goetia error is carried");

    assert_eq!(back.to_string(), "denied (and also second)");
    assert_eq!(back.kind(), io::ErrorKind::PermissionDenied);
}
