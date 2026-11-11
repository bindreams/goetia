//! `type: managed` integration tests against a real SCM. See the module doc
//! comment on `goetia::backend::scm::manager` for the five traps these
//! cover, and `fixture.rs` for the daemon process every spec here runs.

use std::collections::BTreeMap;
use std::process::Command;
use std::time::Duration;

use goetia::backend::scm::manager::Scm;
use goetia::decide::Outcome;
use goetia::manager::conformance;
use goetia::manager::{Budget, Installed, ServiceManager as _, State};
use goetia::spec::{AccountId, DaemonFields, DaemonSpec, Id, Kind, Restart, User};
use windows_service::service::ServiceAccess;
use windows_service::service_manager::{ServiceManager as WinServiceManager, ServiceManagerAccess};
use windows_sys::Win32::Foundation::ERROR_ACCESS_DENIED;
use winreg::enums::{HKEY_LOCAL_MACHINE, KEY_READ, KEY_WRITE, RegType};

use crate::common::{self, conformance_mk, fixture_command};
use crate::deny::Denied;
use crate::install_helper::INSTALL_AS;
use crate::support::raw_registry::{create_link, create_raw_key, set_raw_value};
use crate::support::registry::{
    create_parameters_key, parameters_key, service_key, try_parameters_key, try_service_key,
};
use crate::support::{self, ConnectBack, ELEVATED, ScratchKey, ServiceGuard};

/// Held by every test that denies an elevated reader something under
/// `HKLM\SYSTEM\CurrentControlSet\Services` (the `Parameters` key or a service object), by every
/// test whose assertion reads the aggregate `list` entry (`support::may_account_for`), by
/// `tests/cli_binary.rs`'s `list` test, and by the tests here and in `tests/shim_integration` that
/// install, rewrite or uninstall a service and read it back through `list`, so they take turns
/// rather than race. The hazards:
///
/// - Two concurrent unreadable services make `list` report one nameless `Undetermined` entry
///   instead of a named one, so a deny test's lookup of its own service by name fails.
/// - A negative assertion that counts the aggregate fails while any service is denied.
/// - A service-object denial makes a concurrent `list` report an error for that service, which
///   `cli_binary`'s `list_json` refuses.
///
/// Shared with `tests/shim_integration` and `tests/cli_binary.rs` by label *name*: skuld serializes
/// across processes by it, and all three drive the one SCM on the host.
#[skuld::label]
const SCM_DENIAL: skuld::Label;

/// Held by every test here that takes the machine-wide install lock (a `type: simple` install),
/// which the lock tests in `src/backend/scm/shim/lock_tests.rs` and `tests/shim_integration` poll
/// for who holds it. Shared with both by label *name*.
#[skuld::label]
const UNIT_DIR_EXCLUSIVE: skuld::Label;

fn id_of(s: &str) -> Id {
    Id::try_from(s).expect("random_test_id/short local ids are valid Ids")
}

/// A fresh, installed `type: managed` service at `id`, with no ports wired up.
fn install_plain(mgr: &Scm, id: &str) -> DaemonSpec {
    let spec = DaemonSpec::new(support::plain_fields(id, fixture_command(id, 1, 1, "plain"))).unwrap();
    mgr.install(&spec, false).expect("install");
    spec
}

fn seed_foreign(id: &str) {
    support::cmd::run(
        "sc.exe",
        &[
            "create",
            id,
            "binPath=",
            &support::current_exe_str(),
            "start=",
            "demand",
        ],
    )
    .expect_ok();
}

/// A hand-edit `sc config` can make without touching `Parameters`: append an
/// argument to the command line. Changes the registration's `Arguments` line,
/// which the embedded blob cannot explain — the Windows analogue of a
/// unit-file `MemoryMax=8G` a spec-level diff would render invisible.
fn hand_edit(id: &str) {
    let mutated = format!("\"{}\" --hand-edited", support::current_exe_str());
    support::cmd::run("sc.exe", &["config", id, "binPath=", &mutated]).expect_ok();
}

// Step 1: conformance =================================================================================================

/// `SCM_DENIAL`: seeding `UNDETERMINED_ID` denies an elevated reader one service's
/// `Parameters` for the length of the run, which puts an `Undetermined` entry naming that id in
/// every concurrent `list()` (a nameless aggregate if another service is unreadable too).
#[skuld::test(requires = [support::elevated], labels = [ELEVATED, SCM_DENIAL], serial = SCM_DENIAL)]
fn scm_passes_conformance() {
    let _foreign_guard = ServiceGuard::new(conformance::FOREIGN_ID);
    seed_foreign(conformance::FOREIGN_ID);

    let mgr = Scm::new();
    let _hand_edited_guard = ServiceGuard::new(conformance::HAND_EDITED_ID);
    mgr.install(&conformance_mk(conformance::HAND_EDITED_ID), false)
        .expect("seed install for the hand-edit scenario");
    hand_edit(conformance::HAND_EDITED_ID);

    // Seed `UNDETERMINED_ID`: install normally, then deny reading the one key that carries the
    // marker. An explicit `Deny` ACE stops the elevated reader too (see `deny.rs`), so the read
    // that would classify this id never completes. The deny is declared last so it is lifted
    // before `sc delete` runs; cleanup is ours, not `run`'s.
    let _undetermined_guard = ServiceGuard::new(conformance::UNDETERMINED_ID);
    mgr.install(&conformance_mk(conformance::UNDETERMINED_ID), false)
        .expect("seed install for the undetermined scenario");
    let _undetermined_denied = Denied::parameters(conformance::UNDETERMINED_ID);

    conformance::run(&mgr, &conformance_mk);
}

// Step 2: round trip, conflict, start/stop/status, uninstall, list ====================================================

#[skuld::test(requires = [support::elevated], labels = [ELEVATED, SCM_DENIAL], serial = SCM_DENIAL)]
fn install_then_list_round_trips_without_the_source_file() {
    let mgr = Scm::new();
    let id = support::random_test_id();
    let _guard = ServiceGuard::new(&id);

    let mut env = BTreeMap::new();
    env.insert("GOETIA_TEST_KEY".to_string(), "value".to_string());
    let spec = DaemonSpec::new(DaemonFields {
        env,
        ..support::plain_fields(&id, fixture_command(&id, 1, 1, "plain"))
    })
    .unwrap();

    let outcome = mgr.install(&spec, false).expect("install");
    assert!(matches!(outcome, Outcome::Create), "{outcome:?}");

    // There is no manifest file for SCM to begin with — the whole spec's
    // survival depends on the `Parameters` blob in the registry.
    let installed = mgr.list().expect("list");
    let found = installed
        .into_iter()
        .find_map(|entry| match entry {
            Installed::Ours { spec: s, .. } if s.id.as_str() == id => Some(s),
            _ => None,
        })
        .unwrap_or_else(|| panic!("{id} did not appear in list"));
    assert_eq!(found, spec, "round-tripped spec must equal the original exactly");

    let status = mgr.status(&spec.id).expect("status");
    assert_eq!(status.state, State::Stopped);
    assert!(!status.enabled);
}

/// SCM matches service names case-insensitively, so a request for `ID` reaches the service
/// installed as `id`. Neither `install` (forced or not) nor `preview_install` may write over it.
#[skuld::test(requires = [support::elevated], labels = [ELEVATED, SCM_DENIAL], serial = SCM_DENIAL)]
fn a_differently_cased_id_is_refused_and_leaves_the_service_unchanged() {
    let mgr = Scm::new();
    let id = support::random_test_id();
    let upper = id.to_uppercase();
    let _guard = ServiceGuard::new(&id);
    let _upper_guard = ServiceGuard::new(&upper);
    let installed = install_plain(&mgr, &id);

    let requested = DaemonSpec::new(DaemonFields {
        id: id_of(&upper),
        ..installed.clone().into_fields()
    })
    .unwrap();
    let expected = Outcome::RefuseOtherDaemon { installed: id_of(&id) };

    assert_eq!(mgr.preview_install(&requested).expect("preview_install"), expected);
    for force in [false, true] {
        assert_eq!(
            mgr.install(&requested, force).expect("install"),
            expected,
            "force={force}"
        );
    }

    let after = mgr
        .list()
        .expect("list")
        .into_iter()
        .find_map(|entry| match entry {
            Installed::Ours { spec, .. } if spec.id.as_str().eq_ignore_ascii_case(&id) => Some(spec),
            _ => None,
        })
        .unwrap_or_else(|| panic!("{id} did not appear in list"));
    assert_eq!(after, installed, "the installed service must not be written over");
}

/// `apply` restates the start type the service has when it updates one, because `change_config`
/// always sends a real `dwStartType`. An update that restated the create default instead would undo
/// `enable` here, silently. `enable` rather than `disable` is the one to pin: a fresh service is
/// already demand-start, so a service reset to the default would still read as disabled.
#[skuld::test(requires = [support::elevated], labels = [ELEVATED])]
fn an_update_keeps_the_start_type_enable_set() {
    let mgr = Scm::new();
    let id = support::random_test_id();
    let _guard = ServiceGuard::new(&id);
    let command = fixture_command(&id, 1, 1, "plain");
    let spec = DaemonSpec::new(support::plain_fields(&id, command.clone())).unwrap();
    mgr.install(&spec, false).expect("install");
    mgr.enable(&spec.id).expect("enable");
    assert!(mgr.status(&spec.id).expect("status after enable").enabled);

    let changed = DaemonSpec::new(DaemonFields {
        env: BTreeMap::from([("GOETIA_TEST_KEY".to_string(), "value".to_string())]),
        ..support::plain_fields(&id, command)
    })
    .unwrap();
    let outcome = mgr.install(&changed, false).expect("install the changed spec");
    assert!(matches!(outcome, Outcome::Update { .. }), "{outcome:?}");

    assert!(
        mgr.status(&spec.id).expect("status after the update").enabled,
        "the update must not reset the start type `enable` set"
    );
}

#[skuld::test(requires = [support::elevated], labels = [ELEVATED])]
fn sc_config_binpath_change_is_detected_as_conflict() {
    let mgr = Scm::new();
    let id = support::random_test_id();
    let _guard = ServiceGuard::new(&id);
    let spec = DaemonSpec::new(support::plain_fields(&id, fixture_command(&id, 1, 1, "plain"))).unwrap();
    mgr.install(&spec, false).expect("install");

    hand_edit(&id);

    let outcome = mgr
        .install(&spec, false)
        .expect("install over a hand-edited artifact must not error");
    match outcome {
        Outcome::Conflict { artifact_diff, .. } => assert!(!artifact_diff.is_empty(), "conflict must carry a diff"),
        other => panic!("expected Conflict, got {other:?}"),
    }

    let forced = mgr.install(&spec, true).expect("forced install");
    assert!(
        !matches!(forced, Outcome::Conflict { .. }),
        "force must resolve it: {forced:?}"
    );
}

