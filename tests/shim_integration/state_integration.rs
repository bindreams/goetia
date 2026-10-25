//! The shim's state file (`%ProgramData%\Goetia\state\<id>`): what install provisions, what the
//! shim writes as the service account, and what `stop` and `start` read from it. Every wait is for
//! a positive event: a daemon connecting back, a process exiting, SCM reaching a state.

use std::process::Stdio;
use std::time::Duration;

use cosca::identity::{ProcessId, Resolved};
use goetia::backend::scm::exit_status::{ERROR_SERVICE_ALREADY_RUNNING, RecordedExit};
use goetia::backend::scm::manager::ScmManager;
use goetia::backend::scm::state_file::{self, Loaded, RestartDue, StateRecord, host};
use goetia::decide::Outcome;
use goetia::manager::{Budget, ServiceManager as _};
use goetia::spec::{Restart, User};
use windows_service::service::{ServiceAccess, ServiceState};
use windows_service::service_manager::{ServiceManager, ServiceManagerAccess};

use crate::common::{
    ShimProcess, fallback_log, fixture_command, grant_everyone_read_and_execute, recorded_exit, scm_status,
};
use crate::restart_integration::{fail_once, fail_once_as, on_failure};
use crate::support::{self, ConnectBack, ELEVATED, ServiceGuard};

/// A delay that cannot elapse during a test, so the record read is the first failure's.
const HOUR: Duration = Duration::from_secs(3600);

fn loaded(id: &str) -> Loaded {
    state_file::read(&host::path(id)).expect("read the state file")
}

fn record(id: &str) -> StateRecord {
    match loaded(id) {
        Loaded::Record(record) => record,
        other => panic!("{id}: expected a state record, found {other:?}"),
    }
}

// Provisioning ========================================================================================================

#[skuld::test(requires = [support::elevated], labels = [ELEVATED])]
fn install_creates_the_state_file_and_a_reinstall_keeps_what_the_shim_wrote() {
    let mgr = ScmManager::new();
    let id = support::random_test_id();
    let _guard = ServiceGuard::new(&id);
    let spec = on_failure(&id, fixture_command("report", &["1"]), Some(HOUR));

    mgr.install(&spec, false).expect("install");
    assert_eq!(record(&id), StateRecord::empty(), "a new file holds the empty record");

    let failure = state_file::FailureRecord::at(host::clock(), RestartDue::After(5));
    state_file::note_failure(&host::path(&id), failure.clone()).expect("record a failure as the shim would");
    assert_eq!(mgr.install(&spec, false).expect("install again"), Outcome::UpToDate);

    assert_eq!(
        record(&id).failure,
        Some(failure),
        "a re-install must not forget a pending restart"
    );
}

#[skuld::test(requires = [support::elevated], labels = [ELEVATED])]
fn uninstall_removes_the_state_file() {
    let mgr = ScmManager::new();
    let id = support::random_test_id();
    let _guard = ServiceGuard::new(&id);
    let spec = on_failure(&id, fixture_command("report", &["1"]), Some(HOUR));
    mgr.install(&spec, false).expect("install");
    assert!(host::path(&id).exists(), "precondition: install made the file");

    mgr.uninstall(&spec.id).expect("uninstall");

    assert!(!host::path(&id).exists());
}

// What the shim writes ================================================================================================

/// The premise of the whole design: the shim runs as the service's account, reads its own failure
/// actions with the access that account has, and writes the file through the ACL install gave it.
/// `LocalService` is the narrowest built-in account a daemon runs as.
#[skuld::test(requires = [support::elevated], labels = [ELEVATED])]
fn a_local_service_shim_writes_its_failure_record() {
    let mgr = ScmManager::new();
    let id = support::random_test_id();
    let _guard = ServiceGuard::new(&id);
    // `LocalService` cannot be assumed to reach the repository's build tree; see `WorldReadableExe`.
    grant_everyone_read_and_execute(env!("CARGO_BIN_EXE_goetia-shim"));
    grant_everyone_read_and_execute(&support::current_exe_str());

    let (_reported, _spec) = fail_once_as(&mgr, &id, HOUR, User::Name("LocalService".to_string()));

    let failure = record(&id).failure.expect("the shim recorded its failure");
    assert_eq!(
        failure.restart,
        RestartDue::After(3_600_000),
        "the shim must read the restart action it runs under as LocalService: {failure:?}"
    );
}

#[skuld::test(requires = [support::elevated], labels = [ELEVATED])]
fn a_system_shim_records_the_failure_with_the_delay_of_the_restart_it_queued() {
    let mgr = ScmManager::new();
    let id = support::random_test_id();
    let _guard = ServiceGuard::new(&id);

    let (_reported, _spec) = fail_once(&mgr, &id, HOUR);

    let failure = record(&id).failure.expect("the shim recorded its failure");
    assert_eq!(failure.restart, RestartDue::After(3_600_000), "{failure:?}");
}

