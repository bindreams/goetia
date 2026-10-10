//! `log_failure`'s test-build seam, and the production file channel it stands in for.
//!
//! The journal tests check the fixture, not production: only the first one is a smoke test of the
//! seam itself. The Event Log half of production is covered by the shim integration tests.

use super::test_hook::{REPORTED, take};
use super::*;
use crate::test_support::TestId;

const PREFIX: &str = "goetia-shim-logging-test";

// seam ================================================================================================================

#[skuld::test]
fn log_failure_journals_instead_of_writing_on_any_thread() {
    let id = TestId::new(PREFIX);
    assert!(
        !default_log_path(id.as_str()).exists(),
        "the fallback log exists before the test wrote anything"
    );

    std::thread::scope(|scope| {
        scope.spawn(|| log_failure(id.as_str(), "y"));
    });

    assert_eq!(
        (take(id.as_str()), default_log_path(id.as_str()).exists()),
        (vec![format!("goetia-shim[{}]: y", id.as_str())], false),
        "a line from another thread must be journalled under its id and must not reach the file"
    );
}

// journal fixture -----------------------------------------------------------------------------------------------------

#[skuld::test]
fn lines_come_back_oldest_first() {
    let id = TestId::new(PREFIX);

    log_failure(id.as_str(), "first");
    log_failure(id.as_str(), "second");

    assert_eq!(
        take(id.as_str()),
        vec![
            format!("goetia-shim[{}]: first", id.as_str()),
            format!("goetia-shim[{}]: second", id.as_str()),
        ]
    );
}

#[skuld::test]
fn ids_do_not_see_each_others_lines() {
    let (a, b) = (TestId::new(PREFIX), TestId::new(PREFIX));

    log_failure(a.as_str(), "from a");
    log_failure(b.as_str(), "from b");

    assert_eq!(take(a.as_str()), vec![format!("goetia-shim[{}]: from a", a.as_str())]);
    assert_eq!(take(b.as_str()), vec![format!("goetia-shim[{}]: from b", b.as_str())]);
}

#[skuld::test]
fn take_of_an_id_never_reported_is_empty() {
    let id = TestId::new(PREFIX);
    assert!(take(id.as_str()).is_empty());

    log_failure(id.as_str(), "once");
    assert_eq!(take(id.as_str()).len(), 1);
    assert!(take(id.as_str()).is_empty(), "take must remove what it returns");
}

#[skuld::test]
fn a_poisoned_journal_still_records_and_returns() {
    let id = TestId::new(PREFIX);

    std::thread::scope(|scope| {
        let panicked = scope
            .spawn(|| {
                let _held = REPORTED.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                panic!("poisoning the journal on purpose");
            })
            .join();
        assert!(panicked.is_err());
    });
    assert!(
        REPORTED.is_poisoned(),
        "the journal was not poisoned, so this test proves nothing"
    );

    log_failure(id.as_str(), "after poison");
    let taken = take(id.as_str());
    REPORTED.clear_poison();

    assert_eq!(taken, vec![format!("goetia-shim[{}]: after poison", id.as_str())]);
}

// production file channel =============================================================================================

#[skuld::test]
fn write_fallback_creates_missing_directories_and_writes_the_line() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let path = dir.path().join("Goetia").join("logs").join("x.log");

    write_fallback(&path, "the line");

    assert_eq!(
        std::fs::read_to_string(&path).expect("the file was written"),
        "the line\n"
    );
}

#[skuld::test]
fn write_fallback_appends_rather_than_truncates() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let path = dir.path().join("x.log");

    write_fallback(&path, "first");
    write_fallback(&path, "second");

    assert_eq!(
        std::fs::read_to_string(&path).expect("the file was written"),
        "first\nsecond\n"
    );
}

#[skuld::test]
fn write_fallback_is_best_effort_when_the_path_cannot_be_opened() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let file = dir.path().join("a-file");
    std::fs::write(&file, "x").expect("a regular file to use as a parent");

    // A directory cannot be created under a regular file: this must neither panic nor write.
    write_fallback(&file.join("x.log"), "lost");

    assert_eq!(std::fs::read_to_string(&file).expect("the file is untouched"), "x");
}

#[skuld::test]
fn failure_line_is_the_id_in_brackets_then_the_message() {
    assert_eq!(failure_line("svc", "boom"), "goetia-shim[svc]: boom");
}