#[skuld::test(requires = [support::elevated], labels = [ELEVATED])]
fn start_stop_status_reflect_reality() {
    let mgr = Scm::new();
    let id = support::random_test_id();
    let _guard = ServiceGuard::new(&id);
    let started = ConnectBack::listen();
    let stopped = ConnectBack::listen();
    let spec = DaemonSpec::new(support::plain_fields(
        &id,
        fixture_command(&id, started.port(), stopped.port(), "plain"),
    ))
    .unwrap();
    mgr.install(&spec, false).expect("install");

    mgr.start(&spec.id, Budget::DEFAULT).expect("start");
    started.accept("the fixture to report SERVICE_RUNNING");
    let status = mgr.status(&spec.id).expect("status while running");
    assert_eq!(status.state, State::Running);
    assert!(
        status.pid.is_some(),
        "type: managed reports the daemon's own pid, not a shim's"
    );

    mgr.stop(&spec.id, Budget::DEFAULT).expect("stop");
    stopped.accept("the fixture to handle SERVICE_CONTROL_STOP");
    let status = mgr.status(&spec.id).expect("status after stop");
    assert_eq!(status.state, State::Stopped);
}

// kill ================================================================================================================

/// The fixture as a daemon that reports in on every start, under `restart`. `on-failure`, not
/// `always`: SCM's recovery actions are what restart a service whose process was terminated.
fn reporting(id: &str, restart: Restart, started: &ConnectBack, stopped: &ConnectBack) -> DaemonSpec {
    DaemonSpec::new(DaemonFields {
        restart,
        restart_delay: Some(Duration::from_secs(1)),
        ..support::plain_fields(id, fixture_command(id, started.port(), stopped.port(), "plain"))
    })
    .unwrap()
}

#[skuld::test(requires = [support::elevated], labels = [ELEVATED])]
fn a_killed_on_failure_daemon_comes_back_with_a_new_pid() {
    let mgr = Scm::new();
    let id = support::random_test_id();
    let _guard = ServiceGuard::new(&id);
    let (started, stopped) = (ConnectBack::listen(), ConnectBack::listen());
    let spec = reporting(&id, Restart::OnFailure, &started, &stopped);
    mgr.install(&spec, false).expect("install");

    support::kill::a_killed_daemon_comes_back_with_a_new_pid(&mgr, &spec.id, &started);
}

#[skuld::test(requires = [support::elevated], labels = [ELEVATED])]
fn a_killed_never_restart_daemon_stays_down() {
    let mgr = Scm::new();
    let id = support::random_test_id();
    let _guard = ServiceGuard::new(&id);
    let (started, stopped) = (ConnectBack::listen(), ConnectBack::listen());
    let spec = reporting(&id, Restart::Never, &started, &stopped);
    mgr.install(&spec, false).expect("install");

    support::kill::a_killed_daemon_that_never_restarts_stays_down(&mgr, &spec.id, &started, &|| {
        let scm = WinServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT).expect("open SCM");
        let service = scm
            .open_service(&id, ServiceAccess::QUERY_CONFIG)
            .expect("open the service to read its failure actions");
        let configured = service.get_failure_actions().expect("failure actions");
        assert!(
            configured.actions.is_none_or(|actions| actions.is_empty()),
            "the service has failure actions configured"
        );
    });
}

/// `kill` returns once SCM has recorded the exit, so for a service that never restarts the first
/// `status` after it reads not-running and has no pid. The shared `a_killed_daemon_that_never_restarts_stays_down` waits for that state
/// instead; this is the contract it hides.
#[skuld::test(requires = [support::elevated], labels = [ELEVATED])]
fn kill_returns_once_scm_has_recorded_the_exit() {
    let mgr = Scm::new();
    let id = support::random_test_id();
    let _guard = ServiceGuard::new(&id);
    let (started, stopped) = (ConnectBack::listen(), ConnectBack::listen());
    let spec = reporting(&id, Restart::Never, &started, &stopped);
    mgr.install(&spec, false).expect("install");
    mgr.start(&spec.id, Budget::DEFAULT).expect("start");
    started.accept("the fixture to report SERVICE_RUNNING");

    mgr.kill(&spec.id).expect("kill");

    let status = mgr.status(&spec.id).expect("status straight after the kill");
    assert!(
        matches!(status.state, State::Stopped | State::Failed),
        "kill returned before SCM recorded the exit ({status:?})"
    );
    assert_eq!(status.pid, None, "a killed daemon has no process ({status:?})");
}

#[skuld::test(requires = [support::elevated], labels = [ELEVATED])]
fn killing_a_stopped_service_is_not_running() {
    let mgr = Scm::new();
    let id = support::random_test_id();
    let _guard = ServiceGuard::new(&id);
    let (started, stopped) = (ConnectBack::listen(), ConnectBack::listen());
    let spec = reporting(&id, Restart::Never, &started, &stopped);
    mgr.install(&spec, false).expect("install");
    let get_manager = || -> goetia::Result<Box<dyn goetia::manager::ServiceManager>> { Ok(Box::new(Scm::new())) };

    support::kill::killing_a_stopped_daemon_is_not_running(&get_manager, &spec.id);

    mgr.start(&spec.id, Budget::DEFAULT).expect("start");
    started.accept("the fixture to report SERVICE_RUNNING");
    mgr.stop(&spec.id, Budget::DEFAULT).expect("stop");
    stopped.accept("the fixture to handle SERVICE_CONTROL_STOP");
    support::kill::killing_a_stopped_daemon_is_not_running(&get_manager, &spec.id);
}

#[skuld::test(requires = [support::elevated], labels = [ELEVATED, SCM_DENIAL], serial = SCM_DENIAL)]
fn uninstall_leaves_nothing() {
    let mgr = Scm::new();
    let id = support::random_test_id();
    let _guard = ServiceGuard::new(&id);
    let spec = DaemonSpec::new(support::plain_fields(&id, fixture_command(&id, 1, 1, "plain"))).unwrap();
    mgr.install(&spec, false).expect("install");

    mgr.uninstall(&spec.id).expect("uninstall");

    let query = support::cmd::run("sc.exe", &["query", &id]);
    assert!(!query.ok(), "service should no longer exist:\n{query}");

    let installed = mgr.list().expect("list");
    assert!(
        !support::may_account_for(&installed, &id),
        "uninstalled id must not appear in list"
    );
}

#[skuld::test(requires = [support::elevated], labels = [ELEVATED, SCM_DENIAL], serial = SCM_DENIAL)]
fn list_ignores_foreign_services() {
    let id = support::random_test_id();
    let guard = ServiceGuard::new(&id);
    seed_foreign(guard.id());

    let mgr = Scm::new();
    let installed = mgr.list().expect("list");
    assert!(
        !support::may_account_for(&installed, &id),
        "a foreign service must never appear in list"
    );
}

// Step 3: uninstall then immediate reinstall ==========================================================================

#[skuld::test(requires = [support::elevated], labels = [ELEVATED])]
fn uninstall_then_immediate_reinstall_succeeds() {
    let mgr = Scm::new();
    let id = support::random_test_id();
    let _guard = ServiceGuard::new(&id);
    let started = ConnectBack::listen();
    let stopped = ConnectBack::listen();
    let spec = DaemonSpec::new(support::plain_fields(
        &id,
        fixture_command(&id, started.port(), stopped.port(), "plain"),
    ))
    .unwrap();

    mgr.install(&spec, false).expect("install");
    mgr.start(&spec.id, Budget::DEFAULT).expect("start");
    started.accept("the fixture to report SERVICE_RUNNING");

    // Uninstalling a *running* service is the real test of
    // delete-after-stop: `delete` must not run until a confirmed stop, or the registry key survives
    // and the immediate reinstall below meets `ERROR_SERVICE_MARKED_FOR_DELETE`.
    mgr.uninstall(&spec.id).expect("uninstall a running service");
    stopped.accept("the fixture to handle SERVICE_CONTROL_STOP as part of uninstall");

    let outcome = mgr.install(&spec, false).expect("immediate reinstall");
    assert!(matches!(outcome, Outcome::Create), "{outcome:?}");
}

// Step 4: uninstall reports a service that will not stop, leaves it disabled, and is re-runnable ======================

#[skuld::test(requires = [support::elevated], labels = [ELEVATED])]
fn uninstall_errors_when_service_will_not_stop() {
    let mgr = Scm::new();
    let id = support::random_test_id();
    let _guard = ServiceGuard::new(&id);
    let started = ConnectBack::listen();
    // `refuse-stop`: the fixture reports RUNNING with an empty
    // `controls_accepted`, so the service reads RUNNING without accepting
    // STOP, and `uninstall` fails at once (a state it can see), not on a
    // wait that has to time out (this project forbids time-based sync).
    let spec = DaemonSpec::new(support::plain_fields(
        &id,
        fixture_command(&id, started.port(), 1, "refuse-stop"),
    ))
    .unwrap();

    mgr.install(&spec, false).expect("install");
    mgr.start(&spec.id, Budget::DEFAULT).expect("start");
    started.accept("the fixture to report SERVICE_RUNNING");

    let err = mgr
        .uninstall(&spec.id)
        .expect_err("a service that declines SERVICE_CONTROL_STOP cannot be stopped by uninstall");
    let text = err.report().to_string();
    assert!(
        text.contains("does not accept STOP")
            && text.contains("NOT deleted")
            && text.contains("now disabled")
            && text.contains("start type was manual")
            && text.contains(&format!("goetia daemon disable {id}"))
            && text.contains("goetia daemon kill"),
        "error should say why, that the service was not deleted and is disabled, the installed start type \
         (DEMAND_START is `manual`) with its command, and the remedy: {err}"
    );

    // The service is still there, running, and left disabled: `uninstall` does not put the start type back.
    let status = mgr.status(&spec.id).expect("status: the service must still exist");
    assert_eq!(status.state, State::Running);
    let config = support::cmd::run("sc.exe", &["qc", &id]).expect_ok();
    assert!(
        config.stdout.contains("DISABLED"),
        "start type should be left DISABLED after the failed uninstall:\n{config}"
    );

    // The named remedy, then the retry: it succeeds and the record is gone (1060).
    mgr.kill(&spec.id).expect("kill the service uninstall could not stop");
    mgr.uninstall(&spec.id).expect("re-running uninstall after the kill");
    let query = support::cmd::run("sc.exe", &["query", &id]);
    assert_eq!(
        query.code,
        Some(1060),
        "the record should be gone:
{query}"
    );
}