#[skuld::test(requires = [support::elevated], labels = [ELEVATED])]
fn a_restart_never_failure_is_recorded_as_queuing_nothing() {
    let mgr = ScmManager::new();
    let id = support::random_test_id();
    let _guard = ServiceGuard::new(&id);
    let reported = ConnectBack::listen();
    let mut spec = on_failure(
        &id,
        fixture_command("exit-on-close", &[&reported.port().to_string(), "7"]),
        None,
    );
    spec.restart = Restart::Never;
    mgr.install(&spec, false).expect("install");
    mgr.start(&spec.id, Budget::DEFAULT).expect("start");
    let stream = reported.accept_stream("the daemon to wait for its release");
    let shim = ShimProcess::attach(&id);
    drop(stream);
    shim.wait_for_exit();

    assert_eq!(record(&id).failure.expect("a failure").restart, RestartDue::Never);
    // So a stop finds nothing pending, though the service is stopped with a failure.
    mgr.stop(&spec.id, Budget::DEFAULT).expect("stop");
}

// What stop reads =====================================================================================================

/// The restart fires, the new run starts at or after its due time and so spends the record, and a
/// stop right after it is not refused: without that, every stop in the margin after a restart would
/// be.
#[skuld::test(requires = [support::elevated], labels = [ELEVATED])]
fn a_stop_after_the_restart_has_fired_is_not_refused() {
    let mgr = ScmManager::new();
    let id = support::random_test_id();
    let _guard = ServiceGuard::new(&id);
    let (reported, spec) = fail_once(&mgr, &id, Duration::from_secs(1));

    let _second_run = reported.accept_stream("the restarted run to connect");

    mgr.stop(&spec.id, Budget::DEFAULT).expect("stop");
    assert_eq!(
        recorded_exit(&id),
        RecordedExit { win32: 0, specific: 0 },
        "the commanded stop is what SCM holds last"
    );
}

/// A missing file is no record: the service looks as one registered before the file existed.
#[skuld::test(requires = [support::elevated], labels = [ELEVATED])]
fn a_service_with_no_state_file_has_no_restart_pending() {
    let mgr = ScmManager::new();
    let id = support::random_test_id();
    let _guard = ServiceGuard::new(&id);
    let (_reported, spec) = fail_once(&mgr, &id, HOUR);
    std::fs::remove_file(host::path(&id)).expect("remove the state file");

    mgr.stop(&spec.id, Budget::DEFAULT)
        .expect("a stop with no record is not refused");
}

// A survivor ==========================================================================================================

/// A daemon the shim could not kill cannot be produced end to end (both kills are refused only by
/// a unit-test hook), so the survivor is planted: a running process named in the state file, which
/// is all the shim and the CLI ever read.
#[skuld::test(requires = [support::elevated], labels = [ELEVATED])]
fn a_start_is_refused_while_the_survivor_runs_and_allowed_when_it_is_gone() {
    let mgr = ScmManager::new();
    let id = support::random_test_id();
    let _guard = ServiceGuard::new(&id);
    let reported = ConnectBack::listen();
    let mut spec = on_failure(
        &id,
        fixture_command("report", &[&reported.port().to_string()]),
        Some(HOUR),
    );
    spec.restart = Restart::Never;
    mgr.install(&spec, false).expect("install");

    let planted = ConnectBack::listen();
    let mut survivor = std::process::Command::new(support::current_exe_str())
        .args(["--goetia-shim-fixture", "report", &planted.port().to_string()])
        .stdin(Stdio::null())
        .spawn()
        .expect("spawn the process to plant as the survivor");
    planted.accept("the planted survivor to be running");
    let Resolved::Found(identity) = ProcessId::of(survivor.id()) else {
        panic!("the planted survivor has no identity");
    };
    state_file::note_survivor(&host::path(&id), &identity.to_record().expect("record")).expect("plant the survivor");

    // goetia's own start refuses before it asks SCM to start anything.
    let refused = mgr
        .start(&spec.id, Budget::DEFAULT)
        .expect_err("a start over a running survivor");
    assert!(refused.to_string().contains(&survivor.id().to_string()), "{refused}");
    assert_eq!(scm_status(&id).current_state, ServiceState::Stopped);

    // `sc start` does not go through goetia: the shim itself refuses to launch a second daemon.
    let scm = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT).expect("connect");
    let service = scm
        .open_service(&id, ServiceAccess::START | ServiceAccess::QUERY_STATUS)
        .expect("open the service to start it");
    service.start::<&std::ffi::OsStr>(&[]).expect("StartService");
    // `StartService` returns with the service `START_PENDING`, so this waits for the shim's report.
    support::kill::until(|| scm_status(&id).current_state == ServiceState::Stopped);
    assert_eq!(
        recorded_exit(&id),
        RecordedExit {
            win32: ERROR_SERVICE_ALREADY_RUNNING,
            specific: 0
        }
    );
    let logged = fallback_log(&id);
    assert!(
        logged.contains(&format!("not starting the daemon: process {}", survivor.id())),
        "the shim did not say why it refused:\n{logged}"
    );
    assert!(
        !reported.connected_yet(),
        "the refused start must not have run a daemon"
    );

    // Once the survivor is gone the next start goes through, and the shim forgets it.
    survivor.kill().expect("kill the planted survivor");
    survivor.wait().expect("reap the planted survivor");
    mgr.start(&spec.id, Budget::DEFAULT)
        .expect("start once the survivor is gone");
    reported.accept("the daemon to run");
    assert_eq!(record(&id), StateRecord::empty());

    mgr.stop(&spec.id, Budget::DEFAULT).expect("stop");
}
