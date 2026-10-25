use cosca::identity::ProcessId;
use goetia::backend::scm::exit_status::{ERROR_SERVICE_ALREADY_RUNNING, Reported};
use std::time::Duration;

use goetia::backend::scm::state_file::{self, Actions, Clock, FailureRecord, Loaded, RestartDue, StateRecord};

use super::*;
use crate::logging::test_hook::{take, take_warnings};
use crate::test_support::{TestId, assert_one_failure};

const PREFIX: &str = "goetia-shim-state-test";

const BOOT: u64 = 7;

/// What the OS says of a path whose directory does not exist.
const ERROR_PATH_NOT_FOUND: i32 = 3;

fn clock(ms: u64) -> Clock {
    Clock { boot: BOOT, ms }
}

/// A store over a fresh directory, whose clock stands at `ms` and whose failure actions read as
/// `due`.
fn store(ms: u64, due: Result<RestartDue, String>) -> (Store, tempfile::TempDir) {
    let dir = tempfile::tempdir().expect("a temp dir for the state file");
    let path = dir.path().join("svc");
    (Store::at(path, clock(ms), due), dir)
}

fn record_of(store: &Store) -> StateRecord {
    match state_file::read(&store.path).expect("read") {
        Loaded::Record(record) => record,
        other => panic!("{other:?}"),
    }
}

fn this_process() -> cosca::identity::ProcessIdRecord {
    ProcessId::current().to_record().expect("this process has an identity")
}

// begin ===============================================================================================================

#[skuld::test]
fn a_start_with_no_state_file_proceeds_in_silence() {
    let id = TestId::new(PREFIX);
    let (store, _dir) = store(100, Ok(RestartDue::Never));

    assert_eq!(begin(id.as_str(), &store), Ok(()));

    assert!(!store.path.exists(), "a start never creates the file");
}

#[skuld::test]
fn a_start_over_a_running_survivor_is_a_shim_failure_that_names_the_process_and_the_remedy() {
    let id = TestId::new(PREFIX);
    let (store, _dir) = store(100, Ok(RestartDue::Never));
    let mut record = StateRecord::empty();
    record.daemon = Some(this_process());
    state_file::write(&store.path, &record).expect("write");

    let refused = begin(id.as_str(), &store);

    assert_eq!(
        refused,
        Err(Reported::shim_failure(Some(ERROR_SERVICE_ALREADY_RUNNING as i32)))
    );
    assert_one_failure(
        &take(id.as_str()),
        id.as_str(),
        &format!(
            "not starting the daemon: process {} is the daemon an earlier run launched, and it is still running; a second one would run beside it. Stop that process, then start the service. If it is not running, delete the state file {}",
            std::process::id(),
            store.path.display()
        ),
    );
    assert_eq!(
        record_of(&store),
        record,
        "the record is left for the next start to judge"
    );
}

#[skuld::test]
fn a_start_over_a_survivor_that_has_gone_forgets_it_and_proceeds() {
    let id = TestId::new(PREFIX);
    let (store, _dir) = store(100, Ok(RestartDue::Never));
    let mut gone = this_process();
    gone.token += 1;
    let mut record = StateRecord::empty();
    record.daemon = Some(gone);
    state_file::write(&store.path, &record).expect("write");

    assert_eq!(begin(id.as_str(), &store), Ok(()));

    assert_eq!(record_of(&store), StateRecord::empty());
}

#[skuld::test]
fn a_start_over_a_state_file_that_does_not_decode_resets_it_and_warns() {
    let id = TestId::new(PREFIX);
    let (store, _dir) = store(100, Ok(RestartDue::Never));
    std::fs::write(&store.path, "{not json").expect("write garbage");

    assert_eq!(begin(id.as_str(), &store), Ok(()));

    assert_eq!(record_of(&store), StateRecord::empty());
    let warnings = take_warnings(id.as_str());
    let [line] = warnings.as_slice() else {
        panic!("{warnings:?}");
    };
    assert!(
        line.starts_with(&format!(
            "goetia-shim[{}]: the state file {} could not be read and was reset: ",
            id.as_str(),
            store.path.display()
        )),
        "{line}"
    );
}

