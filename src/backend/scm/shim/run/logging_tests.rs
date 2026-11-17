//! `Log`'s composition of the two channels, over the `Fake`'s stated replies, and the pure helpers
//! under it. What `Real` does to a file is `os_tests`'s; the Event Log half of production is covered
//! by the shim integration tests.

use super::*;
use crate::backend::scm::shim::run::os::fake::{ChannelCall, FakeGuard};
use crate::error::Error;

const LINE: &str = "goetia-shim[svc]: the line";

fn appended(path: &Path, parents: Parents, ok: bool) -> ChannelCall {
    ChannelCall::Append {
        path: path.to_path_buf(),
        line: LINE.to_string(),
        parents,
        ok,
    }
}

/// The default log: the shim makes no folder for it.
fn appended_default(path: &Path, ok: bool) -> ChannelCall {
    appended(path, Parents::Never, ok)
}

/// The daemon's own `logs` path, whose folders are the user's to name and so are made.
fn appended_own(path: &Path, ok: bool) -> ChannelCall {
    appended(path, Parents::Create, ok)
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

/// A daemon with no `logs` writes its default log, so its path is the default and no folder is made for
/// it, whatever the call that set it.
#[skuld::test]
fn a_daemon_log_that_is_the_default_is_written_with_no_folder_made() {
    let fake = FakeGuard::new();
    let log = log_over(&fake);
    let default = Path::new("logs").join("svc.log");
    log.set_daemon_log(default.clone());

    log.failure("svc", "the line");

    assert_eq!(
        fake.take_channel_calls(),
        vec![appended_default(&default, true), reported()]
    );
}

#[skuld::test]
fn parents_are_made_for_every_path_but_the_default() {
    let fake = FakeGuard::new();
    let log = log_over(&fake);

    assert_eq!(
        (
            log.parents_of("svc", &Path::new("logs").join("svc.log")),
            log.parents_of("svc", Path::new(r"D:\logs\mine.log")),
            log.parents_of("other", &Path::new("logs").join("svc.log")),
        ),
        (Parents::Never, Parents::Create, Parents::Create)
    );
}

#[skuld::test]
fn daemon_log_path_is_the_spec_s_logs_or_the_default() {
    let fake = FakeGuard::new();
    let log = log_over(&fake);
    let set = Path::new(r"D:\logs\mine.log");

    assert_eq!(
        (log.daemon_log_path("svc", Some(set)), log.daemon_log_path("svc", None)),
        (Some(set.to_path_buf()), Some(Path::new("logs").join("svc.log")))
    );
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
            log.daemon_log_path("svc", None),
            log.daemon_log_path("svc", Some(set))
        ),
        (None, None, Some(set.to_path_buf()))
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
        vec![(daemon, Parents::Create), (default, Parents::Never)]
    );
}

#[skuld::test]
fn log_destinations_try_the_default_once_when_there_is_no_daemon_log() {
    let default = PathBuf::from("default.log");

    assert_eq!(log_destinations(None, default.clone()), vec![(default, Parents::Never)]);
}

#[skuld::test]
fn log_destinations_do_not_retry_the_default_when_it_is_the_daemon_log() {
    let default = PathBuf::from("default.log");

    assert_eq!(
        log_destinations(Some(&default), default.clone()),
        vec![(default, Parents::Never)]
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
        Log::real().default_log_path("svc"),
        Some(program_data.join("Goetia").join("logs").join("svc.log"))
    );
}
