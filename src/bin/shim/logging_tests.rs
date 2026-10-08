//! `log_failure`'s test-build seam: it journals under the daemon id rather than writing the
//! fallback file and the Event Log, from any thread. What production writes is proven by
//! `tests/shim_integration`'s `unreadable_blob_logs_to_fallback_path_and_event_log`.

use super::test_hook::{REPORTED, take};
use super::*;
use crate::test_support::TestId;

const PREFIX: &str = "goetia-shim-logging-test";

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