#[skuld::test]
fn a_start_after_the_restart_was_due_drops_the_failure_it_spent() {
    let id = TestId::new(PREFIX);
    let (store, _dir) = store(7000, Ok(RestartDue::Never));
    let mut record = StateRecord::empty();
    record.failure = Some(FailureRecord::new(BOOT, 1000, RestartDue::After(5000)));
    state_file::write(&store.path, &record).expect("write");

    assert_eq!(begin(id.as_str(), &store), Ok(()));

    assert_eq!(record_of(&store), StateRecord::empty());
}

// record_failure ======================================================================================================

#[skuld::test]
fn a_daemon_failure_is_recorded_with_the_restart_the_failure_actions_queue() {
    let id = TestId::new(PREFIX);
    let (store, _dir) = store(5000, Ok(RestartDue::After(1000)));

    record_failure(id.as_str(), &store, Reported::DaemonExit(7));

    assert_eq!(
        record_of(&store).failure,
        Some(FailureRecord::new(BOOT, 5000, RestartDue::After(1000)))
    );
}

#[skuld::test]
fn a_shim_failure_is_recorded_too() {
    let id = TestId::new(PREFIX);
    let (store, _dir) = store(5000, Ok(RestartDue::After(1000)));

    record_failure(id.as_str(), &store, Reported::shim_failure(Some(2)));

    assert!(record_of(&store).failure.is_some());
}

#[skuld::test]
fn reports_that_queue_no_restart_record_nothing() {
    let id = TestId::new(PREFIX);
    for reported in [Reported::Clean, Reported::incomplete_stop()] {
        let (store, _dir) = store(5000, Ok(RestartDue::After(1000)));

        record_failure(id.as_str(), &store, reported);

        assert!(!store.path.exists(), "{reported:?}");
    }
}

#[skuld::test]
fn a_failure_keeps_the_daemon_already_recorded() {
    let id = TestId::new(PREFIX);
    let (store, _dir) = store(5000, Ok(RestartDue::Never));
    state_file::note_launch(&store.path, &this_process()).expect("survivor");

    record_failure(id.as_str(), &store, Reported::DaemonExit(1));

    assert!(record_of(&store).daemon.is_some());
}

#[skuld::test]
fn failure_actions_that_cannot_be_read_record_an_unknown_delay_and_say_so() {
    let id = TestId::new(PREFIX);
    let (store, _dir) = store(5000, Err("access denied".to_string()));

    record_failure(id.as_str(), &store, Reported::DaemonExit(7));

    assert_eq!(
        record_of(&store).failure.expect("a failure").restart,
        RestartDue::Unknown
    );
    let warnings = take_warnings(id.as_str());
    assert_eq!(
        warnings,
        [format!(
            "goetia-shim[{}]: could not read this service's failure actions (access denied): the failure is recorded with an unknown restart delay, so `goetia daemon stop` refuses until the host next boots or the service is uninstalled",
            id.as_str()
        )]
    );
}

#[skuld::test]
fn failure_actions_that_cannot_be_read_fall_back_on_the_restart_install_configured() {
    let id = TestId::new(PREFIX);
    let (store, _dir) = store(5000, Err("access denied".to_string()));
    state_file::configure(&store.path, Actions::new(true, &[Duration::from_secs(30)])).expect("configure");

    record_failure(id.as_str(), &store, Reported::DaemonExit(7));

    assert_eq!(
        record_of(&store).failure.expect("a failure").restart,
        RestartDue::After(30_000)
    );
    assert_eq!(
        take_warnings(id.as_str()),
        [format!(
            "goetia-shim[{}]: could not read this service's failure actions (access denied): the failure is recorded with the restart `goetia daemon install` configured",
            id.as_str()
        )]
    );
}