/// Measures which code SCM gives a STOP to a RUNNING service that accepts none (the unit tests
/// script 1052 and 1061 alike): `stop` reads the status either way and returns the "does not accept
/// STOP" error at once. The call returns; a wait would hang.
#[skuld::test(requires = [support::elevated], labels = [ELEVATED])]
fn stop_of_a_service_that_accepts_no_stop_says_so_without_waiting() {
    let mgr = Scm::new();
    let id = support::random_test_id();
    let _guard = ServiceGuard::new(&id);
    let started = ConnectBack::listen();
    let spec = DaemonSpec::new(support::plain_fields(
        &id,
        fixture_command(&id, started.port(), 1, "refuse-stop"),
    ))
    .unwrap();

    mgr.install(&spec, false).expect("install");
    mgr.start(&spec.id, Budget::DEFAULT).expect("start");
    started.accept("the fixture to report SERVICE_RUNNING");

    for (what, budget) in [("stop", Budget::Unbounded), ("stop --timeout 0", Budget::Immediate)] {
        let text = mgr
            .stop(&spec.id, budget)
            .expect_err("a service that declines SERVICE_CONTROL_STOP cannot be stopped")
            .report()
            .to_string();
        assert!(
            text.contains("does not accept STOP") && text.contains("goetia daemon kill"),
            "{what}: {text}"
        );
    }

    mgr.kill(&spec.id).expect("kill the service stop could not stop");
}

// Step 5: an install interrupted before the metadata write is recoverable =============================================

#[skuld::test(requires = [support::elevated], labels = [ELEVATED])]
fn interrupted_install_is_recoverable() {
    let mgr = Scm::new();
    let id = support::random_test_id();
    let guard = ServiceGuard::new(&id);
    let spec = DaemonSpec::new(support::plain_fields(&id, fixture_command(&id, 1, 1, "plain"))).unwrap();

    // Simulate a crash between `CreateServiceW` and the `Parameters` write:
    // create the real service by hand, with the command `registration()`
    // would build, but never write `Parameters` at all.
    let cmd = fixture_command(&id, 1, 1, "plain");
    let cmdline = format!("\"{}\" {}", cmd[0], cmd[1..].join(" "));
    support::cmd::run(
        "sc.exe",
        &["create", guard.id(), "binPath=", &cmdline, "start=", "demand"],
    )
    .expect_ok();

    // `type: managed` has no second ownership proof (see the module doc
    // comment on `manager`): the orphan is indistinguishable from a
    // stranger's service, so `install` must refuse rather than adopt it —
    // with or without `--force`, which never overrides a foreign refusal.
    for force in [false, true] {
        let outcome = mgr
            .install(&spec, force)
            .unwrap_or_else(|e| panic!("install over the orphan must not error (force={force}): {e}"));
        match outcome {
            Outcome::RefuseForeign { recovery } => assert!(!recovery.is_empty()),
            other => panic!("expected RefuseForeign (force={force}), got {other:?}"),
        }
    }

    // The named recovery is real: delete the orphan through the native
    // tool, then `install` completes it cleanly.
    support::cmd::run("sc.exe", &["delete", guard.id()]).expect_ok();
    let recovered = mgr.install(&spec, false).expect("install after the recovery command");
    assert!(matches!(recovered, Outcome::Create), "{recovered:?}");
}

// Step 6: env on type: managed (spec §8 must-verify #5) ===============================================================

#[skuld::test(requires = [support::elevated], labels = [ELEVATED])]
fn managed_kind_environment_availability() {
    const VAR: &str = "GOETIA_SCM_ENV_PROBE";
    const VALUE: &str = "goetia-managed-env-probe-value";

    let mgr = Scm::new();
    let id = support::random_test_id();
    let _guard = ServiceGuard::new(&id);
    let started = ConnectBack::listen();

    let mut env = BTreeMap::new();
    env.insert(VAR.to_string(), VALUE.to_string());
    let spec = DaemonSpec::new(DaemonFields {
        env,
        ..support::plain_fields(&id, fixture_command(&id, started.port(), 1, &format!("env:{VAR}")))
    })
    .unwrap();

    mgr.install(&spec, false).expect("install");
    mgr.start(&spec.id, Budget::DEFAULT).expect("start");
    let reported = started.accept_line("the fixture to report its own environment");

    support::record_probe(
        "scm-managed-env",
        &format!(
            "mechanism: HKLM\\SYSTEM\\CurrentControlSet\\Services\\<name>\\Environment (REG_MULTI_SZ)\n\
             var: {VAR}\n\
             expected: {VALUE}\n\
             observed: {reported}\n\
             supported: {}\n",
            reported == VALUE,
        ),
    );
    assert_eq!(
        reported, VALUE,
        "type: managed env support (design spec §8 must-verify #5): the daemon process did not see its \
         configured environment variable"
    );
}

// `Environment` is part of the compared registration ==================================================================

fn environment_of(id: &str) -> Option<Vec<String>> {
    match service_key(id, KEY_READ).get_value::<Vec<String>, _>("Environment") {
        Ok(lines) => Some(lines),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => panic!("read Environment of {id}: {e}"),
    }
}

/// A drifted registration previews as a `Conflict`, `install --force` restores it, and the next
/// preview is `UpToDate`. Returns the conflict's diff.
fn assert_conflict_cleared_by_force(mgr: &Scm, spec: &DaemonSpec) -> String {
    let outcome = mgr.preview_install(spec).expect("preview_install");
    let Outcome::Conflict { artifact_diff, .. } = outcome else {
        panic!("expected Conflict for a hand-edited registration, got {outcome:?}");
    };

    let forced = mgr.install(spec, true).expect("forced install");
    assert!(!matches!(forced, Outcome::Conflict { .. }), "{forced:?}");
    let again = mgr.preview_install(spec).expect("preview_install after force");
    assert!(matches!(again, Outcome::UpToDate), "{again:?}");
    artifact_diff
}

const INSTALLED_ENVIRONMENT: &str = "GOETIA_TEST_KEY=value";

/// An installed `type: managed` service whose spec sets one environment variable.
fn install_with_environment(mgr: &Scm, id: &str) -> DaemonSpec {
    let env = BTreeMap::from([("GOETIA_TEST_KEY".to_string(), "value".to_string())]);
    let spec = DaemonSpec::new(DaemonFields {
        env,
        ..support::plain_fields(id, fixture_command(id, 1, 1, "plain"))
    })
    .unwrap();
    mgr.install(&spec, false).expect("install");
    spec
}

#[skuld::test(requires = [support::elevated], labels = [ELEVATED])]
fn a_hand_edited_environment_is_a_conflict() {
    let mgr = Scm::new();
    let id = support::random_test_id();
    let _guard = ServiceGuard::new(&id);
    let spec = install_with_environment(&mgr, &id);

    service_key(&id, KEY_WRITE)
        .set_value("Environment", &vec!["HAND_EDITED=1".to_string()])
        .expect("overwrite Environment");

    let diff = assert_conflict_cleared_by_force(&mgr, &spec);
    assert!(diff.contains("HAND_EDITED=1"), "{diff}");
    assert_eq!(environment_of(&id), Some(vec![INSTALLED_ENVIRONMENT.to_string()]));
}

#[skuld::test(requires = [support::elevated], labels = [ELEVATED])]
fn a_deleted_environment_is_a_conflict() {
    let mgr = Scm::new();
    let id = support::random_test_id();
    let _guard = ServiceGuard::new(&id);
    let spec = install_with_environment(&mgr, &id);

    service_key(&id, KEY_WRITE)
        .delete_value("Environment")
        .expect("delete Environment");

    assert_conflict_cleared_by_force(&mgr, &spec);
    assert_eq!(environment_of(&id), Some(vec![INSTALLED_ENVIRONMENT.to_string()]));
}

/// Failure actions are compared too, and `sc.exe` is how they are edited by hand.
#[skuld::test(requires = [support::elevated], labels = [ELEVATED])]
fn reset_failure_actions_are_a_conflict() {
    let mgr = Scm::new();
    let id = support::random_test_id();
    let _guard = ServiceGuard::new(&id);
    let spec = DaemonSpec::new(DaemonFields {
        restart: Restart::OnFailure,
        restart_delay: Some(Duration::from_secs(3)),
        ..support::plain_fields(&id, fixture_command(&id, 1, 1, "plain"))
    })
    .unwrap();
    mgr.install(&spec, false).expect("install");

    support::cmd::run("sc.exe", &["failureflag", &id, "0"]).expect_ok();
    support::cmd::run("sc.exe", &["failure", &id, "reset=", "0", "actions=", ""]).expect_ok();

    let diff = assert_conflict_cleared_by_force(&mgr, &spec);
    assert!(diff.contains("FailureActions"), "{diff}");
}

/// A wrong-typed `Environment` is a hand edit too: a `Conflict` that `--force` clears, not an error.
#[skuld::test(requires = [support::elevated], labels = [ELEVATED])]
fn a_wrong_typed_environment_is_a_conflict() {
    let mgr = Scm::new();
    let id = support::random_test_id();
    let _guard = ServiceGuard::new(&id);
    let spec = install_with_environment(&mgr, &id);

    service_key(&id, KEY_WRITE)
        .set_value("Environment", &INSTALLED_ENVIRONMENT)
        .expect("overwrite Environment with a REG_SZ");

    let diff = assert_conflict_cleared_by_force(&mgr, &spec);
    assert!(diff.contains("REG_SZ"), "{diff}");
    assert_eq!(environment_of(&id), Some(vec![INSTALLED_ENVIRONMENT.to_string()]));
}

/// The registry stores any type number; `winreg` cannot read one above `REG_QWORD` (11).
#[skuld::test(requires = [support::elevated], labels = [ELEVATED])]
fn an_environment_of_an_unknown_registry_type_is_a_conflict() {
    const UNKNOWN_TYPE: u32 = 12;

    let mgr = Scm::new();
    let id = support::random_test_id();
    let _guard = ServiceGuard::new(&id);
    let spec = install_with_environment(&mgr, &id);

    let key = service_key(&id, KEY_WRITE);
    let name: Vec<u16> = "Environment".encode_utf16().collect();
    set_raw_value(&key, &name, UNKNOWN_TYPE, INSTALLED_ENVIRONMENT.as_bytes());

    let diff = assert_conflict_cleared_by_force(&mgr, &spec);
    assert!(diff.contains(&format!("type {UNKNOWN_TYPE}")), "{diff}");
    assert_eq!(environment_of(&id), Some(vec![INSTALLED_ENVIRONMENT.to_string()]));
}

