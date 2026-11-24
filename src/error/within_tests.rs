use std::error::Error as _;
use std::io;
use std::sync::Arc;

use super::*;
use crate::error::{Error, KillProgress};

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
    let carried = || Error::RequestInDoubt {
        request: "read".to_string(),
        reached: false,
        note: None,
        source: "no answer".into(),
    };
    let e = unread_after_open_refused(7, denied(), io::Error::other(carried()));

    let Some(Error::ManagerUnreadAfterOpenRefused { source, .. }) = e.get_ref().and_then(|e| e.downcast_ref::<Error>())
    else {
        panic!("the unread variant: {e:?}");
    };
    assert!(matches!(**source, Error::RequestInDoubt { .. }), "{source:?}");

    let told = target_failed("hold", e);

    assert!(told.is_indeterminate(), "{told:?}");
    let report = told.report().to_string();
    assert!(
        report.starts_with(
            "failed to hold: the process (pid 7) could not be opened (denied), and the re-read of what runs the \
             daemon failed: "
        ),
        "{report}"
    );
    assert!(report.ends_with(&carried().report().to_string()), "{report}");
}

/// A plain read failure is the bare OS error under the hold's words, which already say the re-read failed,
/// and the failure has the read's kind.
#[skuld::test]
fn a_plain_failed_read_after_a_refused_open_is_the_bare_os_error_under_the_same_error() {
    let e = unread_after_open_refused(7, denied(), io::Error::new(io::ErrorKind::TimedOut, "read failed"));

    assert_eq!(e.kind(), io::ErrorKind::TimedOut);
    assert_eq!(
        target_failed("hold", e).report().to_string(),
        "failed to hold: the process (pid 7) could not be opened (denied), and the re-read of what runs the \
         daemon failed: read failed"
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

/// `within` is a general helper (a stream's name, a privilege step): a failure said within a context is
/// still a failure of the operation, which keeps `failed to`, whether it carries a goetia error or not.
#[skuld::test]
fn a_failure_said_within_a_context_by_a_target_is_still_a_failure_of_the_operation() {
    let carrying = target_failed("kill `x`", within("stdout", io::Error::other(Error::blob("carried"))));
    assert_eq!(
        carrying.report().to_string(),
        "failed to kill `x`: stdout: blob: carried"
    );
    assert!(matches!(carrying.cause(), Error::Blob { .. }), "{carrying:?}");

    let plain = target_failed("kill `x`", within("stdout", denied()));
    assert_eq!(plain.report().to_string(), "failed to kill `x`: stdout: denied");
}

/// A kill that signalled is the chain's: a context said around it keeps the report from reading as a failed
/// operation.
#[skuld::test]
fn a_context_around_a_kill_that_signalled_still_says_it_was_signalled() {
    for source in [denied(), io::Error::other(Error::blob("carried"))] {
        let wrapped = within("outer", kill_unconfirmed(7, KillProgress::Signalled, source));

        let report = target_failed("kill `x`", wrapped).report().to_string();

        assert!(
            report.starts_with("kill `x`: outer: process 7 was signalled, but goetia could not establish"),
            "{report}"
        );
    }
}

/// `carried`, a goetia error, held in each of the payloads the one description of an `io::Error` looks
/// through, as an `io::Error` carries it.
fn carried_in_each_payload(carried: fn() -> Error) -> Vec<(&'static str, io::Error)> {
    vec![
        ("directly", io::Error::other(carried())),
        ("a Box<Error>", io::Error::other(Box::new(carried()))),
        ("an io::Error", io::Error::other(io::Error::other(carried()))),
        (
            "an io::Error in an io::Error",
            io::Error::other(io::Error::other(io::Error::other(Box::new(carried())))),
        ),
    ]
}

fn carries_error(e: &io::Error) -> bool {
    look(e).is_some()
}

fn blob() -> Error {
    Error::blob("carried")
}

fn in_doubt() -> Error {
    Error::RequestInDoubt {
        request: "read".to_string(),
        reached: false,
        note: None,
        source: "no answer".into(),
    }
}

/// What the search and the printer see through, every walk of an `io::Error` sees through.
#[skuld::test]
fn a_goetia_error_is_carried_through_every_payload_the_search_looks_through() {
    for (how, e) in carried_in_each_payload(blob) {
        assert!(carries_error(&e), "{how}: {e:?}");
    }
    assert!(!carries_error(&denied()));
    assert!(!carries_error(&io::Error::other(Arc::new(denied()))));
}

/// A target's failure that carries a goetia error in any such payload is a step over it, as a directly
/// carried one is, and its text is the same.
#[skuld::test]
fn a_targets_failure_is_the_goetia_error_it_carries_in_whatever_payload() {
    for (how, e) in carried_in_each_payload(blob) {
        let failed = target_failed("kill `x`", e);

        assert!(
            matches!(&failed, Error::Step { what, .. } if what == "failed to kill `x`"),
            "{how}: {failed:?}"
        );
        assert_eq!(
            failed.report().to_string(),
            "failed to kill `x`: blob: carried",
            "{how}"
        );
    }
}

/// The class of the error a failure carries is the failure's, in whatever payload it is carried, said within
/// contexts or not: a hold or a kill that left the outcome open exits `4`.
#[skuld::test]
fn an_indeterminate_error_is_indeterminate_in_whatever_payload_it_is_carried() {
    for (how, e) in carried_in_each_payload(in_doubt) {
        let failed = target_failed("kill `x`", e);
        assert!(failed.is_indeterminate(), "{how}: {failed:?}");
        assert_eq!(
            failed.report().to_string(),
            format!("failed to kill `x`: {}", in_doubt().report()),
            "{how}"
        );
    }
    for (how, e) in carried_in_each_payload(in_doubt) {
        let failed = target_failed("kill `x`", within("outer", e));
        assert!(failed.is_indeterminate(), "{how} within a context: {failed:?}");
    }
}

/// A kill that signalled is told with no verdict on the kill and is indeterminate, in whatever payload it
/// is carried, over whatever failed after the signal.
#[skuld::test]
fn a_kill_that_signalled_is_told_with_no_verdict_in_whatever_payload() {
    for (how, e) in carried_in_each_payload(|| Error::KillUnconfirmed {
        pid: 7,
        progress: KillProgress::Exited,
        source: Box::new(blob()),
    }) {
        let failed = target_failed("kill `x`", e);

        assert!(failed.is_indeterminate(), "{how}: {failed:?}");
        assert_eq!(
            failed.report().to_string(),
            "kill `x`: process 7 was signalled and has exited, but goetia could not confirm that the manager \
             recorded it: blob: carried",
            "{how}"
        );
    }
}

/// The failure after the signal is carried as the typed error `told` makes of it, and the kind is kept.
#[skuld::test]
fn what_failed_after_the_signal_is_carried_typed_with_its_kind() {
    let e = kill_unconfirmed(7, KillProgress::Signalled, io::Error::new(io::ErrorKind::TimedOut, "wait"));

    assert_eq!(e.kind(), io::ErrorKind::TimedOut);
    let failed = target_failed("kill `x`", e);
    let Error::Step { source, .. } = &failed else {
        panic!("a step: {failed:?}");
    };
    let Error::KillUnconfirmed { source, .. } = &**source else {
        panic!("the kill's variant: {failed:?}");
    };
    assert!(matches!(**source, Error::RawOs { .. }), "{source:?}");
}
