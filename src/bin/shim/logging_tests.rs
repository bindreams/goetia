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
fn write_line_creates_missing_directories_and_writes_the_line() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let path = dir.path().join("Goetia").join("logs").join("x.log");

    write_line(&path, "the line");

    assert_eq!(
        std::fs::read_to_string(&path).expect("the file was written"),
        "the line\n"
    );
}

#[skuld::test]
fn write_line_appends_rather_than_truncates() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let path = dir.path().join("x.log");

    write_line(&path, "first");
    write_line(&path, "second");

    assert_eq!(
        std::fs::read_to_string(&path).expect("the file was written"),
        "first\nsecond\n"
    );
}

#[skuld::test]
fn write_line_is_best_effort_when_the_path_cannot_be_opened() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let file = dir.path().join("a-file");
    std::fs::write(&file, "x").expect("a regular file to use as a parent");

    // A directory cannot be created under a regular file: this must neither panic nor write.
    write_line(&file.join("x.log"), "lost");

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
        None,
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
        None,
        |line| seen = Some(line.to_string()),
    );

    assert_eq!(seen.as_deref(), Some("the line"));
}

#[skuld::test]
fn write_channels_writes_the_daemon_log_and_not_the_default_when_one_is_set() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let default = dir.path().join("default").join("svc.log");
    let daemon = dir.path().join("daemon").join("svc.log");
    let mut seen = None;

    write_channels(
        "svc",
        "the line",
        |_| default.clone(),
        Some(&daemon),
        |line| seen = Some(line.to_string()),
    );

    assert_eq!(
        (std::fs::read_to_string(&daemon).ok(), default.exists(), seen.as_deref()),
        (Some("the line\n".to_string()), false, Some("the line")),
        "the line goes to the daemon's log, not the default, and still to the Event Log"
    );
}

#[skuld::test]
fn write_channels_writes_the_default_when_no_daemon_log_is_set() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let default = dir.path().join("default").join("svc.log");

    write_channels("svc", "the line", |_| default.clone(), None, |_| {});

    assert_eq!(std::fs::read_to_string(&default).ok().as_deref(), Some("the line\n"));
}

#[skuld::test]
fn write_channels_writes_the_default_when_the_daemon_log_cannot_be_opened() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let default = dir.path().join("default").join("svc.log");
    let file = dir.path().join("a-file");
    std::fs::write(&file, "x").expect("a regular file to use as a parent");
    let unopenable = file.join("svc.log");

    write_channels("svc", "the line", |_| default.clone(), Some(&unopenable), |_| {});

    assert_eq!(std::fs::read_to_string(&default).ok().as_deref(), Some("the line\n"));
}

#[skuld::test]
fn daemon_log_path_is_the_spec_s_logs_or_the_default() {
    let set = Path::new(r"D:\logs\mine.log");

    assert_eq!(
        (daemon_log_path("svc", Some(set)), daemon_log_path("svc", None)),
        (set.to_path_buf(), default_log_path("svc"))
    );
}

/// Records each `write` call; fails them all when `fail` is set.
#[derive(Default)]
struct Recorder {
    writes: Vec<Vec<u8>>,
    fail: bool,
}

impl std::io::Write for Recorder {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        if self.fail {
            return Err(std::io::Error::other("disk full"));
        }
        self.writes.push(buf.to_vec());
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[skuld::test]
fn append_line_is_one_write_so_daemon_output_cannot_land_between_text_and_newline() {
    let mut out = Recorder::default();

    append_line(&mut out, "the line").expect("the write succeeds");

    assert_eq!(out.writes, vec![b"the line\n".to_vec()]);
}

#[skuld::test]
fn append_line_reports_a_failed_write() {
    let mut out = Recorder {
        fail: true,
        ..Recorder::default()
    };

    assert!(append_line(&mut out, "the line").is_err());
}

#[skuld::test]
fn write_line_says_whether_the_line_was_written() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let file = dir.path().join("a-file");
    std::fs::write(&file, "x").expect("a regular file to use as a parent");

    assert_eq!(
        (
            write_line(&dir.path().join("ok.log"), "the line"),
            write_line(&file.join("x.log"), "the line")
        ),
        (true, false)
    );
}

#[skuld::test]
fn log_destinations_try_the_daemon_log_then_the_default() {
    let daemon = PathBuf::from("daemon.log");
    let default = PathBuf::from("default.log");

    assert_eq!(log_destinations(Some(&daemon), default.clone()), vec![daemon, default]);
}

#[skuld::test]
fn log_destinations_try_the_default_once_when_there_is_no_daemon_log() {
    let default = PathBuf::from("default.log");

    assert_eq!(log_destinations(None, default.clone()), vec![default]);
}

#[skuld::test]
fn log_destinations_do_not_retry_the_default_when_it_is_the_daemon_log() {
    let default = PathBuf::from("default.log");

    assert_eq!(log_destinations(Some(&default), default.clone()), vec![default]);
}

#[skuld::test]
fn log_destinations_compare_paths_case_insensitively() {
    let default = PathBuf::from(r"C:\ProgramData\Goetia\logs\svc.log");
    let daemon = PathBuf::from(r"c:\programdata\goetia\LOGS\svc.log");

    assert_eq!(log_destinations(Some(&daemon), default.clone()), vec![default]);
}
