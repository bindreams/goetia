//! `log_failure`'s test-build seam, and the production file channel it stands in for.
//!
//! The journal has one smoke test, of the seam itself. The Event Log half of production is covered
//! by the shim integration tests.

use super::test_hook::take;
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

#[skuld::test]
fn write_channels_writes_the_file_first_then_gives_the_event_log_the_same_line() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let path = dir.path().join("Goetia").join("logs").join("svc.log");
    let mut asked_for = None;
    let mut seen = None;

    write_channels(
        "svc",
        "the line",
        |id| {
            asked_for = Some(id.to_string());
            path.clone()
        },
        |line| seen = Some((line.to_string(), std::fs::read_to_string(&path).ok())),
    );

    assert_eq!(
        asked_for.as_deref(),
        Some("svc"),
        "the path must be derived from the id"
    );
    assert_eq!(
        seen,
        Some(("the line".to_string(), Some("the line\n".to_string()))),
        "the Event Log must get the line the file got, after the file had it"
    );
}

#[skuld::test]
fn write_channels_still_reaches_the_event_log_when_the_file_cannot_be_opened() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let file = dir.path().join("a-file");
    std::fs::write(&file, "x").expect("a regular file to use as a parent");
    let mut seen = None;

    write_channels(
        "svc",
        "the line",
        |_| file.join("x.log"),
        |line| seen = Some(line.to_string()),
    );

    assert_eq!(seen.as_deref(), Some("the line"));
}