#[skuld::test]
fn a_failure_that_cannot_be_written_is_logged_with_the_consequence() {
    let id = TestId::new(PREFIX);
    let dir = tempfile::tempdir().expect("temp dir");
    let missing = dir.path().join("no-such-dir").join("svc");
    let store = Store::at(missing.clone(), clock(5000), Ok(RestartDue::After(1000)));

    record_failure(id.as_str(), &store, Reported::DaemonExit(7));

    let lines = take(id.as_str());
    let [line] = lines.as_slice() else {
        panic!("{lines:?}");
    };
    assert!(
        line.starts_with(&format!(
            "goetia-shim[{}]: could not record this failure in {}: ",
            id.as_str(),
            missing.display()
        )),
        "{line}"
    );
    assert!(
        line.ends_with(
            "; `goetia daemon stop` cannot see that SCM has a restart queued. Run `goetia daemon install` again"
        ),
        "{line}"
    );
}

// record_launch =======================================================================================================

#[skuld::test]
fn the_daemon_is_recorded_by_pid_and_creation_time() {
    let id = TestId::new(PREFIX);
    let (store, _dir) = store(5000, Ok(RestartDue::Never));

    assert_eq!(record_launch(id.as_str(), &store, ProcessId::current()), Ok(()));

    assert_eq!(record_of(&store).daemon, Some(this_process()));
}

#[skuld::test]
fn a_launch_replaces_the_daemon_an_earlier_run_left() {
    let id = TestId::new(PREFIX);
    let (store, _dir) = store(5000, Ok(RestartDue::Never));
    let mut gone = this_process();
    gone.token += 1;
    state_file::note_launch(&store.path, &gone).expect("the earlier run");

    assert_eq!(record_launch(id.as_str(), &store, ProcessId::current()), Ok(()));

    assert_eq!(record_of(&store).daemon, Some(this_process()));
}

#[skuld::test]
fn a_launch_that_cannot_be_written_is_logged_with_the_consequence_and_reports_the_os_error() {
    let id = TestId::new(PREFIX);
    let dir = tempfile::tempdir().expect("temp dir");
    let missing = dir.path().join("no-such-dir").join("svc");
    let store = Store::at(missing.clone(), clock(5000), Ok(RestartDue::Never));

    let failed = record_launch(id.as_str(), &store, ProcessId::current());

    assert_eq!(failed, Err(Some(ERROR_PATH_NOT_FOUND)));
    let lines = take(id.as_str());
    let [line] = lines.as_slice() else {
        panic!("{lines:?}");
    };
    assert!(
        line.starts_with(&format!(
            "goetia-shim[{}]: could not record the daemon in {}: ",
            id.as_str(),
            missing.display()
        )),
        "{line}"
    );
    assert!(
        line.ends_with("; the daemon is stopped rather than run where goetia cannot see it, and the service fails. If this service was registered by an older goetia, run `goetia daemon install` again"),
        "{line}"
    );
}

// forget_launch =======================================================================================================

#[skuld::test]
fn a_daemon_that_has_ended_is_forgotten() {
    let id = TestId::new(PREFIX);
    let (store, _dir) = store(5000, Ok(RestartDue::Never));
    state_file::note_launch(&store.path, &this_process()).expect("launch");

    forget_launch(id.as_str(), &store, ProcessId::current());

    assert_eq!(record_of(&store).daemon, None);
}

#[skuld::test]
fn a_daemon_that_cannot_be_forgotten_is_logged_as_a_warning_that_says_what_goetia_will_read() {
    let id = TestId::new(PREFIX);
    let dir = tempfile::tempdir().expect("temp dir");
    let missing = dir.path().join("no-such-dir").join("svc");
    let store = Store::at(missing.clone(), clock(5000), Ok(RestartDue::Never));

    forget_launch(id.as_str(), &store, ProcessId::current());

    let warnings = take_warnings(id.as_str());
    let [line] = warnings.as_slice() else {
        panic!("{warnings:?}");
    };
    assert!(
        line.starts_with(&format!(
            "goetia-shim[{}]: could not clear the daemon from {}: ",
            id.as_str(),
            missing.display()
        )),
        "{line}"
    );
    assert!(
        line.ends_with("; it names a process that has ended, which goetia reads as gone"),
        "{line}"
    );
}
