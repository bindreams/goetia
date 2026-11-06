//! `Log`'s composition of the two channels, over the `Fake`'s stated replies, and the pure helpers
//! under it. What `Real` does to a file is `os_tests`'s; the Event Log half of production is covered
//! by the shim integration tests.

use super::*;
use crate::os::fake::{ChannelCall, FakeGuard};

const LINE: &str = "goetia-shim[svc]: the line";

fn appended(path: &Path, ok: bool) -> ChannelCall {
    ChannelCall::Append {
        path: path.to_path_buf(),
        line: LINE.to_string(),
        ok,
    }
}

fn reported() -> ChannelCall {
    ChannelCall::EventLog {
        severity: Severity::Error,
        line: LINE.to_string(),
    }
}

fn log_over(fake: &FakeGuard) -> Log {
    Log::new(fake.channels(), PathBuf::from("logs"))
}

#[skuld::test]
fn failure_line_is_the_id_in_brackets_then_the_message() {
    assert_eq!(failure_line("svc", "boom"), "goetia-shim[svc]: boom");
}

#[skuld::test]
fn a_failure_goes_to_the_default_file_first_then_the_event_log_gets_the_same_line() {
    let fake = FakeGuard::new();

    log_over(&fake).failure("svc", "the line");

    assert_eq!(
        fake.take_channel_calls(),
        vec![appended(&Path::new("logs").join("svc.log"), true), reported()],
        "the path must be derived from the id, and the Event Log must get the line the file got, after the file had it"
    );
}

#[skuld::test]
fn a_failure_still_reaches_the_event_log_when_the_file_cannot_be_opened() {
    let fake = FakeGuard::new();
    let default = Path::new("logs").join("svc.log");
    fake.will_fail_append(&default);

    log_over(&fake).failure("svc", "the line");

    assert_eq!(fake.take_channel_calls(), vec![appended(&default, false), reported()]);
}

#[skuld::test]
fn a_failure_goes_to_the_daemon_log_and_not_the_default_when_one_is_set() {
    let fake = FakeGuard::new();
    let log = log_over(&fake);
    let daemon = Path::new("daemon").join("svc.log");
    log.set_daemon_log(daemon.clone());

    log.failure("svc", "the line");

    assert_eq!(
        fake.take_channel_calls(),
        vec![appended(&daemon, true), reported()],
        "the line goes to the daemon's log, not the default, and still to the Event Log"
    );
}

#[skuld::test]
fn a_failure_goes_to_the_default_when_the_daemon_log_cannot_be_opened() {
    let fake = FakeGuard::new();
    let log = log_over(&fake);
    let daemon = Path::new("daemon").join("svc.log");
    let default = Path::new("logs").join("svc.log");
    log.set_daemon_log(daemon.clone());
    fake.will_fail_append(&daemon);

    log.failure("svc", "the line");

    assert_eq!(
        fake.take_channel_calls(),
        vec![appended(&daemon, false), appended(&default, true), reported()]
    );
}

#[skuld::test]
fn daemon_log_path_is_the_spec_s_logs_or_the_default() {
    let fake = FakeGuard::new();
    let log = log_over(&fake);
    let set = Path::new(r"D:\logs\mine.log");

    assert_eq!(
        (log.daemon_log_path("svc", Some(set)), log.daemon_log_path("svc", None)),
        (set.to_path_buf(), Path::new("logs").join("svc.log"))
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
fn a_failure_still_reaches_the_event_log_when_neither_file_can_be_opened() {
    let fake = FakeGuard::new();
    let log = log_over(&fake);
    let daemon = Path::new("daemon").join("svc.log");
    let default = Path::new("logs").join("svc.log");
    log.set_daemon_log(daemon.clone());
    fake.will_fail_append(&daemon);
    fake.will_fail_append(&default);

    log.failure("svc", "the line");

    assert_eq!(
        fake.take_channel_calls(),
        vec![appended(&daemon, false), appended(&default, false), reported()],
        "each file is tried once, and the Event Log still gets the line"
    );
}

#[skuld::test]
fn setting_the_daemon_log_again_to_the_same_path_is_accepted() {
    let fake = FakeGuard::new();
    let log = log_over(&fake);
    let daemon = Path::new("daemon").join("svc.log");
    log.set_daemon_log(daemon.clone());
    log.set_daemon_log(daemon.clone());

    log.failure("svc", "the line");

    assert_eq!(fake.take_channel_calls(), vec![appended(&daemon, true), reported()]);
}
