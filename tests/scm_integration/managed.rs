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
use windows_sys::Wdk::Foundation::OBJECT_ATTRIBUTES;
use windows_sys::Wdk::System::Registry::{KeyFlagsInformation, NtCreateKey, NtQueryKey, NtSetValueKey};
use windows_sys::Win32::Foundation::{OBJ_CASE_INSENSITIVE, OBJ_OPENLINK, UNICODE_STRING};
use winreg::enums::{KEY_READ, KEY_WRITE, RegType};

use crate::common::{self, conformance_mk, fixture_command};
use crate::deny::Denied;
use crate::install_helper::INSTALL_AS;
use crate::support::registry::{parameters_key, service_key, try_parameters_key};
use crate::support::{self, ConnectBack, ELEVATED, ServiceGuard};

/// Held by every test whose assertion is about `list`'s answer for the *whole
/// host* — its exit code, or the absence of any `undetermined` entry — and by
/// every test that denies an elevated reader something under
/// `HKLM\SYSTEM\CurrentControlSet\Services`, which changes that answer. The two
/// groups are the same group precisely because either one invalidates the
/// other, so they take turns rather than race.
///
/// Named for systemd's unit directory rather than for SCM's service tree
/// deliberately: skuld coordinates serialization across processes by label
/// *name*, and `tests/cli_binary.rs::native_backend_answers_list_unelevated`
/// runs a real `daemon list` — on Windows, through this backend — holding that
/// same name.
#[skuld::label]
const UNIT_DIR_EXCLUSIVE: skuld::Label;

fn id_of(s: &str) -> Id {
    Id::try_from(s).expect("random_test_id/short local ids are valid Ids")
}

