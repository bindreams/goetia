//! The shim's state (`%ProgramData%\Goetia\state\<dir_name(id)>\record`): what install provisions,
//! what the shim writes as the service account, and what `stop` and `start` read from it. Every
//! wait is for a positive event: a daemon connecting back, a process exiting, SCM reaching a state.

use std::collections::BTreeSet;
use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use cosca::identity::{ProcessId, Resolved};
use goetia::backend::scm::exit_status::{ERROR_SERVICE_ALREADY_RUNNING, RecordedExit};
use goetia::backend::scm::manager::ScmManager;
use goetia::backend::scm::state_file::{self, Actions, Loaded, RestartDue, StateRecord, host};
use goetia::decide::Outcome;
use goetia::manager::{Budget, ServiceManager as _};
use goetia::spec::{DaemonSpec, Restart, User};
use windows_service::service::{ServiceAccess, ServiceState};
use windows_service::service_manager::{ServiceManager, ServiceManagerAccess};

use crate::common::{
    ShimProcess, fallback_log, fixture_command, grant_everyone_read_and_execute, kill_process, recorded_exit,
    scm_status,
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

/// The trustees of the ACEs in the ACL of `path`, as SDDL names them: an alias such as `SY`, `BA`,
/// `LS` (LocalService) or `NS` (NetworkService), or a SID string. Read back through `icacls /save`,
/// which gives the descriptor and not a localized rendering of it.
fn trustees(path: &Path) -> BTreeSet<String> {
    let scratch = tempfile::tempdir().expect("a directory for the saved ACL");
    let saved = scratch.path().join("acl.txt");
    support::cmd::run(
        "icacls.exe",
        &[&path.display().to_string(), "/save", &saved.display().to_string()],
    )
    .expect_ok();
    let bytes = std::fs::read(&saved).expect("read the saved ACL");
    // `icacls` writes UTF-16 with a byte order mark.
    let units: Vec<u16> = bytes
        .chunks_exact(2)
        .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
        .collect();
    let text = String::from_utf16_lossy(&units);
    let sddl = text
        .lines()
        .map(|line| line.trim_start_matches('\u{feff}'))
        .find(|line| line.starts_with("D:"))
        .unwrap_or_else(|| panic!("no DACL in the saved ACL of {}:\n{text}", path.display()));
    sddl.split('(')
        .skip(1)
        .map(|ace| {
            let ace = ace.trim_end_matches(')');
            ace.split(';')
                .nth(5)
                .unwrap_or_else(|| panic!("an ACE without a trustee: {ace:?} in {sddl}"))
                .to_string()
        })
        .collect()
}

fn names(trustees: &[&str]) -> BTreeSet<String> {
    trustees.iter().map(|name| (*name).to_string()).collect()
}

// Provisioning ========================================================================================================

#[skuld::test(requires = [support::elevated], labels = [ELEVATED])]
fn install_creates_the_state_file_and_a_reinstall_keeps_what_the_shim_wrote() {
    let mgr = ScmManager::new();
    let id = support::random_test_id();
    let _guard = ServiceGuard::new(&id);
    let spec = on_failure(&id, fixture_command("report", &["1"]), Some(HOUR));

    mgr.install(&spec, false).expect("install");
    assert_eq!(
        record(&id),
        StateRecord {
            configured: Some(Actions::new(true, &[HOUR])),
            ..StateRecord::empty()
        },
        "a new file holds the failure actions the service was registered with, for a shim that cannot read its own"
    );

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
fn uninstall_removes_the_service_directory() {
    let mgr = ScmManager::new();
    let id = support::random_test_id();
    let _guard = ServiceGuard::new(&id);
    let spec = on_failure(&id, fixture_command("report", &["1"]), Some(HOUR));
    mgr.install(&spec, false).expect("install");
    assert!(host::path(&id).exists(), "precondition: install made the file");

    mgr.uninstall(&spec.id).expect("uninstall");

    assert!(!host::service_dir(&id).exists());
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
    // The boot identity is the kernel's boot counter, which any account can read: the one this
    // shim recorded is the one an administrator reads now, so the record is not mistaken for an
    // earlier boot's.
    assert_eq!(
        failure.boot,
        host::clock().boot,
        "a LocalService shim must record the boot it runs on: {failure:?}"
    );
}

/// After the shim has replaced its record (which it does at every launch), the file still grants
/// only the account its service runs as: a rename keeps the descriptor of the file it moves, which
/// is the one the directory gave the shim's temporary. A service under another account has a
/// directory of its own, and no service account can reach the state directory itself.
#[skuld::test(requires = [support::elevated], labels = [ELEVATED])]
fn a_services_record_stays_closed_to_other_service_accounts_after_the_shim_replaced_it() {
    let mgr = ScmManager::new();
    let id = support::random_test_id();
    let _guard = ServiceGuard::new(&id);
    let other = support::random_test_id();
    let _other_guard = ServiceGuard::new(&other);
    grant_everyone_read_and_execute(env!("CARGO_BIN_EXE_goetia-shim"));
    grant_everyone_read_and_execute(&support::current_exe_str());
    let neighbour = DaemonSpec {
        user: User::Name("NetworkService".to_string()),
        ..on_failure(&other, fixture_command("report", &["1"]), Some(HOUR))
    };
    mgr.install(&neighbour, false).expect("install the other service");

    let (_reported, _spec) = fail_once_as(&mgr, &id, HOUR, User::Name("LocalService".to_string()));

    assert!(
        record(&id).failure.is_some(),
        "precondition: the shim wrote through its ACL"
    );
    for path in [host::path(&id), host::service_dir(&id)] {
        let granted = trustees(&path);
        assert!(granted.contains("LS"), "{}: {granted:?}", path.display());
        assert!(!granted.contains("NS"), "{}: {granted:?}", path.display());
        assert!(
            granted.is_subset(&names(&["SY", "BA", "LS"])),
            "{}: {granted:?}",
            path.display()
        );
    }
    let neighbours = trustees(&host::path(&other));
    assert!(
        neighbours.contains("NS") && !neighbours.contains("LS"),
        "{neighbours:?}"
    );
    assert_eq!(
        trustees(&host::dir()),
        names(&["SY", "BA"]),
        "no service account may create or read anything directly under the state directory"
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

// A restart SCM queued for a process that did not report ==============================================================

/// `goetia daemon kill` ends the shim as a crash, which SCM recovers from; it records the failure
/// first, as the shim records its own, so a `stop` in the delay is refused.
#[skuld::test(requires = [support::elevated], labels = [ELEVATED])]
fn a_stop_after_goetia_kill_is_refused_in_the_restart_delay() {
    let mgr = ScmManager::new();
    let id = support::random_test_id();
    let _guard = ServiceGuard::new(&id);
    let started = ConnectBack::listen();
    let spec = on_failure(
        &id,
        fixture_command("report", &[&started.port().to_string()]),
        Some(HOUR),
    );
    mgr.install(&spec, false).expect("install");
    mgr.start(&spec.id, Budget::DEFAULT).expect("start");
    started.accept("the daemon to report in");

    mgr.kill(&spec.id).expect("kill");

    let failure = record(&id).failure.expect("kill recorded the failure it causes");
    assert_eq!(failure.restart, RestartDue::After(3_600_000), "{failure:?}");
    let refused = mgr
        .stop(&spec.id, Budget::DEFAULT)
        .expect_err("a stop cannot cancel the queued restart");
    assert!(matches!(refused, goetia::Error::RestartPending { .. }), "{refused:?}");
}

/// A shim ended from outside reports nothing and records nothing. SCM sees a crash (1067), which
/// the shim never reports itself, so `stop` falls back on the status rule.
#[skuld::test(requires = [support::elevated], labels = [ELEVATED])]
fn a_stop_after_the_shim_was_ended_from_outside_is_refused_in_the_restart_delay() {
    let mgr = ScmManager::new();
    let id = support::random_test_id();
    let _guard = ServiceGuard::new(&id);
    let started = ConnectBack::listen();
    let spec = on_failure(
        &id,
        fixture_command("report", &[&started.port().to_string()]),
        Some(HOUR),
    );
    mgr.install(&spec, false).expect("install");
    mgr.start(&spec.id, Budget::DEFAULT).expect("start");
    started.accept("the daemon to report in");
    let shim = ShimProcess::attach(&id);

    kill_process(shim.pid());
    support::kill::until(|| scm_status(&id).current_state == ServiceState::Stopped);

    assert_eq!(record(&id).failure, None, "an ended shim wrote nothing");
    let refused = mgr
        .stop(&spec.id, Budget::DEFAULT)
        .expect_err("a stop cannot cancel the queued restart");
    assert!(matches!(refused, goetia::Error::RestartPending { .. }), "{refused:?}");
}

// A survivor ==========================================================================================================

/// A running process named as `id`'s survivor in its state file: all the shim and the CLI ever read
/// of a daemon an incomplete stop could not kill.
fn plant_survivor(id: &str) -> std::process::Child {
    let planted = ConnectBack::listen();
    let survivor = std::process::Command::new(support::current_exe_str())
        .args(["--goetia-shim-fixture", "report", &planted.port().to_string()])
        .stdin(Stdio::null())
        .spawn()
        .expect("spawn the process to plant as the survivor");
    planted.accept("the planted survivor to be running");
    let Resolved::Found(identity) = ProcessId::of(survivor.id()) else {
        panic!("the planted survivor has no identity");
    };
    state_file::note_launch(&host::path(id), &identity.to_record().expect("record")).expect("plant the survivor");
    survivor
}

/// A daemon the shim could not kill cannot be produced end to end (both kills are refused only by
/// a unit-test hook), so the survivor is planted: a running process named as the launched daemon in
/// the state file, which is all the shim and the CLI ever read.
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

    let mut survivor = plant_survivor(&id);

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
    let launched = record(&id).daemon.expect("the shim recorded the daemon it launched");
    assert_ne!(
        launched.pid,
        survivor.id(),
        "the new daemon replaced the old one's entry"
    );

    mgr.stop(&spec.id, Budget::DEFAULT).expect("stop");
    assert_eq!(record(&id).daemon, None, "a daemon that ended is forgotten");
}

/// The record of a surviving daemon outlives the service it was written for, so registering the
/// id again and starting it cannot run a second daemon beside the first.
#[skuld::test(requires = [support::elevated], labels = [ELEVATED])]
fn uninstall_keeps_a_live_survivor_so_a_reinstalled_service_cannot_start_over_it() {
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
    let mut survivor = plant_survivor(&id);
    let failure = state_file::FailureRecord::at(host::clock(), RestartDue::After(5));
    state_file::note_failure(&host::path(&id), failure).expect("record a failure as the shim would");

    // The service is deleted; the error says the daemon may still run.
    let told = mgr
        .uninstall(&spec.id)
        .expect_err("uninstall reports a daemon it could not stop");
    assert!(told.to_string().contains("deleted"), "{told}");
    let kept = record(&id);
    assert!(kept.daemon.is_some(), "the survivor must outlive the service");
    assert_eq!(kept.failure, None, "the pending restart went with the service");

    mgr.install(&spec, false).expect("install again");
    let refused = mgr
        .start(&spec.id, Budget::DEFAULT)
        .expect_err("a start over a running survivor");
    assert!(refused.to_string().contains(&survivor.id().to_string()), "{refused}");
    assert!(!reported.connected_yet(), "no second daemon may have started");

    survivor.kill().expect("kill the planted survivor");
    survivor.wait().expect("reap the planted survivor");
    mgr.start(&spec.id, Budget::DEFAULT)
        .expect("start once the survivor is gone");
    reported.accept("the daemon to run");
    mgr.stop(&spec.id, Budget::DEFAULT).expect("stop");
}

// What kill and a lost delay record ===================================================================================

/// A process that ends without reporting is a crash, and SCM runs recovery for it whatever the
/// failure-actions flag says; `kill` records the restart it causes the same way.
#[skuld::test(requires = [support::elevated], labels = [ELEVATED])]
fn goetia_kill_records_the_restart_a_crash_queues_whatever_the_failure_flag_says() {
    let mgr = ScmManager::new();
    let id = support::random_test_id();
    let _guard = ServiceGuard::new(&id);
    let started = ConnectBack::listen();
    let spec = on_failure(
        &id,
        fixture_command("report", &[&started.port().to_string()]),
        Some(HOUR),
    );
    mgr.install(&spec, false).expect("install");
    mgr.start(&spec.id, Budget::DEFAULT).expect("start");
    started.accept("the daemon to report in");
    support::cmd::run("sc.exe", &["failureflag", &id, "0"]).expect_ok();

    mgr.kill(&spec.id).expect("kill");

    let failure = record(&id).failure.expect("kill recorded the failure it causes");
    assert_eq!(failure.restart, RestartDue::After(3_600_000), "{failure:?}");
}

/// A failure whose delay nobody knows is not spent by a start: a start by hand does not cancel what
/// SCM queued, so the stop that follows is refused, until the host boots again.
#[skuld::test(requires = [support::elevated], labels = [ELEVATED])]
fn a_failure_of_unknown_delay_survives_a_start_and_keeps_a_stop_refused() {
    let mgr = ScmManager::new();
    let id = support::random_test_id();
    let _guard = ServiceGuard::new(&id);
    let started = ConnectBack::listen();
    let mut spec = on_failure(
        &id,
        fixture_command("report", &[&started.port().to_string()]),
        Some(HOUR),
    );
    spec.restart = Restart::Never;
    mgr.install(&spec, false).expect("install");
    state_file::note_failure(
        &host::path(&id),
        state_file::FailureRecord::at(host::clock(), RestartDue::Unknown),
    )
    .expect("record a failure of unknown delay as the shim would");

    mgr.start(&spec.id, Budget::DEFAULT).expect("start");
    started.accept("the daemon to run");

    assert!(record(&id).failure.is_some(), "the start must not spend the failure");
    let refused = mgr
        .stop(&spec.id, Budget::DEFAULT)
        .expect_err("a stop cannot cancel what may be queued");
    assert!(matches!(refused, goetia::Error::RestartPending { .. }), "{refused:?}");
}

// A daemon that cannot be recorded ====================================================================================

/// A service registered by an older goetia has no directory, and the shim cannot make one as the
/// account it runs as. It does not run the daemon where goetia cannot see it: the service fails,
/// and the log says what to do.
#[skuld::test(requires = [support::elevated], labels = [ELEVATED])]
fn a_service_with_no_state_directory_fails_to_start_rather_than_run_a_daemon_unrecorded() {
    let mgr = ScmManager::new();
    let id = support::random_test_id();
    let _guard = ServiceGuard::new(&id);
    let started = ConnectBack::listen();
    let mut spec = on_failure(&id, fixture_command("report", &[&started.port().to_string()]), None);
    spec.restart = Restart::Never;
    mgr.install(&spec, false).expect("install");
    std::fs::remove_dir_all(host::service_dir(&id)).expect("remove the service's directory");

    mgr.start(&spec.id, Budget::DEFAULT)
        .expect_err("a daemon that cannot be recorded must not be reported running");

    // ERROR_PATH_NOT_FOUND: the directory the record would be in.
    assert_eq!(recorded_exit(&id), RecordedExit { win32: 3, specific: 0 });
    let logged = fallback_log(&id);
    assert!(logged.contains("could not record the daemon"), "{logged}");
    assert!(logged.contains("goetia daemon install"), "{logged}");
}
