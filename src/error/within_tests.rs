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

/// A read that failed with a goetia error is the source of the error the hold's failure becomes: its class
/// is the failure's, and the refused open is told with it.
#[skuld::test]
fn a_goetia_error_the_read_carries_is_the_source_of_a_hold_that_could_not_tell_a_refusal() {
    let read = io::Error::other(Error::RequestInDoubt {
        request: "read".to_string(),
        reached: false,
        note: None,
        source: "no answer".into(),
    });

    let back = carried(unread_after_open_refused(7, denied(), read)).expect("an error is carried");

    assert!(back.is_indeterminate(), "{back:?}");
    let Error::ManagerUnreadAfterOpenRefused {
        pid,
        open_refused,
        source,
    } = &back
    else {
        panic!("{back:?}");
    };
    assert_eq!(*pid, 7);
    assert_eq!(open_refused.to_string(), "denied");
    assert!(matches!(**source, Error::RequestInDoubt { .. }), "{back:?}");
}

/// A plain read failure is an OS failure under the same error, and the failure has the read's kind.
#[skuld::test]
fn a_plain_failed_read_after_a_refused_open_is_an_os_failure_under_the_same_error() {
    let e = unread_after_open_refused(7, denied(), io::Error::new(io::ErrorKind::TimedOut, "read"));

    assert_eq!(e.kind(), io::ErrorKind::TimedOut);
    let back = carried(e).expect("an error is carried");
    let Error::ManagerUnreadAfterOpenRefused { source, .. } = &back else {
        panic!("{back:?}");
    };
    assert!(
        matches!(&**source, Error::Os { what, .. } if what == "read it"),
        "{back:?}"
    );
}

/// A target's failure is the goetia error it carries, under a step that says what was done, else an OS
/// failure of what was done.
#[skuld::test]
fn a_targets_failure_keeps_what_was_done_whether_or_not_it_carries_a_goetia_error() {
    let carrying = target_failed("kill `x`", io::Error::other(Error::blob("carried")));
    assert!(
        matches!(&carrying, Error::Step { what, .. } if what == "failed to kill `x`"),
        "{carrying:?}"
    );
    assert!(matches!(carrying.cause(), Error::Blob { .. }), "{carrying:?}");
    assert!(
        carrying.report().to_string().starts_with("failed to kill `x`: "),
        "{}",
        carrying.report()
    );

    let plain = target_failed("kill `x`", denied());
    assert_eq!(plain.report().to_string(), "failed to kill `x`: denied");
}