/// A fresh, installed `type: managed` service at `id`, with no ports wired up.
fn install_plain(mgr: &Scm, id: &str) -> DaemonSpec {
    let spec = DaemonSpec::new(support::spec(id, fixture_command(id, 1, 1, "plain"))).unwrap();
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

/// `UNIT_DIR_EXCLUSIVE`: seeding `UNDETERMINED_ID` denies an elevated reader one service's
/// `Parameters` for the length of the run, which puts an `Undetermined` entry naming that id in
/// every concurrent `list()` (a nameless aggregate if another service is unreadable too) — the
/// very answer this file's host-wide assertions are about.
#[skuld::test(requires = [support::elevated], labels = [ELEVATED, UNIT_DIR_EXCLUSIVE], serial = UNIT_DIR_EXCLUSIVE)]
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

#[skuld::test(requires = [support::elevated], labels = [ELEVATED])]
fn install_then_list_round_trips_without_the_source_file() {
    let mgr = Scm::new();
    let id = support::random_test_id();
    let _guard = ServiceGuard::new(&id);

    let mut env = BTreeMap::new();
    env.insert("GOETIA_TEST_KEY".to_string(), "value".to_string());
    let spec = DaemonSpec::new(DaemonFields {
        env,
        ..support::spec(&id, fixture_command(&id, 1, 1, "plain"))
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
#[skuld::test(requires = [support::elevated], labels = [ELEVATED])]
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
    let spec = DaemonSpec::new(support::spec(&id, command.clone())).unwrap();
    mgr.install(&spec, false).expect("install");
    mgr.enable(&spec.id).expect("enable");
    assert!(mgr.status(&spec.id).expect("status after enable").enabled);

    let changed = DaemonSpec::new(DaemonFields {
        env: BTreeMap::from([("GOETIA_TEST_KEY".to_string(), "value".to_string())]),
        ..support::spec(&id, command)
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
    let spec = DaemonSpec::new(support::spec(&id, fixture_command(&id, 1, 1, "plain"))).unwrap();
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
    let spec = DaemonSpec::new(support::spec(
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
        ..support::spec(id, fixture_command(id, started.port(), stopped.port(), "plain"))
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

#[skuld::test(requires = [support::elevated], labels = [ELEVATED, UNIT_DIR_EXCLUSIVE], serial = UNIT_DIR_EXCLUSIVE)]
fn uninstall_leaves_nothing() {
    let mgr = Scm::new();
    let id = support::random_test_id();
    let _guard = ServiceGuard::new(&id);
    let spec = DaemonSpec::new(support::spec(&id, fixture_command(&id, 1, 1, "plain"))).unwrap();
    mgr.install(&spec, false).expect("install");

    mgr.uninstall(&spec.id).expect("uninstall");

    let query = support::cmd::run("sc.exe", &["query", &id]);
    assert!(!query.ok(), "service should no longer exist:\n{query}");

    let installed = mgr.list().expect("list");
    assert!(
        installed.iter().all(|entry| match entry {
            Installed::Ours { spec: s, .. } => s.id.as_str() != id,
            Installed::OursUnusable { name, .. } => name != &id,
            Installed::Undetermined { name, .. } => match name {
                Some(n) => n != &id,
                // An aggregate may stand for this id, so its absence cannot
                // be concluded: fail rather than certify what list cannot
                // establish. See `Installed::Undetermined`.
                None => false,
            },
        }),
        "uninstalled id must not appear in list"
    );
}

#[skuld::test(requires = [support::elevated], labels = [ELEVATED, UNIT_DIR_EXCLUSIVE], serial = UNIT_DIR_EXCLUSIVE)]
fn list_ignores_foreign_services() {
    let id = support::random_test_id();
    let guard = ServiceGuard::new(&id);
    seed_foreign(guard.id());

    let mgr = Scm::new();
    let installed = mgr.list().expect("list");
    assert!(
        installed.iter().all(|entry| match entry {
            Installed::Ours { spec, .. } => spec.id.as_str() != id,
            Installed::OursUnusable { name, .. } => name != &id,
            Installed::Undetermined { name, .. } => match name {
                Some(n) => n != &id,
                // An aggregate may stand for this id, so its absence cannot
                // be concluded: fail rather than certify what list cannot
                // establish. See `Installed::Undetermined`.
                None => false,
            },
        }),
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
    let spec = DaemonSpec::new(support::spec(
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
    let spec = DaemonSpec::new(support::spec(
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
/// STOP" error at once. The call returns or the CI watchdog ends the job; a wait would hang.
#[skuld::test(requires = [support::elevated], labels = [ELEVATED])]
fn stop_of_a_service_that_accepts_no_stop_says_so_without_waiting() {
    let mgr = Scm::new();
    let id = support::random_test_id();
    let _guard = ServiceGuard::new(&id);
    let started = ConnectBack::listen();
    let spec = DaemonSpec::new(support::spec(
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
    let spec = DaemonSpec::new(support::spec(&id, fixture_command(&id, 1, 1, "plain"))).unwrap();

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
        ..support::spec(&id, fixture_command(&id, started.port(), 1, &format!("env:{VAR}")))
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
        ..support::spec(id, fixture_command(id, 1, 1, "plain"))
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
        ..support::spec(&id, fixture_command(&id, 1, 1, "plain"))
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
/// but never started, so no shim binary is needed.
#[skuld::test(requires = [support::elevated], labels = [ELEVATED])]
fn install_twice_is_up_to_date_for_both_kinds() {
    let mgr = Scm::new();
    for kind in [Kind::Managed, Kind::Simple] {
        let id = support::random_test_id();
        let _guard = ServiceGuard::new(&id);
        let env = BTreeMap::from([("GOETIA_TEST_KEY".to_string(), "value".to_string())]);
        let spec = DaemonSpec::new(DaemonFields {
            kind,
            env,
            restart: Restart::OnFailure,
            restart_delay: Some(Duration::from_secs(3)),
            ..support::spec(&id, fixture_command(&id, 1, 1, "plain"))
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

/// `name` (UTF-16 units, any, a NUL included) as the counted string `ntdll` takes. It points at
/// `name`, which must outlive the call it is passed to.
fn counted(name: &[u16]) -> UNICODE_STRING {
    // A registry name has at most 16 383 units, so its length in bytes fits.
    let bytes = u16::try_from(name.len() * 2).expect("a registry name of at most 16 383 units");
    UNICODE_STRING {
        Length: bytes,
        MaximumLength: bytes,
        Buffer: name.as_ptr().cast_mut(),
    }
}

/// A raw `NtSetValueKey` of `name` (UTF-16 units, any, a NUL included) with `vtype` and `data`.
fn set_raw_value(key: &winreg::RegKey, name: &[u16], vtype: u32, data: &[u8]) {
    let name = counted(name);
    // SAFETY: `key` holds an open handle with write access; `name` points at units that outlive the
    // call, and `data` is `data.len()` readable bytes.
    let status = unsafe {
        NtSetValueKey(
            key.raw_handle(),
            &name,
            0,
            vtype,
            data.as_ptr().cast(),
            data.len() as u32,
        )
    };
    assert!(status >= 0, "write a value of type {vtype}: NTSTATUS {status:#x}");
}

/// A raw `NtCreateKey` of the child `name` (UTF-16 units, any, a NUL included) of `parent`, with
/// `options` (`REG_OPTION_CREATE_LINK` makes a link). The key is returned open.
fn create_raw_key(parent: &winreg::RegKey, name: &[u16], options: u32) -> winreg::RegKey {
    const KEY_ALL_ACCESS: u32 = 0xF003F;
    let name = counted(name);
    let attributes = OBJECT_ATTRIBUTES {
        Length: size_of::<OBJECT_ATTRIBUTES>() as u32,
        RootDirectory: parent.raw_handle(),
        ObjectName: &name,
        Attributes: OBJ_CASE_INSENSITIVE | OBJ_OPENLINK,
        SecurityDescriptor: std::ptr::null(),
        SecurityQualityOfService: std::ptr::null(),
    };
    let mut handle = std::ptr::null_mut();
    let mut disposition = 0u32;
    // SAFETY: `attributes` and the name it points at outlive the call; `handle` and `disposition`
    // are valid out-params; the class is null, which the API allows.
    let status = unsafe {
        NtCreateKey(
            &mut handle,
            KEY_ALL_ACCESS,
            &attributes,
            0,
            std::ptr::null(),
            options,
            &mut disposition,
        )
    };
    assert!(status >= 0, "create a key: NTSTATUS {status:#x}");
    // `winreg` closes a handle it holds on drop, which is all this needs from it.
    winreg::RegKey::predef(handle)
}

/// Whether `key`, opened as itself, is a registry link: `REG_FLAG_LINK` in the key's own flags
/// (`KEY_FLAGS_INFORMATION`: Wow64 flags, key flags, control flags).
fn is_registry_link(key: &winreg::RegKey) -> bool {
    const REG_FLAG_LINK: u32 = 0x0002;
    let mut flags = [0u32; 3];
    let mut written = 0u32;
    // SAFETY: `key` holds an open handle; `flags` is 12 writable bytes; `written` is a valid
    // out-param.
    let status = unsafe {
        NtQueryKey(
            key.raw_handle(),
            KeyFlagsInformation,
            flags.as_mut_ptr().cast(),
            size_of_val(&flags) as u32,
            &mut written,
        )
    };
    assert!(status >= 0, "query a key's flags: NTSTATUS {status:#x}");
    flags[1] & REG_FLAG_LINK != 0
}

/// A key under `HKLM\SOFTWARE` removed on drop, for a registry link to point at.
struct LinkTarget {
    path: String,
}

impl LinkTarget {
    fn create(id: &str) -> (Self, winreg::RegKey) {
        let path = format!(r"SOFTWARE\goetia-link-target-{id}");
        let (key, _) = winreg::RegKey::predef(winreg::enums::HKEY_LOCAL_MACHINE)
            .create_subkey(&path)
            .expect("create the link target");
        (Self { path }, key)
    }

    /// The target as the kernel names it, which is what a link's value holds.
    fn nt_path(&self) -> String {
        format!(r"\Registry\Machine\{}", self.path)
    }
}

impl Drop for LinkTarget {
    fn drop(&mut self) {
        let hklm = winreg::RegKey::predef(winreg::enums::HKEY_LOCAL_MACHINE);
        if let Err(e) = hklm.delete_subkey_all(&self.path)
            && e.kind() != std::io::ErrorKind::NotFound
        {
            eprintln!("LinkTarget: could not remove HKLM\\{}: {e}", self.path);
        }
    }
}

/// `RegDeleteTreeW` follows a registry link: a stray link under `Parameters` would have its target
/// wiped. `--force` deletes the link itself and leaves the target as it was.
#[skuld::test(requires = [support::elevated], labels = [ELEVATED])]
fn force_install_deletes_a_stray_registry_link_and_leaves_its_target() {
    const REG_LINK: u32 = 6;
    const REG_OPTION_CREATE_LINK: u32 = 2;
    let mgr = Scm::new();
    let id = support::random_test_id();
    let _guard = ServiceGuard::new(&id);
    let spec = install_plain(&mgr, &id);
    let (target, target_key) = LinkTarget::create(&id);
    target_key.set_value("Keep", &"x").expect("write Keep");
    target_key.create_subkey("Child").expect("create Child");
    let parameters = parameters_key(&id, KEY_READ | KEY_WRITE);
    let units = |text: &str| -> Vec<u16> { text.encode_utf16().collect() };
    let link = create_raw_key(&parameters, &units("Link"), REG_OPTION_CREATE_LINK);
    let link_value: Vec<u8> = units(&target.nt_path())
        .into_iter()
        .flat_map(u16::to_le_bytes)
        .collect();
    set_raw_value(&link, &units("SymbolicLinkValue"), REG_LINK, &link_value);
    drop(link);

    assert_conflict_cleared_by_force(&mgr, &spec);

    let parameters = parameters_key(&id, KEY_READ);
    let names: Vec<String> = parameters.enum_keys().map(|name| name.expect("enumerate")).collect();
    assert!(names.is_empty(), "the link survived: {names:?}");
    assert_eq!(target_key.get_value::<String, _>("Keep").expect("read Keep"), "x");
    target_key
        .open_subkey("Child")
        .expect("the target's own subkey survived");
}

/// `Parameters` itself may be a registry link. goetia reads it as the link (its target is not
/// consulted) and `--force` deletes it and writes a real key: the target keeps what it held, a
/// `Marker` of its own included.
#[skuld::test(requires = [support::elevated], labels = [ELEVATED])]
fn force_install_replaces_a_parameters_that_is_a_registry_link_and_leaves_its_target() {
    const REG_LINK: u32 = 6;
    const REG_OPTION_CREATE_LINK: u32 = 2;
    let mgr = Scm::new();
    let id = support::random_test_id();
    let _guard = ServiceGuard::new(&id);
    let spec = install_plain(&mgr, &id);
    let units = |text: &str| -> Vec<u16> { text.encode_utf16().collect() };
    let (target, target_key) = LinkTarget::create(&id);
    target_key.set_value("Keep", &"x").expect("write Keep");
    target_key.set_value("Marker", &"not goetia's").expect("write Marker");

    // goetia's four values, copied into the link key itself: the service stays marked, so the read
    // reaches the comparison, which a target holding no marker would not.
    let parameters = parameters_key(&id, KEY_READ | KEY_WRITE);
    let fields: Vec<(String, winreg::RegValue)> = parameters.enum_values().map(|v| v.expect("enumerate")).collect();
    drop(parameters);
    let service = service_key(&id, KEY_READ | KEY_WRITE);
    service.delete_subkey_all("Parameters").expect("remove Parameters");
    let link = create_raw_key(&service, &units("Parameters"), REG_OPTION_CREATE_LINK);
    // No closing NUL, which is how a link's target is stored, so that the link resolves.
    let link_value: Vec<u8> = units(&target.nt_path())
        .into_iter()
        .flat_map(u16::to_le_bytes)
        .collect();
    set_raw_value(&link, &units("SymbolicLinkValue"), REG_LINK, &link_value);
    for (name, value) in fields {
        set_raw_value(&link, &units(&name), value.vtype as u32, &value.bytes);
    }
    drop(link);

    let diff = assert_conflict_cleared_by_force(&mgr, &spec);

    assert!(diff.contains("SymbolicLinkValue"), "{diff}");
    assert_eq!(target_key.get_value::<String, _>("Keep").expect("read Keep"), "x");
    assert_eq!(
        target_key.get_value::<String, _>("Marker").expect("read Marker"),
        "not goetia's"
    );
    // Opened as itself, which `parameters_key` does not: it would follow a link.
    let service = service_key(&id, KEY_READ | KEY_WRITE);
    let parameters = create_raw_key(&service, &units("Parameters"), 0);
    assert!(!is_registry_link(&parameters), "Parameters is still a link");
    let marker: String = parameters
        .get_value("Marker")
        .expect("a real Parameters holds a Marker");
    assert_ne!(marker, "not goetia's", "Parameters holds goetia's Marker");
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
    assert!(diff.contains("D800"), "{diff}");
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

struct LocalUserGuard {
    name: String,
}

impl LocalUserGuard {
    fn create(name: &str, password: &str) -> Self {
        // Built after the add, not before: `/add` is atomic, so on failure nothing
        // was created and a guard would delete an account this test did not make.
        support::cmd::run("net.exe", &["user", name, password, "/add"]).expect_ok();
        Self { name: name.to_string() }
    }
}

impl Drop for LocalUserGuard {
    fn drop(&mut self) {
        // `net.exe` exits 2 for every error; the message names the Win32 one, and 2221 is
        // NERR_UserNotFound ("NET HELPMSG 2221"), the account already gone.
        support::cleanup::warn_left_behind(support::cmd::run_cleanup_unless(
            "net.exe",
            &[&["user", &self.name, "/delete"]],
            |run| run.stderr.contains("2221"),
        ));
    }
}

#[skuld::test(requires = [support::elevated], labels = [ELEVATED])]
fn real_account_gets_service_logon_right() {
    // Windows local (SAM) account names are capped at 20 characters.
    let suffix = format!("{:08x}", rand::random::<u32>());
    let account = format!("gt{suffix}");
    // <=14 chars: longer passwords make `net user ... /add` prompt
    // interactively about pre-Windows-2000 compatibility, which hangs
    // waiting on stdin in CI.
    let password = "Goetia!Test12";
    let _user_guard = LocalUserGuard::create(&account, password);

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
    // what is meant. A start that never arrives is bounded by CI's per-binary watchdog — a failure
    // bound surfaced to a human, not a synchronisation device.
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
        ..support::spec(&id, fixture_command(&id, 1, 1, "plain"))
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
        ..support::spec(&id, fixture_command(&id, 1, 1, "plain"))
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
    let exe = common::WorldReadableExe::new(&id);
    let started = ConnectBack::listen();
    let stopped = ConnectBack::listen();
    let spec = DaemonSpec::new(DaemonFields {
        user: User::Name("LocalService".to_string()),
        ..support::spec(
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
        ..support::spec(&id, fixture_command(&id, 1, 1, "plain"))
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
    let by_root = DaemonSpec::new(support::spec(&root_id, fixture_command(&root_id, 1, 1, "plain"))).unwrap();
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

#[skuld::test(requires = [support::elevated], labels = [ELEVATED])]
fn restart_on_failure_round_trips_failure_actions() {
    let mgr = Scm::new();
    let id = support::random_test_id();
    let _guard = ServiceGuard::new(&id);
    let spec = DaemonSpec::new(DaemonFields {
        restart: Restart::OnFailure,
        restart_delay: Some(Duration::from_secs(3)),
        ..support::spec(&id, fixture_command(&id, 1, 1, "plain"))
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

#[skuld::test(requires = [support::elevated], labels = [ELEVATED])]
fn deleted_account_makes_the_service_oursunusable() {
    let suffix = format!("{:08x}", rand::random::<u32>());
    let account = format!("gt{suffix}");
    // <=14 chars: longer passwords make `net user ... /add` prompt
    // interactively about pre-Windows-2000 compatibility, which hangs
    // waiting on stdin in CI.
    let password = "Goetia!Test12";
    let user_guard = LocalUserGuard::create(&account, password);
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
#[skuld::test(requires = [support::elevated], labels = [ELEVATED, UNIT_DIR_EXCLUSIVE], serial = UNIT_DIR_EXCLUSIVE)]
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
#[skuld::test(requires = [support::elevated], labels = [ELEVATED, UNIT_DIR_EXCLUSIVE], serial = UNIT_DIR_EXCLUSIVE)]
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

/// The same boundary on the `list` side, and what the entry standing for it is allowed to say. One
/// denied `Parameters` read is one entry that names its service — and carries the *rendered cause*
/// `registry::read_parameters` built: the key, the operation and the Win32 message. `list` used to
/// match `Err(_)` and hand the entry a static sentence instead, which says nothing of a real Win32
/// failure.
///
/// `an_elevated_list_leaves_nothing_undetermined` is what makes the count exactly one here: this
/// denial is the only unreadable `Parameters` on the host, so the aggregate takes its named form.
#[skuld::test(requires = [support::elevated], labels = [ELEVATED, UNIT_DIR_EXCLUSIVE], serial = UNIT_DIR_EXCLUSIVE)]
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

/// The service-object boundary, which the `Parameters` test cannot reach: every verb but `install`
/// opens the service object first, so a DACL denying `SERVICE_QUERY_CONFIG`/`SERVICE_QUERY_STATUS`
/// stops goetia before it ever looks at the metadata. Fixing `read_parameters` alone would leave
/// this path — and therefore `status` and `install` — still claiming ownership it never proved.
#[skuld::test(requires = [support::elevated], labels = [ELEVATED, UNIT_DIR_EXCLUSIVE], serial = UNIT_DIR_EXCLUSIVE)]
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