/// An `UpToDate` install writes nothing, so a second install of either kind must compare equal on
/// everything the first wrote: failure actions, `Environment`, `Parameters`. A `type: simple`
/// service's `Environment` is empty on both sides, whatever its spec's `env` says. It is registered
/// but never started, so the shim is only provided and checked, never run: its manager has a provider,
/// which `Scm::new()` has not.
#[skuld::test(requires = [support::elevated], labels = [ELEVATED, UNIT_DIR_EXCLUSIVE], serial = UNIT_DIR_EXCLUSIVE)]
fn install_twice_is_up_to_date_for_both_kinds() {
    for kind in [Kind::Managed, Kind::Simple] {
        let mgr = match kind {
            Kind::Managed => Scm::new(),
            Kind::Simple => support::stock_shim_manager(),
        };
        let id = support::random_test_id();
        let _guard = ServiceGuard::new(&id);
        let env = BTreeMap::from([("GOETIA_TEST_KEY".to_string(), "value".to_string())]);
        let spec = DaemonSpec::new(DaemonFields {
            kind,
            env,
            restart: Restart::OnFailure,
            restart_delay: Some(Duration::from_secs(3)),
            ..support::plain_fields(&id, fixture_command(&id, 1, 1, "plain"))
        })
        .unwrap();

        let first = mgr.install(&spec, false).expect("install");
        assert!(matches!(first, Outcome::Create), "{kind:?}: {first:?}");
        let second = mgr.install(&spec, false).expect("reinstall, unchanged spec");
        assert!(matches!(second, Outcome::UpToDate), "{kind:?}: {second:?}");

        let expected = match kind {
            Kind::Managed => Some(vec!["GOETIA_TEST_KEY=value".to_string()]),
            Kind::Simple => None,
        };
        assert_eq!(environment_of(&id), expected, "{kind:?}");
    }
}

/// goetia owns `Parameters` whole: `install --force` removes a stray value and a subkey, and a
/// marker stored in another case is still ours, so the service reads as unchanged afterwards.
#[skuld::test(requires = [support::elevated], labels = [ELEVATED])]
fn force_install_removes_strays_from_parameters_and_keeps_the_service_ours() {
    let mgr = Scm::new();
    let id = support::random_test_id();
    let _guard = ServiceGuard::new(&id);
    let spec = install_plain(&mgr, &id);

    let parameters = parameters_key(&id, KEY_READ | KEY_WRITE);
    let marker: String = parameters.get_value("Marker").expect("read Marker");
    parameters.delete_value("Marker").expect("delete Marker");
    parameters.set_value("marker", &marker).expect("write marker");
    parameters.set_value("Stray", &"x").expect("write Stray");
    let (subkey, _) = parameters.create_subkey("Sub").expect("create Sub");
    subkey.set_value("Nested", &"y").expect("write Nested");

    let forced = mgr.install(&spec, true).expect("forced install");
    assert!(!matches!(forced, Outcome::Conflict { .. }), "{forced:?}");

    let parameters = parameters_key(&id, KEY_READ);
    let stray = parameters.get_value::<String, _>("Stray");
    assert!(
        matches!(&stray, Err(e) if e.kind() == std::io::ErrorKind::NotFound),
        "the stray value survived: {stray:?}"
    );
    let sub = parameters.open_subkey("Sub");
    assert!(
        matches!(&sub, Err(e) if e.kind() == std::io::ErrorKind::NotFound),
        "the subkey survived"
    );
    let again = mgr.preview_install(&spec).expect("preview_install after force");
    assert!(matches!(again, Outcome::UpToDate), "{again:?}");
    mgr.status(&spec.id).expect("status: the service still reads as ours");
}

/// A stray subkey, or a value that is not a string, is the only difference from what goetia wrote:
/// it is still a `Conflict`, and `install --force` removes it.
#[skuld::test(requires = [support::elevated], labels = [ELEVATED])]
fn a_stray_subkey_or_non_string_value_alone_is_a_conflict_that_force_removes() {
    let mgr = Scm::new();
    let id = support::random_test_id();
    let _guard = ServiceGuard::new(&id);
    let spec = install_plain(&mgr, &id);

    let parameters = parameters_key(&id, KEY_READ | KEY_WRITE);
    let (subkey, _) = parameters.create_subkey("Sub").expect("create Sub");
    subkey.set_value("Nested", &"y").expect("write Nested");
    let diff = assert_conflict_cleared_by_force(&mgr, &spec);
    assert!(diff.contains("Sub"), "{diff}");
    let parameters = parameters_key(&id, KEY_READ | KEY_WRITE);
    assert!(
        matches!(parameters.open_subkey("Sub"), Err(e) if e.kind() == std::io::ErrorKind::NotFound),
        "the subkey survived"
    );

    parameters.set_value("Count", &7u32).expect("write a REG_DWORD");
    let diff = assert_conflict_cleared_by_force(&mgr, &spec);
    assert!(diff.contains("Count"), "{diff}");
    let count = parameters_key(&id, KEY_READ).get_value::<u32, _>("Count");
    assert!(
        matches!(&count, Err(e) if e.kind() == std::io::ErrorKind::NotFound),
        "the REG_DWORD survived: {count:?}"
    );
    mgr.status(&spec.id).expect("status: the service still reads as ours");
}

/// `RegDeleteTreeW` follows a registry link: a stray link under `Parameters` would have its target
/// wiped. `--force` deletes the link itself and leaves the target as it was.
#[skuld::test(requires = [support::elevated], labels = [ELEVATED])]
fn force_install_deletes_a_stray_registry_link_and_leaves_its_target() {
    let mgr = Scm::new();
    let id = support::random_test_id();
    // Declared first, so that it is removed last.
    let target = ScratchKey::new(&format!("link-target-{id}"));
    let _guard = ServiceGuard::new(&id);
    let spec = install_plain(&mgr, &id);
    let target_key = target.create();
    target_key.set_value("Keep", &"x").expect("write Keep");
    target_key.create_subkey("Child").expect("create Child");
    let parameters = parameters_key(&id, KEY_READ | KEY_WRITE);
    let _link = create_link(&parameters, "Link", &target.nt_path());

    assert_conflict_cleared_by_force(&mgr, &spec);

    let parameters = parameters_key(&id, KEY_READ);
    let names: Vec<String> = parameters.enum_keys().map(|name| name.expect("enumerate")).collect();
    assert!(names.is_empty(), "the link survived: {names:?}");
    assert_eq!(target_key.get_value::<String, _>("Keep").expect("read Keep"), "x");
    target_key
        .open_subkey("Child")
        .expect("the target's own subkey survived");
}

/// `Parameters` itself may be a registry link. A link key cannot hold goetia's values (a write of
/// any value but `SymbolicLinkValue` to it is denied), and goetia reads it as itself, never through
/// its target: so a service whose `Parameters` was replaced by a link to a key holding goetia's four
/// values is no longer goetia's. `list` omits it, `status` and `install`, forced or not, refuse it as
/// foreign, and the target and the link are as they were.
#[skuld::test(requires = [support::elevated], labels = [ELEVATED, SCM_DENIAL], serial = SCM_DENIAL)]
fn a_parameters_that_is_a_link_to_goetias_values_is_foreign_and_install_leaves_target_and_link_alone() {
    let mgr = Scm::new();
    let id = support::random_test_id();
    // Declared first, so that it is removed last.
    let target = ScratchKey::new(&format!("link-target-{id}"));
    let _guard = ServiceGuard::new(&id);
    let spec = install_plain(&mgr, &id);
    let units = |text: &str| -> Vec<u16> { text.encode_utf16().collect() };

    // The target holds goetia's four values, copied from the installed `Parameters`, and a `Keep`.
    let parameters = parameters_key(&id, KEY_READ);
    let fields: Vec<(String, winreg::RegValue)> = parameters.enum_values().map(|v| v.expect("enumerate")).collect();
    drop(parameters);
    let target_key = target.create();
    target_key.set_value("Keep", &"x").expect("write Keep");
    for (name, value) in &fields {
        target_key
            .set_raw_value(name, value)
            .expect("copy a value to the target");
    }

    // `Parameters` becomes a link to it.
    let service = service_key(&id, KEY_READ | KEY_WRITE);
    service.delete_subkey_all("Parameters").expect("remove Parameters");
    let _link = create_link(&service, "Parameters", &target.nt_path());

    let listed = mgr.list().expect("list");
    assert!(
        !listed.iter().any(|entry| match entry {
            Installed::Ours { spec, .. } => spec.id.as_str() == id,
            Installed::OursUnusable { name, .. } => name == &id,
            // An aggregate may stand for this id: fail rather than certify what `list` cannot.
            Installed::Undetermined { name, .. } => name.as_deref().is_none_or(|n| n == id),
        }),
        "a service whose Parameters is a link is not goetia's, and omitted: {listed:?}"
    );
    let status = mgr.status(&spec.id).expect_err("status of a foreign service");
    assert!(matches!(status.cause(), goetia::Error::Foreign { .. }), "{status:?}");
    for force in [false, true] {
        let outcome = mgr.install(&spec, force).expect("install");
        assert!(
            matches!(outcome, Outcome::RefuseForeign { .. }),
            "force={force}: {outcome:?}"
        );
    }

    // The refusals wrote nothing through the link. A write through it would have removed `Keep`,
    // which is a stray to goetia, and the link still points at the target: opened as itself it holds
    // its `SymbolicLinkValue`, and opened as every other reader does it is the target.
    assert_eq!(target_key.get_value::<String, _>("Keep").expect("read Keep"), "x");
    let as_itself = create_raw_key(&service, &units("Parameters"), 0);
    let held = as_itself
        .get_raw_value("SymbolicLinkValue")
        .expect("Parameters is still a link");
    let link_value: Vec<u8> = units(&target.nt_path())
        .into_iter()
        .flat_map(u16::to_le_bytes)
        .collect();
    assert_eq!(held.bytes, link_value);
    let followed = parameters_key(&id, KEY_READ);
    for (name, value) in &fields {
        let seen = followed
            .get_raw_value(name)
            .unwrap_or_else(|e| panic!("the target lost {name}: {e}"));
        assert_eq!(seen.bytes, value.bytes, "{name} through the link");
        assert_eq!(seen.vtype, value.vtype, "{name} through the link");
    }
}

/// `RegDeleteTreeW` below the top level stops at a NUL, so a stray subkey holding a child named
/// `a\0b` would stay. `--force` removes the whole subtree.
#[skuld::test(requires = [support::elevated], labels = [ELEVATED])]
fn force_install_removes_a_stray_subkey_holding_a_child_whose_name_has_a_nul() {
    let mgr = Scm::new();
    let id = support::random_test_id();
    let _guard = ServiceGuard::new(&id);
    let spec = install_plain(&mgr, &id);
    let parameters = parameters_key(&id, KEY_READ | KEY_WRITE);
    let (sub, _) = parameters.create_subkey("Sub").expect("create Sub");
    let nul_named: Vec<u16> = "a\0b".encode_utf16().collect();
    let child = create_raw_key(&sub, &nul_named, 0);
    child.set_value("Nested", &"y").expect("write Nested");
    drop(child);
    drop(sub);

    assert_conflict_cleared_by_force(&mgr, &spec);

    let parameters = parameters_key(&id, KEY_READ);
    let names: Vec<String> = parameters.enum_keys().map(|name| name.expect("enumerate")).collect();
    assert!(names.is_empty(), "the subtree survived: {names:?}");
}

