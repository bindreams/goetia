//! `Log`'s composition of the two channels, over the `Fake`'s stated replies, and the pure helpers
//! under it. What `Real` does to a file is `os_tests`'s; the Event Log half of production is covered
//! by the shim integration tests.

use super::*;
use crate::backend::scm::shim::run::os::fake::{ChannelCall, FakeGuard, HOLD_FAILED};
use crate::error::Error;

const LINE: &str = "goetia-shim[svc]: the line";

fn appended(path: &Path, kind: LogKind, ok: bool) -> ChannelCall {
    ChannelCall::Append {
        path: path.to_path_buf(),
        line: LINE.to_string(),
        kind,
        ok,
    }
}

/// The default log: the shim makes no folder for it.
fn appended_default(path: &Path, ok: bool) -> ChannelCall {
    appended(path, LogKind::Default, ok)
}

/// The daemon's own `logs` path, whose folders are the user's to name and so are made.
fn appended_own(path: &Path, ok: bool) -> ChannelCall {
    appended(path, LogKind::Own, ok)
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
        vec![appended_default(&Path::new("logs").join("svc.log"), true), reported()],
        "the path must be derived from the id, with no folder made for it, and the Event Log must get the line the file got, after the file had it"
    );
}

#[skuld::test]
fn a_failure_still_reaches_the_event_log_when_the_file_cannot_be_opened() {
    let fake = FakeGuard::new();
    let default = Path::new("logs").join("svc.log");
    fake.will_fail_append(&default);

    log_over(&fake).failure("svc", "the line");

    assert_eq!(
        fake.take_channel_calls(),
        vec![appended_default(&default, false), reported()]
    );
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
        vec![appended_own(&daemon, true), reported()],
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
        vec![
            appended_own(&daemon, false),
            appended_default(&default, true),
            reported()
        ]
    );
}

/// The shim compares no paths: a daemon with no `logs` writes its default log, which an install made and the
/// shim only opens; any `logs:` is the user's own, whose parents are made. An install refuses a `logs:` in the
/// folder of the default logs, so the two are never the same file.
#[skuld::test]
fn the_default_log_is_the_one_of_an_unset_logs_and_every_set_logs_is_the_users_own() {
    let fake = FakeGuard::new();
    let log = log_over(&fake);
    let set = Path::new(r"D:\logs\mine.log");

    assert_eq!(
        (log.daemon_log("svc", None), log.daemon_log("svc", Some(set))),
        (
            Some((Path::new("logs").join("svc.log"), LogKind::Default)),
            Some((set.to_path_buf(), LogKind::Own))
        )
    );
}

/// A `logs:` that an install of an earlier version allowed and that is the default log's path as written is
/// the default log: opened as the install's file, not as the user's own, which the file's permissions refuse.
#[skuld::test]
fn a_logs_equal_to_the_default_path_as_written_is_the_default_log() {
    let fake = FakeGuard::new();
    let log = log_over(&fake);
    let default = Path::new("logs").join("svc.log");

    assert_eq!(
        log.daemon_log("svc", Some(&default)),
        Some((default.clone(), LogKind::Default))
    );
    assert_eq!(
        log.daemon_log("other", Some(&default)),
        Some((default, LogKind::Own)),
        "another daemon's default log is not this one's"
    );
}

/// The shim opens its default log once, from its start, and holds it ([`Channels::hold_default`]).
#[skuld::test]
fn holding_the_default_log_asks_the_channels_to_hold_the_log_of_the_id() {
    let fake = FakeGuard::new();

    log_over(&fake).hold_default("svc");

    assert_eq!(
        fake.take_channel_calls(),
        vec![ChannelCall::HoldDefault {
            path: Path::new("logs").join("svc.log")
        }]
    );
}

/// A default log that cannot be held is told to the Event Log, once, with the error and the file: the
/// daemon's own open of it says the same only for the output, and the shim's lines would otherwise fail
/// without a word. The shim goes on: each line opens the file by its path.
#[skuld::test]
fn a_default_log_that_cannot_be_held_is_reported_to_the_event_log_once_and_lines_still_try_the_path() {
    let fake = FakeGuard::new();
    let log = log_over(&fake);
    let default = Path::new("logs").join("svc.log");
    fake.will_fail_hold(&default);

    log.hold_default("svc");

    let calls = fake.take_channel_calls();
    let [
        ChannelCall::HoldDefault { .. },
        ChannelCall::EventLog { severity, line },
    ] = calls.as_slice()
    else {
        panic!("a hold, then one line to the Event Log: {calls:?}");
    };
    assert_eq!(*severity, Severity::Error);
    assert!(line.starts_with("goetia-shim[svc]: "), "{line}");
    assert!(line.contains(&default.display().to_string()), "the file: {line}");
    assert!(line.contains(HOLD_FAILED), "the error: {line}");

    log.failure("svc", "the line");
    assert_eq!(
        fake.take_channel_calls(),
        vec![appended_default(&default, true), reported()],
        "the shim goes on with the path"
    );
}

