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

/// A goetia error that a pair's primary carries comes back as the pair's [`PairKind`] around it: its
/// class is the pair's, and the second is told with it.
#[skuld::test]
fn a_goetia_error_carried_by_a_pairs_primary_is_the_source_of_the_error_the_pair_becomes() {
    let primary = io::Error::other(Error::RequestInDoubt {
        request: "read".to_string(),
        reached: false,
        note: None,
        source: "no answer".into(),
    });
    let pair = paired(primary, PairKind::ManagerUnreadAfterOpenRefused, denied());

    let back = carried(pair).expect("the primary carries a goetia error");

    assert!(back.is_indeterminate(), "{back:?}");
    let Error::ManagerUnreadAfterOpenRefused { open_refused, source } = &back else {
        panic!("{back:?}");
    };
    assert_eq!(open_refused.to_string(), "denied");
    assert!(matches!(**source, Error::RequestInDoubt { .. }), "{back:?}");
}

/// A pair whose primary carries no goetia error is not one: it stays the pair, whole.
#[skuld::test]
fn a_pair_whose_primary_carries_no_goetia_error_stays_a_pair() {
    let pair = paired(
        denied(),
        PairKind::ManagerUnreadAfterOpenRefused,
        io::Error::other("second"),
    );

    let back = carried(pair).expect_err("no goetia error is carried");

    assert_eq!(
        back.to_string(),
        "denied (after the service's process could not be opened: second)"
    );
    assert_eq!(back.kind(), io::ErrorKind::PermissionDenied);
}

/// A pair of two OS failures has no goetia error to classify: even one its primary carries leaves it whole.
#[cfg(windows)]
#[skuld::test]
fn a_pair_of_a_denied_open_and_a_failed_step_stays_whole_whatever_its_primary_carries() {
    let primary = io::Error::other(Error::blob("carried"));
    let pair = paired(primary, PairKind::DebugPrivilegeAlsoFailed, denied());

    let back = carried(pair).expect_err("this pair carries no class");

    assert_eq!(
        back.to_string(),
        "blob: carried (enabling SeDebugPrivilege for the open also failed: denied)"
    );
}

/// A target's failure is the goetia error it carries, else an OS failure of what was done.
#[skuld::test]
fn a_targets_failure_is_the_error_it_carries_else_an_os_failure_of_what_was_done() {
    let carrying = target_failed("kill `x`", io::Error::other(Error::blob("carried")));
    assert!(matches!(carrying, Error::Blob { .. }), "{carrying:?}");

    let plain = target_failed("kill `x`", denied());
    assert_eq!(plain.report().to_string(), "failed to kill `x`: denied");
}