/// What `winreg` cannot enumerate must still be drift that `install --force` removes: a goetia field
/// stored as `REG_EXPAND_SZ`, a value of a type above `REG_QWORD`, and a name that is not valid UTF-16.
#[skuld::test(requires = [support::elevated], labels = [ELEVATED])]
fn force_install_removes_strays_that_winreg_cannot_enumerate() {
    const REG_SZ: u32 = 1;
    const REG_EXPAND_SZ: u32 = 2;
    const CUSTOM_TYPE: u32 = 0x20;
    let mgr = Scm::new();
    let id = support::random_test_id();
    let _guard = ServiceGuard::new(&id);
    let spec = install_plain(&mgr, &id);
    let parameters = parameters_key(&id, KEY_READ | KEY_WRITE);
    let units = |text: &str| -> Vec<u16> { text.encode_utf16().collect() };
    // UTF-16LE with the closing NUL, as a string value is stored.
    let sz = |text: &str| -> Vec<u8> { units(text).into_iter().chain([0]).flat_map(u16::to_le_bytes).collect() };

    set_raw_value(&parameters, &units("Schema"), REG_EXPAND_SZ, &sz("1"));
    let diff = assert_conflict_cleared_by_force(&mgr, &spec);
    assert!(diff.contains("Schema") && diff.contains("REG_EXPAND_SZ"), "{diff}");

    // A `Marker` another tool rewrote as another string type is still goetia's: the service stays
    // ours, and `--force` writes it back as `REG_SZ`.
    let marker: String = parameters.get_value("Marker").expect("read Marker");
    set_raw_value(&parameters, &units("Marker"), REG_EXPAND_SZ, &sz(&marker));
    mgr.status(&spec.id).expect("status: a rewritten Marker is still ours");
    let diff = assert_conflict_cleared_by_force(&mgr, &spec);
    assert!(diff.contains("Marker") && diff.contains("REG_EXPAND_SZ"), "{diff}");
    assert_eq!(
        parameters.get_value::<String, _>("Marker").expect("read Marker"),
        marker
    );
    assert_eq!(
        parameters.get_raw_value("Marker").expect("read Marker raw").vtype,
        RegType::REG_SZ,
        "--force writes Marker back as REG_SZ"
    );

    set_raw_value(&parameters, &units("Custom"), CUSTOM_TYPE, b"bytes");
    let diff = assert_conflict_cleared_by_force(&mgr, &spec);
    assert!(
        diff.contains("Custom") && diff.contains(&CUSTOM_TYPE.to_string()),
        "{diff}"
    );

    set_raw_value(&parameters, &[0x41, 0xD800], REG_SZ, &sz("x"));
    let diff = assert_conflict_cleared_by_force(&mgr, &spec);
    assert!(diff.contains(r#"value "A\u{d800}" of type REG_SZ"#), "{diff}");
    mgr.status(&spec.id).expect("status: the service still reads as ours");
}

/// `RegDeleteValueW` stops at a NUL, so a stray `Marker\0x` would delete goetia's `Marker` or stay.
/// `--force` removes it by its whole name and leaves goetia's four values as they were.
#[skuld::test(requires = [support::elevated], labels = [ELEVATED])]
fn force_install_removes_a_stray_whose_name_holds_a_nul_and_keeps_marker() {
    const REG_SZ: u32 = 1;
    let mgr = Scm::new();
    let id = support::random_test_id();
    let _guard = ServiceGuard::new(&id);
    let spec = install_plain(&mgr, &id);
    let parameters = parameters_key(&id, KEY_READ | KEY_WRITE);
    let marker: String = parameters.get_value("Marker").expect("read Marker");
    let units: Vec<u16> = "Marker\0x".encode_utf16().collect();
    let bytes: Vec<u8> = "x\0".encode_utf16().flat_map(u16::to_le_bytes).collect();

    set_raw_value(&parameters, &units, REG_SZ, &bytes);
    let diff = assert_conflict_cleared_by_force(&mgr, &spec);

    assert!(diff.contains(r"Marker\0x"), "{diff}");
    assert_eq!(
        parameters.get_value::<String, _>("Marker").expect("read Marker"),
        marker
    );
    let mut names: Vec<String> = parameters
        .enum_values()
        .map(|value| value.expect("enumerate").0)
        .collect();
    names.sort();
    assert_eq!(names, ["Marker", "Schema", "Spec", "Version"]);
}

/// The key is rewritten, never replaced: a handle opened before `--force` reads the new values after.
#[skuld::test(requires = [support::elevated], labels = [ELEVATED])]
fn force_install_rewrites_parameters_in_place_so_a_handle_held_across_it_still_reads() {
    let mgr = Scm::new();
    let id = support::random_test_id();
    let _guard = ServiceGuard::new(&id);
    let spec = install_plain(&mgr, &id);
    let held = parameters_key(&id, KEY_READ | KEY_WRITE);
    held.set_value("Stray", &"x").expect("write Stray");

    let forced = mgr.install(&spec, true).expect("forced install");
    assert!(!matches!(forced, Outcome::Conflict { .. }), "{forced:?}");

    held.get_value::<String, _>("Marker")
        .expect("the handle opened before --force still reads Marker");
    let stray = held.get_value::<String, _>("Stray");
    assert!(
        matches!(&stray, Err(e) if e.kind() == std::io::ErrorKind::NotFound),
        "--force left the stray behind: {stray:?}"
    );
}

// Step 7: a real account gets SeServiceLogonRight =====================================================================

#[skuld::test(requires = [support::elevated], labels = [ELEVATED])]
fn real_account_gets_service_logon_right() {
    // Windows local (SAM) account names are capped at 20 characters.
    let suffix = format!("{:08x}", rand::random::<u32>());
    let account = format!("gt{suffix}");
    // <=14 chars: longer passwords make `net user ... /add` prompt
    // interactively about pre-Windows-2000 compatibility, which hangs
    // waiting on stdin in CI.
    let password = "Goetia!Test12";
    let _user_guard = support::LocalUserGuard::create(&account, password);

    let id = support::random_test_id();
    let _guard = ServiceGuard::new(&id);

    // `GOETIA_SERVICE_PASSWORD` is scoped to this one child process — see
    // `install_helper.rs`'s module doc comment for why that matters.
    let output = Command::new(support::current_exe_str())
        .arg(INSTALL_AS)
        .arg(&id)
        .arg(format!(r".\{account}"))
        .env("GOETIA_SERVICE_PASSWORD", password)
        .output()
        .expect("spawn the install-as-account helper");
    assert!(
        output.status.success(),
        "install as a real account failed: stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    // The real proof: the service can actually reach RUNNING under this
    // account. Without `SeServiceLogonRight`, `StartServiceW` fails with
    // error 1069 (`ERROR_LOGON_FAILURE`) — `start` uses a real
    // `NotifyServiceStatusChangeW` wait (`scm_wait`), so this either
    // observes RUNNING or fails immediately, never hanging on a poll.
    let mgr = Scm::new();
    let target = id_of(&id);
    // `Budget::Unbounded`, not `DEFAULT`: the subject here is the logon right, not latency. A
    // first logon under a freshly created account builds a user profile, which routinely outruns
    // the 10s default on a runner — measured at 24,737 ms on a green run, and 14,349 ms when
    // `DEFAULT` turned it into a failure whose message blamed `SeServiceLogonRight` for what was
    // only a slow profile build. A larger fixed number would just move the guess; unbounded says
    // what is meant.
    mgr.start(&target, Budget::Unbounded).unwrap_or_else(|e| {
        panic!("service under a real account failed to start (SeServiceLogonRight likely not granted): {e}")
    });
    let status = mgr.status(&target).expect("status");
    assert_eq!(status.state, State::Running);
    let _ = mgr.stop(&target, Budget::DEFAULT);
}

// Step 8: built-in service accounts install and run without a password or a logon-right grant =========================

/// `S-1-5-19` is `LocalService`'s well-known SID. `LookupAccountSidW` on an
/// English runner resolves it to `NT AUTHORITY\LOCAL SERVICE` — a space,
/// not the `LocalService` an author writes — so this passes both with and
/// without `windows_builtin`'s space-folding. Its real value is as a
/// regression marker for a *localised* host, where the name lookup would
/// return something in another language the fold cannot match at all: do
/// not delete this as redundant with `local_service_account_installs_and_
/// round_trips` below, which never goes through `LookupAccountSidW` at
/// all.
#[skuld::test(requires = [support::elevated], labels = [ELEVATED])]
fn a_well_known_sid_resolves_without_a_name_lookup() {
    let mgr = Scm::new();
    let id = support::random_test_id();
    let _guard = ServiceGuard::new(&id);
    let spec = DaemonSpec::new(DaemonFields {
        user: User::Id(AccountId::Sid("S-1-5-19".to_string())),
        ..support::plain_fields(&id, fixture_command(&id, 1, 1, "plain"))
    })
    .unwrap();

    let outcome = mgr
        .install(&spec, false)
        .expect("a well-known SID resolving to a built-in account must not need a password");
    assert!(matches!(outcome, Outcome::Create), "{outcome:?}");
    assert_eq!(
        common::query_account_name(&id).as_deref(),
        Some(r"NT AUTHORITY\LocalService"),
        "SCM must store the canonical spelling, not whatever LookupAccountSidW happened to return"
    );
}

#[skuld::test(requires = [support::elevated], labels = [ELEVATED])]
fn local_service_account_installs_and_round_trips() {
    let mgr = Scm::new();
    let id = support::random_test_id();
    let _guard = ServiceGuard::new(&id);
    let spec = DaemonSpec::new(DaemonFields {
        user: User::Name("LocalService".to_string()),
        ..support::plain_fields(&id, fixture_command(&id, 1, 1, "plain"))
    })
    .unwrap();

    let outcome = mgr
        .install(&spec, false)
        .expect("installing as LocalService must not require GOETIA_SERVICE_PASSWORD");
    assert!(matches!(outcome, Outcome::Create), "{outcome:?}");
    assert_eq!(
        common::query_account_name(&id).as_deref(),
        Some(r"NT AUTHORITY\LocalService")
    );

    // The real proof: a second, unchanged install must see no drift at
    // all — if SCM had stored a different spelling than it was given, this
    // would be a permanent phantom diff on every future install of this id.
    let second = mgr.install(&spec, false).expect("reinstall, unchanged spec");
    assert!(matches!(second, Outcome::UpToDate), "{second:?}");
}

#[skuld::test(requires = [support::elevated], labels = [ELEVATED])]
fn a_local_service_daemon_actually_runs() {
    let mgr = Scm::new();
    let id = support::random_test_id();
    let _guard = ServiceGuard::new(&id);
    // Held past the `install`/`start`/`stop` calls below and dropped (which
    // deletes the copy) only once nothing further needs to launch it.
    let exe = support::WorldReadableExe::new(&id);
    let started = ConnectBack::listen();
    let stopped = ConnectBack::listen();
    let spec = DaemonSpec::new(DaemonFields {
        user: User::Name("LocalService".to_string()),
        ..support::plain_fields(
            &id,
            common::fixture_command_with_exe(&exe.path_str(), &id, started.port(), stopped.port(), "plain"),
        )
    })
    .unwrap();

    mgr.install(&spec, false).expect("install as LocalService");
    // The real claim this feature makes: not just that the account string
    // is syntactically acceptable to `CreateServiceW` (`install` already
    // proves that), but that it is an identity Windows can actually launch
    // a process under.
    // Unbounded for the reason `real_account_gets_service_logon_right` gives: the subject is the
    // identity, not latency, and a timeout would read as "LocalService cannot launch a process".
    mgr.start(&spec.id, Budget::Unbounded).expect("start as LocalService");
    started.accept("the fixture to report SERVICE_RUNNING under LocalService");
    let status = mgr.status(&spec.id).expect("status while running");
    assert_eq!(status.state, State::Running);

    mgr.stop(&spec.id, Budget::DEFAULT).expect("stop");
    stopped.accept("the fixture to handle SERVICE_CONTROL_STOP under LocalService");
}

#[skuld::test(requires = [support::elevated], labels = [ELEVATED])]
fn local_system_by_name_installs_like_root() {
    let mgr = Scm::new();
    let id = support::random_test_id();
    let _guard = ServiceGuard::new(&id);
    let by_name = DaemonSpec::new(DaemonFields {
        user: User::Name("LocalSystem".to_string()),
        ..support::plain_fields(&id, fixture_command(&id, 1, 1, "plain"))
    })
    .unwrap();
    let outcome = mgr.install(&by_name, false).expect("install as LocalSystem by name");
    assert!(matches!(outcome, Outcome::Create), "{outcome:?}");

    // Not an equality check on the whole `ScmRegistration`: `generate::
    // registration` embeds the whole spec in the registration's parameters
    // (`FIELD_SPEC`, `src/backend/scm/generate.rs:199`), and `spec.user` is
    // `User::Name("LocalSystem")` here but `User::Root` for the comparison
    // install below (`src/spec.rs:104-115`), so the two blobs always
    // differ. What must agree is the live account SCM reports — the same
    // account `User::Root` produces.
    let root_id = support::random_test_id();
    let _root_guard = ServiceGuard::new(&root_id);
    let by_root = DaemonSpec::new(support::plain_fields(
        &root_id,
        fixture_command(&root_id, 1, 1, "plain"),
    ))
    .unwrap();
    mgr.install(&by_root, false).expect("install as User::Root");

    let by_name_account = common::query_account_name(&id);
    let by_root_account = common::query_account_name(&root_id);
    assert!(
        by_name_account.is_some(),
        "a LocalSystem service always reports *some* spelling of its account, never nothing"
    );
    assert_eq!(
        by_name_account, by_root_account,
        "user: {{name: LocalSystem}} must read back identically to user: root"
    );
}

#[skuld::test(requires = [support::elevated], labels = [ELEVATED])]
fn a_builtin_account_is_never_given_a_stale_password() {
    let id = support::random_test_id();
    let _guard = ServiceGuard::new(&id);

    // `GOETIA_SERVICE_PASSWORD` is scoped to this one child process — see
    // `install_helper.rs`'s module doc comment for why that matters.
    // Windows rejects a real password for a built-in account outright, so
    // a *successful start* afterwards is the only observable proof this
    // value never reached `CreateServiceW`: `install` succeeding alone
    // would not distinguish "never read" from "read, and happened not to
    // be rejected".
    let output = Command::new(support::current_exe_str())
        .arg(INSTALL_AS)
        .arg(&id)
        .arg("LocalService")
        .env("GOETIA_SERVICE_PASSWORD", "not-a-real-password")
        .output()
        .expect("spawn the install-as-account helper");
    assert!(
        output.status.success(),
        "install as LocalService with a stale password set failed: stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let mgr = Scm::new();
    let target = id_of(&id);
    // Unbounded for the reason `real_account_gets_service_logon_right` gives: a timeout here would
    // blame a stale password for what was only a slow start.
    mgr.start(&target, Budget::Unbounded).unwrap_or_else(|e| {
        panic!("service under LocalService failed to start (a stale password likely reached CreateServiceW): {e}")
    });
    let status = mgr.status(&target).expect("status");
    assert_eq!(status.state, State::Running);
    let _ = mgr.stop(&target, Budget::DEFAULT);
}

// A precondition that fails fails the install before any service write ================================================

/// One `install` of `id` as `account`, with a password set, in a child process (see `install_helper.rs`).
fn install_as(id: &str, account: &str) -> std::process::Output {
    Command::new(support::current_exe_str())
        .arg(INSTALL_AS)
        .arg(id)
        .arg(account)
        .env("GOETIA_SERVICE_PASSWORD", "Goetia!Test12")
        .output()
        .expect("spawn the install-as-account helper")
}

/// A `user.name` is not resolved at install time, so an account that does not exist reaches the
/// `SeServiceLogonRight` grant, which cannot look it up: the first precondition of `apply` fails.
fn missing_account() -> String {
    format!("gtnone{:08x}", rand::random::<u32>())
}

#[skuld::test(requires = [support::elevated], labels = [ELEVATED])]
fn a_failed_precondition_creates_no_service() {
    let id = support::random_test_id();
    let guard = ServiceGuard::new(&id);
    let account = missing_account();

    let output = install_as(guard.id(), &format!(r".\{account}"));

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        !output.status.success(),
        "install as a missing account succeeded: {stdout}"
    );
    assert!(stdout.contains(&account), "the error must name the account: {stdout}");
    let query = support::cmd::run("sc.exe", &["query", guard.id()]);
    assert!(
        !query.ok(),
        "no service may exist after a failed precondition:\n{query}"
    );
}

#[skuld::test(requires = [support::elevated], labels = [ELEVATED])]
fn a_failed_precondition_leaves_an_installed_service_unchanged() {
    let mgr = Scm::new();
    let id = support::random_test_id();
    let _guard = ServiceGuard::new(&id);
    let spec = install_plain(&mgr, &id);
    let account_before = common::query_account_name(&id);

    let account = missing_account();
    let output = install_as(&id, &format!(r".\{account}"));
    let stdout = String::from_utf8_lossy(&output.stdout);

    assert!(
        !output.status.success(),
        "install as a missing account succeeded: {stdout}"
    );
    assert!(stdout.contains(&account), "the error must name the account: {stdout}");
    assert_eq!(common::query_account_name(&id), account_before);
    let after = mgr.preview_install(&spec).expect("preview_install");
    assert!(matches!(after, Outcome::UpToDate), "{after:?}");
}

// Round-tripping restart: on-failure (the Some(fa) branch of apply_failure_actions/read_failure_actions) ==============

#[skuld::test(requires = [support::elevated], labels = [ELEVATED, SCM_DENIAL], serial = SCM_DENIAL)]
fn restart_on_failure_round_trips_failure_actions() {
    let mgr = Scm::new();
    let id = support::random_test_id();
    let _guard = ServiceGuard::new(&id);
    let spec = DaemonSpec::new(DaemonFields {
        restart: Restart::OnFailure,
        restart_delay: Some(Duration::from_secs(3)),
        ..support::plain_fields(&id, fixture_command(&id, 1, 1, "plain"))
    })
    .unwrap();

    let outcome = mgr.install(&spec, false).expect("install");
    assert!(matches!(outcome, Outcome::Create), "{outcome:?}");

    // A second, unchanged install must read back exactly what was written —
    // proving `read_failure_actions`'s reconstruction agrees with
    // `apply_failure_actions`'s write, not merely that both compile.
    let second = mgr.install(&spec, false).expect("reinstall, unchanged spec");
    assert!(matches!(second, Outcome::UpToDate), "{second:?}");

    let installed = mgr.list().expect("list");
    let found = installed
        .into_iter()
        .find_map(|entry| match entry {
            Installed::Ours { spec: s, .. } if s.id.as_str() == id => Some(s),
            _ => None,
        })
        .unwrap_or_else(|| panic!("{id} did not appear in list"));
    assert_eq!(found.restart, Restart::OnFailure);
    assert_eq!(found.restart_delay, Some(Duration::from_secs(3)));
}

// Ownership::OursUnusable: a blob that decodes but whose account can no longer be resolved ============================

#[skuld::test(requires = [support::elevated], labels = [ELEVATED, SCM_DENIAL], serial = SCM_DENIAL)]
fn deleted_account_makes_the_service_oursunusable() {
    let suffix = format!("{:08x}", rand::random::<u32>());
    let account = format!("gt{suffix}");
    // <=14 chars: longer passwords make `net user ... /add` prompt
    // interactively about pre-Windows-2000 compatibility, which hangs
    // waiting on stdin in CI.
    let password = "Goetia!Test12";
    let user_guard = support::LocalUserGuard::create(&account, password);
    let sid = common::sid_string_for_account(&account);

    let id = support::random_test_id();
    let guard = ServiceGuard::new(&id);

    // `user: {id: <sid>}` — a `User::Name` account never fails to
    // re-resolve (`identity::resolve` does no lookup for it at all), so
    // only the SID path can reach `Ownership::OursUnusable` once the
    // account is gone.
    let output = Command::new(support::current_exe_str())
        .arg(INSTALL_AS)
        .arg(guard.id())
        .arg(&account)
        .arg(&sid)
        .env("GOETIA_SERVICE_PASSWORD", password)
        .output()
        .expect("spawn the install-as-account helper");
    assert!(
        output.status.success(),
        "install as a real account (by SID) failed: stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    // Delete the account out from under the already-installed service, then
    // drop the guard's own cleanup obligation for it (nothing left to
    // delete).
    drop(user_guard);

    let installed = Scm::new().list().expect("list after the account is gone");
    let entry = installed
        .into_iter()
        .find(|entry| match entry {
            Installed::Ours { spec, .. } => spec.id.as_str() == id,
            Installed::OursUnusable { name, .. } => name == &id,
            Installed::Undetermined { name, .. } => name.as_deref() == Some(id.as_str()),
        })
        .unwrap_or_else(|| panic!("{id} disappeared from list entirely instead of becoming OursUnusable"));
    match entry {
        entry @ Installed::OursUnusable { .. } => {
            assert!(!entry.unusable_reason().unwrap_or_default().is_empty());
        }
        Installed::Ours { .. } | Installed::Undetermined { .. } => {
            panic!("expected OursUnusable once the account backing `user.id` is deleted")
        }
    }
}

/// The generator refuses to regenerate a blob that says `restart: always`: an install over it is
/// unusable, not a silent rewrite.
#[skuld::test(requires = [support::elevated], labels = [ELEVATED])]
fn install_refuses_an_always_blob_as_unusable() {
    let mgr = Scm::new();
    let id = support::random_test_id();
    let _guard = ServiceGuard::new(&id);
    let spec = install_plain(&mgr, &id);

    let always = DaemonSpec::new(DaemonFields {
        restart: Restart::Always,
        ..spec.clone().into_fields()
    })
    .unwrap();
    parameters_key(&id, KEY_WRITE)
        .set_value("Spec", &goetia::blob::encode(&always))
        .expect("write Spec");

    let outcome = mgr.preview_install(&spec).expect("preview_install");
    let Outcome::RefuseUnusable { reason, recovery } = &outcome else {
        panic!("expected RefuseUnusable for an always blob, got {outcome:?}");
    };
    let reason = reason.to_string();
    assert!(reason.contains("restart: always"), "{reason}");
    let recovery = recovery.as_deref().expect("a refused blob carries a recovery");
    assert!(
        recovery.contains(&format!("goetia daemon uninstall {id}")),
        "{recovery}"
    );
}

// A read that did not complete: the aggregate, and the two objects that produce it ====================================

/// Whether the aggregate is a privilege artifact or a permanent fixture, asserted rather than
/// assumed: on an elevated run this host must leave nothing undetermined at all, or
/// `goetia daemon list` exits `4` on every Windows machine forever and the code stops meaning
/// anything. That every service's `Parameters` is readable to an Administrator is the *claim*, not
/// the precondition — if some service denies even that, this reddens and the finding is real: the
/// notice's remedy would then not be the whole remedy.
///
/// `no_aggregate_entry_is_emitted_for_a_zero_count` already covers the code-only half (an aggregate
/// emitted unconditionally); what this uniquely pins is the claim about the runner.
#[skuld::test(requires = [support::elevated], labels = [ELEVATED, SCM_DENIAL], serial)]
fn an_elevated_list_leaves_nothing_undetermined() {
    let mgr = Scm::new();
    let id = support::random_test_id();
    let guard = ServiceGuard::new(&id);
    install_plain(&mgr, guard.id());

    let listed = mgr.list().expect("list");

    // Non-vacuous: an elevated reader decodes this service's own marker and lists it, so a run whose
    // reader could in fact read nothing fails here instead of passing on an empty listing.
    assert!(
        listed
            .iter()
            .any(|entry| matches!(entry, Installed::Ours { spec, .. } if spec.id.as_str() == id)),
        "the installed daemon must actually appear: {listed:?}"
    );
    let undetermined: Vec<&Installed> = listed
        .iter()
        .filter(|entry| matches!(entry, Installed::Undetermined { .. }))
        .collect();
    assert!(
        undetermined.is_empty(),
        "an elevated caller was expected to read every service's Parameters; something on this host \
         was not read, which makes `daemon list` exit 4 here for every caller: {undetermined:?}"
    );
}

/// The `Parameters` boundary. A service whose metadata key denies reading is a service whose marker
/// was never read, so goetia established neither that it owns the id nor that it does not — and
/// `Kind::Unusable` would put goetia's name and `uninstall`'s advice on what may be a stranger's
/// service.
#[skuld::test(requires = [support::elevated], labels = [ELEVATED, SCM_DENIAL], serial = SCM_DENIAL)]
fn status_of_a_service_whose_parameters_deny_reading_is_undetermined() {
    let mgr = Scm::new();
    let id = support::random_test_id();
    let guard = ServiceGuard::new(&id);
    let spec = install_plain(&mgr, guard.id());
    mgr.status(&spec.id)
        .expect("status before the denial: the marker reads back fine");

    let _denied = Denied::parameters(&id);

    // The deny ACE really took. Administrators do not bypass an explicit deny, but a test that
    // passed because the ACE never applied would certify nothing at all.
    let blocked = try_parameters_key(&id, KEY_READ)
        .err()
        .unwrap_or_else(|| panic!("the deny ACE did not apply: the Parameters of {id} are still readable"));
    assert_eq!(blocked.kind(), std::io::ErrorKind::PermissionDenied, "{blocked}");

    let err = mgr
        .status(&spec.id)
        .expect_err("a marker that was never read settles nothing about the id");
    let goetia::Error::Undetermined {
        id: reported, recovery, ..
    } = &err
    else {
        panic!("neither ownership nor absence was established, so neither may be claimed: {err:?}");
    };
    assert_eq!(reported, &id);
    assert!(
        recovery.contains("re-run as Administrator"),
        "a denial is the one cause elevation fixes: {recovery}"
    );
}

/// `list` must not fail on a foreign service whose own key denies the unelevated reader: a denied
/// open counts as standing. An explicit `Deny` ACE stops the elevated reader the same way (see
/// `deny.rs`), so this stands in for an unelevated `list`, which this suite has no way to run. A
/// goetia service with the same denial is still listed as its own, which shows that the scan reached it.
#[skuld::test(requires = [support::elevated], labels = [ELEVATED, SCM_DENIAL], serial)]
fn list_reads_a_service_whose_own_key_denies_the_reader_as_standing() {
    #[derive(Clone, Copy, Debug)]
    enum Seed {
        /// No `Parameters` key.
        ForeignBare,
        /// A `Parameters` that holds no marker: readable, and not goetia's.
        ForeignWithParameters,
        Goetias,
    }
    for seed in [Seed::ForeignBare, Seed::ForeignWithParameters, Seed::Goetias] {
        let mgr = Scm::new();
        let id = support::random_test_id();
        let guard = ServiceGuard::new(&id);
        match seed {
            Seed::ForeignBare => seed_foreign(guard.id()),
            Seed::ForeignWithParameters => {
                seed_foreign(guard.id());
                create_parameters_key(&id);
            }
            Seed::Goetias => {
                install_plain(&mgr, guard.id());
            }
        }

        // What the host's `list` says is undetermined before the denial: other services may be
        // unreadable, and two of them aggregate into one entry with no name.
        let undetermined = |listed: &[Installed]| -> Vec<String> {
            listed
                .iter()
                .filter(|entry| matches!(entry, Installed::Undetermined { .. }))
                .map(|entry| format!("{entry:?}"))
                .collect()
        };
        let baseline = undetermined(&mgr.list().expect("list before the denial"));

        let _denied = Denied::service_key(&id);

        // The deny took, as `ERROR_ACCESS_DENIED`: opening the service key to read it fails.
        let blocked = try_service_key(&id, KEY_READ)
            .err()
            .unwrap_or_else(|| panic!("the deny ACE did not apply: `{id}` is still readable"));
        assert_eq!(blocked.raw_os_error(), Some(ERROR_ACCESS_DENIED as i32), "{blocked:?}");

        let listed = mgr
            .list()
            .expect("a service key denied to the reader must not fail `list`");

        // The denial adds nothing undetermined: the same entries as before it, whatever they carry.
        assert_eq!(undetermined(&listed), baseline, "{seed:?}: {listed:?}");
        let ours = listed
            .iter()
            .any(|entry| matches!(entry, Installed::Ours { spec, .. } if spec.id.as_str() == id));
        let named = listed.iter().any(|entry| match entry {
            Installed::OursUnusable { name, .. } => name == &id,
            _ => false,
        });
        match seed {
            Seed::Goetias => assert!(ours, "the scan did not reach a service it must list: {listed:?}"),
            _ => assert!(
                !ours && !named,
                "{seed:?}: a stranger's service was claimed: {listed:?}"
            ),
        }
    }
}

/// The same boundary on the `list` side, and what the entry standing for it is allowed to say. One
/// denied `Parameters` read is one entry that names its service — and carries the *rendered cause*
/// `registry::read_parameters` built: the key, the operation and the Win32 message. `list` used to
/// match `Err(_)` and hand the entry a static sentence instead, which says nothing of a real Win32
/// failure.
#[skuld::test(requires = [support::elevated], labels = [ELEVATED, SCM_DENIAL], serial = SCM_DENIAL)]
fn list_reports_a_denied_parameters_read_with_the_cause_it_established() {
    let mgr = Scm::new();
    let id = support::random_test_id();
    let guard = ServiceGuard::new(&id);
    install_plain(&mgr, guard.id());

    // Non-vacuity: before the denial this service is one of goetia's own, decoded and listed.
    assert!(
        mgr.list()
            .expect("list before the denial")
            .iter()
            .any(|entry| matches!(entry, Installed::Ours { spec, .. } if spec.id.as_str() == id)),
        "the service must be readable before the denial, or the assertion below proves nothing"
    );

    let _denied = Denied::parameters(&id);

    let listed = mgr.list().expect("one denied read must not take down the listing");

    let reason = listed
        .iter()
        .find_map(|entry| match entry {
            Installed::Undetermined { name, reason } if name.as_deref() == Some(id.as_str()) => Some(reason),
            _ => None,
        })
        .unwrap_or_else(|| panic!("a service whose marker was never read is neither claimed nor omitted: {listed:?}"))
        .to_string();
    assert!(
        reason.contains(&support::registry::parameters_path(&id)),
        "the entry must name the key whose read did not complete: {reason}"
    );
    assert!(
        !listed
            .iter()
            .any(|entry| matches!(entry, Installed::Ours { spec, .. } if spec.id.as_str() == id)),
        "the marker went unread, so ownership may not be claimed: {listed:?}"
    );
}

/// A service key under `Services` taken out and replaced by a registry link to a missing key, then
/// put back on drop. SCM keeps the service in its own database, so it still opens by name.
///
/// Declare it after the `ServiceGuard` of the service, so that it is put back before the service is
/// deleted, and before the link guard made by [`create_link`], so that the link is removed first.
struct SwappedServiceKey {
    id: String,
    /// The service's key as it was, under `saved`'s `service`.
    saved: ScratchKey,
}

impl SwappedServiceKey {
    /// Copies `Services\<id>` aside and deletes it. The guard exists before either.
    fn take_out(id: &str, saved: ScratchKey) -> Self {
        let guard = Self {
            id: id.to_string(),
            saved,
        };
        let (copy, _) = guard.saved.create().create_subkey("service").expect("create the copy");
        let services = services_key();
        services.copy_tree(id, &copy).expect("copy the service key aside");
        services.delete_subkey_all(id).expect("delete the service key");
        guard
    }
}

impl Drop for SwappedServiceKey {
    /// Never panics, so a drop that runs while the test unwinds still goes on.
    fn drop(&mut self) {
        let put_back = || -> std::io::Result<()> {
            let (key, _) = try_services_key()?.create_subkey(&self.id)?;
            let saved =
                winreg::RegKey::predef(HKEY_LOCAL_MACHINE).open_subkey_with_flags(self.saved.path(), KEY_READ)?;
            saved.copy_tree("service", &key)
        };
        support::cleanup::warn_left_behind(support::cleanup::retrying(|| {
            put_back()
                .err()
                .map(|e| format!(r"put back the service key of {}: {e}", self.id))
        }));
    }
}

fn try_services_key() -> std::io::Result<winreg::RegKey> {
    winreg::RegKey::predef(HKEY_LOCAL_MACHINE).open_subkey_with_flags(support::SCM_SERVICES_KEY, KEY_READ | KEY_WRITE)
}

fn services_key() -> winreg::RegKey {
    try_services_key().expect("open Services")
}

/// A service key that is a registry link to a missing key is a key SCM has a service at, but nothing
/// below it can be read: `list` reports it in the unreadable aggregate, `status` is `Undetermined`,
/// and neither calls it a stranger's or absent. SCM still opens the service by name, so the
/// `Undetermined` of `status` is the registry read's, not the open's.
///
/// `SCM_DENIAL`: the unreadable service puts an `Undetermined` entry naming it in every concurrent
/// `list()`.
#[skuld::test(requires = [support::elevated], labels = [ELEVATED, SCM_DENIAL], serial = SCM_DENIAL)]
fn a_service_key_that_is_a_link_to_a_missing_key_is_undetermined_for_list_and_status() {
    let mgr = Scm::new();
    let id = support::random_test_id();
    let _guard = ServiceGuard::new(&id);
    seed_foreign(&id);
    let target = ScratchKey::new(&format!("dangling-target-{id}"));
    let _swapped = SwappedServiceKey::take_out(&id, ScratchKey::new(&format!("dangling-saved-{id}")));
    let services = services_key();
    let _link = create_link(&services, &id, &target.nt_path());

    let scm = WinServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT).expect("open SCM");
    scm.open_service(&id, ServiceAccess::QUERY_STATUS | ServiceAccess::QUERY_CONFIG)
        .unwrap_or_else(|e| panic!("SCM must still open a service whose key is a dangling link: {e}"));

    let listed = mgr
        .list()
        .expect("one unreadable service must not take down the listing");
    let reason = listed
        .iter()
        .find_map(|entry| match entry {
            Installed::Undetermined { name, reason } if name.as_deref() == Some(id.as_str()) => Some(reason),
            _ => None,
        })
        .unwrap_or_else(|| panic!("a service whose key cannot be read is neither claimed nor omitted: {listed:?}"))
        .to_string();
    assert!(
        reason.contains(&support::registry::service_path(&id)),
        "the entry must name the service key: {reason}"
    );

    let err = mgr
        .status(&id_of(&id))
        .expect_err("a service whose key cannot be read settles nothing about the id");
    assert!(
        matches!(&err, goetia::Error::Undetermined { id: reported, .. } if reported == &id),
        "{err:?}"
    );
}

/// The service key taken out with nothing in its place, while SCM still opens the service: the
/// registry disagrees with SCM, so `status` is `Undetermined` naming the missing key, neither absent
/// nor foreign. `list` enumerates `Services`, where the key is not, so it has nothing to report for
/// the id: it is not listed.
///
/// `SCM_DENIAL`: `status` here is `Undetermined`, and the service is a stranger to concurrent
/// `list()` assertions.
#[skuld::test(requires = [support::elevated], labels = [ELEVATED, SCM_DENIAL], serial = SCM_DENIAL)]
fn a_service_scm_holds_whose_key_is_gone_is_undetermined_for_status_and_not_listed() {
    let mgr = Scm::new();
    let id = support::random_test_id();
    let _guard = ServiceGuard::new(&id);
    seed_foreign(&id);
    let _swapped = SwappedServiceKey::take_out(&id, ScratchKey::new(&format!("gone-saved-{id}")));

    let scm = WinServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT).expect("open SCM");
    scm.open_service(&id, ServiceAccess::QUERY_STATUS | ServiceAccess::QUERY_CONFIG)
        .unwrap_or_else(|e| panic!("SCM must still open a service whose key is gone: {e}"));

    let err = mgr
        .status(&id_of(&id))
        .expect_err("a service SCM holds whose key is gone is not absent");
    let goetia::Error::Undetermined {
        id: reported, reason, ..
    } = &err
    else {
        panic!("{err:?}");
    };
    assert_eq!(reported, &id);
    let reason = reason.report().to_string();
    assert!(
        reason.contains(&support::registry::service_path(&id)),
        "the reason must name the missing service key: {reason}"
    );

    let listed = mgr.list().expect("list");
    assert!(
        !listed.iter().any(|entry| match entry {
            Installed::Ours { spec, .. } => spec.id.as_str() == id,
            Installed::OursUnusable { name, .. } | Installed::Undetermined { name: Some(name), .. } => name == &id,
            Installed::Undetermined { name: None, .. } => false,
        }),
        "a key that is not under Services is not enumerated: {listed:?}"
    );
}

/// The service-object boundary, which the `Parameters` test cannot reach: every verb but `install`
/// opens the service object first, so a DACL denying `SERVICE_QUERY_CONFIG`/`SERVICE_QUERY_STATUS`
/// stops goetia before it ever looks at the metadata. Fixing `read_parameters` alone would leave
/// this path — and therefore `status` and `install` — still claiming ownership it never proved.
#[skuld::test(requires = [support::elevated], labels = [ELEVATED, SCM_DENIAL], serial = SCM_DENIAL)]
fn status_of_a_service_whose_object_denies_querying_is_undetermined() {
    let mgr = Scm::new();
    let id = support::random_test_id();
    let guard = ServiceGuard::new(&id);
    let spec = install_plain(&mgr, guard.id());

    let _denied = Denied::service_query(&id);

    // The deny ACE really took, and as `ERROR_ACCESS_DENIED` specifically — the code the recovery
    // text is keyed on.
    let scm = WinServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT).expect("open SCM");
    let blocked = scm
        .open_service(&id, ServiceAccess::QUERY_STATUS)
        .err()
        .unwrap_or_else(|| panic!("the deny ACE did not apply: `{id}` is still queryable"));
    assert!(
        matches!(&blocked, windows_service::Error::Winapi(io) if io.raw_os_error() == Some(5)),
        "{blocked:?}"
    );

    let err = mgr
        .status(&spec.id)
        .expect_err("a service object goetia could not open settles nothing about the id");
    let goetia::Error::Undetermined {
        id: reported, recovery, ..
    } = &err
    else {
        panic!("`NotInstalled` would claim absence, `Unusable` ownership; neither was established: {err:?}");
    };
    assert_eq!(reported, &id);
    assert!(
        recovery.contains("re-run as Administrator"),
        "a denial is the one cause elevation fixes: {recovery}"
    );
}

/// The service denies only `DC`, so `read` still proves it is goetia's and the verb's own open
/// fails: exit `1`, not `Undetermined` (exit `4`).
///
/// Writes a real service's DACL, so it runs only on CI. `Denied` drops before `ServiceGuard`,
/// restoring the DACL before the delete.
#[skuld::test(requires = [support::elevated], labels = [ELEVATED])]
fn enable_and_disable_of_a_service_that_denies_changing_its_config_fail_as_the_verb() {
    let mgr = Scm::new();
    let id = support::random_test_id();
    let guard = ServiceGuard::new(&id);
    let spec = install_plain(&mgr, guard.id());

    let _denied = Denied::service_change_config(&id);

    // The deny ACE really took, as `ERROR_ACCESS_DENIED`, and queries still work.
    let scm = WinServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT).expect("open SCM");
    let blocked = scm
        .open_service(&id, ServiceAccess::CHANGE_CONFIG)
        .err()
        .unwrap_or_else(|| panic!("the deny ACE did not apply: `{id}` is still changeable"));
    assert!(
        matches!(&blocked, windows_service::Error::Winapi(io) if io.raw_os_error() == Some(5)),
        "{blocked:?}"
    );
    mgr.status(&spec.id)
        .expect("the deny leaves the read intact, so the id stays classified as goetia's");

    type Verb = fn(&Scm, &Id) -> goetia::Result<()>;
    let verbs: [(&str, Verb); 2] = [("enable", |m, id| m.enable(id)), ("disable", |m, id| m.disable(id))];
    for (verb, run) in verbs {
        let err = run(&mgr, &spec.id).expect_err("a denied open cannot enable or disable");
        assert!(
            !matches!(err.cause(), goetia::Error::Undetermined { .. }),
            "{verb}: ownership was established by the read before the open: {err:?}"
        );
        assert!(!err.is_indeterminate(), "{verb}: exit 1, not 4: {err:?}");
        let report = err.report().to_string();
        assert!(
            report.contains(&format!("`{verb}` was denied access to service `{id}`")),
            "{verb}: {report}"
        );
    }
}