/// One fault is told once per start, whoever meets it: a file the shim could not hold, and then could not open
/// for the daemon's output either, is one line; so is a file the marker and the output both fail to open. Another
/// file's fault is another line.
#[skuld::test]
fn a_fault_in_one_file_is_told_once_whoever_meets_it() {
    let fake = FakeGuard::new();
    let log = log_over(&fake);
    let default = Path::new("logs").join("svc.log");
    let other = Path::new("elsewhere").join("daemon.log");
    let fault = std::io::Error::other("denied");
    fake.will_fail_hold(&default);

    log.hold_default("svc");
    log.open_failed("svc", &default, &fault);
    log.open_failed("svc", &other, &fault);
    log.open_failed("svc", &other, &fault);

    let told = fake.take_failures();
    let [held, opened] = told.as_slice() else {
        panic!("one line for each of the two files: {told:?}");
    };
    assert!(held.contains("hold the default log"), "{held}");
    assert!(
        opened.starts_with(&format!("goetia-shim[svc]: open log file {}: denied", other.display())),
        "{opened}"
    );
}

/// A machine with no `%ProgramData%` has no default log to hold.
#[skuld::test]
fn a_log_with_no_default_file_holds_nothing() {
    let fake = FakeGuard::new();
    let log = Log::from_program_data(
        fake.channels(),
        Err(Error::os("locate %ProgramData%", std::io::Error::other("x"))),
    );
    fake.take_channel_calls();

    log.hold_default("svc");

    assert_eq!(fake.take_channel_calls(), vec![]);
}

#[skuld::test]
fn a_machine_that_cannot_name_program_data_has_no_default_file_and_the_event_log_says_why_first() {
    let fake = FakeGuard::new();
    let cause = Error::os("locate %ProgramData%", std::io::Error::other("no such known folder"));
    let why = cause.report().to_string();

    let log = Log::from_program_data(fake.channels(), Err(cause));

    let first = fake.take_channel_calls();
    let [ChannelCall::EventLog { severity, line }] = first.as_slice() else {
        panic!("the only call is the Event Log's, with nothing appended to a file: {first:?}");
    };
    assert_eq!(*severity, Severity::Error);
    assert!(line.contains(&why), "{line}");
    assert!(line.contains("no log file"), "{line}");
    let set = Path::new(r"D:\logs\mine.log");
    assert_eq!(
        (
            log.default_log_path("svc"),
            log.daemon_log("svc", None),
            log.daemon_log("svc", Some(set))
        ),
        (None, None, Some((set.to_path_buf(), LogKind::Own)))
    );
    log.failure("svc", "the line");
    assert_eq!(
        fake.take_channel_calls(),
        vec![reported()],
        "a failure goes to the Event Log alone"
    );
}

#[skuld::test]
fn a_log_with_no_default_file_still_writes_the_daemons_own() {
    let fake = FakeGuard::new();
    let log = Log::from_program_data(
        fake.channels(),
        Err(Error::os("locate %ProgramData%", std::io::Error::other("x"))),
    );
    fake.take_channel_calls();
    let daemon = Path::new("daemon").join("svc.log");
    log.set_daemon_log(daemon.clone());

    log.failure("svc", "the line");

    assert_eq!(fake.take_channel_calls(), vec![appended_own(&daemon, true), reported()]);
}

#[skuld::test]
fn log_destinations_try_the_daemon_log_then_the_default() {
    let daemon = PathBuf::from("daemon.log");
    let default = PathBuf::from("default.log");

    assert_eq!(
        log_destinations(Some(&daemon), default.clone()),
        vec![(daemon, LogKind::Own), (default, LogKind::Default)]
    );
}

#[skuld::test]
fn log_destinations_try_the_default_once_when_there_is_no_daemon_log() {
    let default = PathBuf::from("default.log");

    assert_eq!(
        log_destinations(None, default.clone()),
        vec![(default, LogKind::Default)]
    );
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
        vec![
            appended_own(&daemon, false),
            appended_default(&default, false),
            reported()
        ],
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

    assert_eq!(fake.take_channel_calls(), vec![appended_own(&daemon, true), reported()]);
}

#[skuld::test]
fn program_data_is_the_known_folder_and_logging_uses_it() {
    let program_data = crate::backend::scm::folder::program_data().unwrap();

    assert_eq!(
        program_data.to_str().map(str::to_lowercase),
        std::env::var("ProgramData").ok().map(|dir| dir.to_lowercase()),
        "the known folder is where the environment says it is, unless a process redirects the variable"
    );
    assert_eq!(
        Log::real(Arc::default()).default_log_path("svc"),
        Some(program_data.join("Goetia").join("logs").join("svc.log"))
    );
}