/// As above, for `uninstall`'s own opens of the service object: one that denies `DC` stops the
/// disable that precedes the delete, one that denies `DELETE` stops the delete. Either is exit `1`
/// naming `uninstall` and the service, says it was not deleted, and leaves the service installed.
/// `kill` has no case here: its open needs only `QUERY_STATUS`, which `read` needs too, so a denial
/// there is `read`'s `Undetermined`.
///
/// Writes a real service's DACL, so it runs only on CI.
#[skuld::test(requires = [support::elevated], labels = [ELEVATED])]
fn uninstall_of_a_service_that_denies_a_step_fails_as_the_verb_and_leaves_it_installed() {
    type Case = (&'static str, fn(&str) -> Denied, &'static str);
    let cases: [Case; 2] = [
        (
            "DC",
            Denied::service_change_config,
            "was NOT deleted and nothing about it was changed",
        ),
        (
            "SD",
            Denied::service_delete,
            "is stopped and disabled but was NOT deleted",
        ),
    ];
    for (denied_right, deny, says) in cases {
        let mgr = Scm::new();
        let id = support::random_test_id();
        let guard = ServiceGuard::new(&id);
        let spec = install_plain(&mgr, guard.id());

        let _denied = deny(&id);

        let err = mgr
            .uninstall(&spec.id)
            .expect_err("a denied open cannot disable or delete the service");
        assert!(
            !matches!(err.cause(), goetia::Error::Undetermined { .. }),
            "{denied_right}: ownership was established by the read before the open: {err:?}"
        );
        assert!(!err.is_indeterminate(), "{denied_right}: exit 1, not 4: {err:?}");
        let report = err.report().to_string();
        assert!(
            report.contains(&format!("`uninstall` was denied access to service `{id}`")),
            "{denied_right}: {report}"
        );
        assert!(report.contains(says), "{denied_right}: {report}");
        mgr.status(&spec.id)
            .unwrap_or_else(|e| panic!("{denied_right}: the service must still be installed: {e:?}"));
    }
}
