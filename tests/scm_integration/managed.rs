//! `type: managed` integration tests against a real SCM. See the module doc
//! comment on `goetia::backend::scm::manager` for the five traps these
//! cover, and `fixture.rs` for the daemon process every spec here runs.

use std::collections::BTreeMap;
use std::process::Command;
use std::time::Duration;

use goetia::backend::scm::manager::ScmManager;
use goetia::decide::Outcome;
use goetia::manager::conformance;
use goetia::manager::{Installed, ServiceManager as _, State};
use goetia::spec::{Id, Restart, User};

use crate::common::{self, conformance_mk, fixture_command, mk_spec};
use crate::install_helper::INSTALL_AS;
use crate::support::{self, ConnectBack, ELEVATED, ServiceGuard};

fn id_of(s: &str) -> Id {
    Id::try_from(s).expect("random_test_id/short local ids are valid Ids")
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
/// argument to the command line. Changes `render()`'s `Arguments` line,
/// which the embedded blob cannot explain — the Windows analogue of a
/// unit-file `MemoryMax=8G` a spec-level diff would render invisible.
fn hand_edit(id: &str) {
    let mutated = format!("\"{}\" --hand-edited", support::current_exe_str());
    support::cmd::run("sc.exe", &["config", id, "binPath=", &mutated]).expect_ok();
}

// Step 1: conformance ==================================================================================================

#[skuld::test(requires = [support::elevated], labels = [ELEVATED])]
fn scm_passes_conformance() {
    seed_foreign(conformance::FOREIGN_ID);
    let _foreign_guard = ServiceGuard::new(conformance::FOREIGN_ID);

    let mgr = ScmManager::new();
    mgr.install(&conformance_mk(conformance::HAND_EDITED_ID), false)
        .expect("seed install for the hand-edit scenario");
    hand_edit(conformance::HAND_EDITED_ID);

    conformance::run(&mgr, &conformance_mk);
}

// Step 2: round trip, conflict, start/stop/status, uninstall, list ====================================================

#[skuld::test(requires = [support::elevated], labels = [ELEVATED])]
fn install_then_list_round_trips_without_the_source_file() {
    let mgr = ScmManager::new();
    let id = support::random_test_id();
    let _guard = ServiceGuard::new(&id);

    let mut env = BTreeMap::new();
    env.insert("GOETIA_TEST_KEY".to_string(), "value".to_string());
    let spec = mk_spec(&id, fixture_command(&id, 1, 1, "plain"), env);

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

#[skuld::test(requires = [support::elevated], labels = [ELEVATED])]
fn sc_config_binpath_change_is_detected_as_conflict() {
    let mgr = ScmManager::new();
    let id = support::random_test_id();
    let _guard = ServiceGuard::new(&id);
    let spec = mk_spec(&id, fixture_command(&id, 1, 1, "plain"), BTreeMap::new());
    mgr.install(&spec, false).expect("install");

    hand_edit(&id);

    let outcome = mgr
        .install(&spec, false)
        .expect("install over a hand-edited artifact must not error");
    match outcome {
        Outcome::Conflict { artifact_diff } => assert!(!artifact_diff.is_empty(), "conflict must carry a diff"),
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
    let mgr = ScmManager::new();
    let id = support::random_test_id();
    let _guard = ServiceGuard::new(&id);
    let started = ConnectBack::listen();
    let stopped = ConnectBack::listen();
    let spec = mk_spec(
        &id,
        fixture_command(&id, started.port(), stopped.port(), "plain"),
        BTreeMap::new(),
    );
    mgr.install(&spec, false).expect("install");

    mgr.start(&spec.id).expect("start");
    started.accept("the fixture to report SERVICE_RUNNING");
    let status = mgr.status(&spec.id).expect("status while running");
    assert_eq!(status.state, State::Running);
    assert!(
        status.pid.is_some(),
        "type: managed reports the daemon's own pid, not a shim's"
    );

    mgr.stop(&spec.id).expect("stop");
    stopped.accept("the fixture to handle SERVICE_CONTROL_STOP");
    let status = mgr.status(&spec.id).expect("status after stop");
    assert_eq!(status.state, State::Stopped);
}

#[skuld::test(requires = [support::elevated], labels = [ELEVATED])]
fn uninstall_leaves_nothing() {
    let mgr = ScmManager::new();
    let id = support::random_test_id();
    let spec = mk_spec(&id, fixture_command(&id, 1, 1, "plain"), BTreeMap::new());
    mgr.install(&spec, false).expect("install");

    mgr.uninstall(&spec.id).expect("uninstall");

    // No `ServiceGuard` here on purpose: if `uninstall` genuinely leaves
    // nothing, there is nothing left to guard; if it does not, the
    // assertions below fail with the leftover still in place to inspect.
    let query = support::cmd::run("sc.exe", &["query", &id]);
    assert!(!query.ok(), "service should no longer exist:\n{query}");

    let installed = mgr.list().expect("list");
    assert!(
        installed.iter().all(|entry| match entry {
            Installed::Ours { spec: s, .. } => s.id.as_str() != id,
            Installed::OursUnreadable { name, .. } => name != &id,
        }),
        "uninstalled id must not appear in list"
    );
}

#[skuld::test(requires = [support::elevated], labels = [ELEVATED])]
fn list_ignores_foreign_services() {
    let id = support::random_test_id();
    let guard = ServiceGuard::new(&id);
    seed_foreign(guard.id());

    let mgr = ScmManager::new();
    let installed = mgr.list().expect("list");
    assert!(
        installed.iter().all(|entry| match entry {
            Installed::Ours { spec, .. } => spec.id.as_str() != id,
            Installed::OursUnreadable { name, .. } => name != &id,
        }),
        "a foreign service must never appear in list"
    );
}

// Step 3: uninstall then immediate reinstall ===========================================================================

#[skuld::test(requires = [support::elevated], labels = [ELEVATED])]
fn uninstall_then_immediate_reinstall_succeeds() {
    let mgr = ScmManager::new();
    let id = support::random_test_id();
    let _guard = ServiceGuard::new(&id);
    let started = ConnectBack::listen();
    let stopped = ConnectBack::listen();
    let spec = mk_spec(
        &id,
        fixture_command(&id, started.port(), stopped.port(), "plain"),
        BTreeMap::new(),
    );

    mgr.install(&spec, false).expect("install");
    mgr.start(&spec.id).expect("start");
    started.accept("the fixture to report SERVICE_RUNNING");

    // Uninstalling a *running* service is the real test of trap 3: `delete`
    // must not run until a confirmed stop, or the registry key survives
    // and the immediate reinstall below meets `ERROR_SERVICE_MARKED_FOR_DELETE`.
    mgr.uninstall(&spec.id).expect("uninstall a running service");
    stopped.accept("the fixture to handle SERVICE_CONTROL_STOP as part of uninstall");

    let outcome = mgr.install(&spec, false).expect("immediate reinstall");
    assert!(matches!(outcome, Outcome::Create), "{outcome:?}");
}

// Step 4: uninstall must not proceed if the service will not stop =====================================================

#[skuld::test(requires = [support::elevated], labels = [ELEVATED])]
fn uninstall_errors_when_service_will_not_stop() {
    let mgr = ScmManager::new();
    let id = support::random_test_id();
    let started = ConnectBack::listen();
    // `refuse-stop`: the fixture reports RUNNING with an empty
    // `controls_accepted`, so `ControlService(STOP)` fails *synchronously*
    // with `ERROR_INVALID_SERVICE_CONTROL` — a real Win32 rejection, not a
    // wait that has to time out (this project forbids time-based sync).
    let spec = mk_spec(
        &id,
        fixture_command(&id, started.port(), 1, "refuse-stop"),
        BTreeMap::new(),
    );

    mgr.install(&spec, false).expect("install");
    mgr.start(&spec.id).expect("start");
    started.accept("the fixture to report SERVICE_RUNNING");

    let err = mgr
        .uninstall(&spec.id)
        .expect_err("a service that declines SERVICE_CONTROL_STOP must not be deleted");
    assert!(
        err.to_string().contains("NOT deleted"),
        "error should explain the service was not deleted: {err}"
    );

    // Confirm it is really still there, then clean up without ever asking
    // it to stop (which would fail the exact same way): kill the process
    // directly by pid, then delete the now-dead service entry.
    let status = mgr.status(&spec.id).expect("status: the service must still exist");
    assert_eq!(status.state, State::Running);
    let pid = status.pid.expect("a running service reports a pid");
    support::cmd::run("taskkill.exe", &["/F", "/PID", &pid.to_string()]).expect_ok();
    support::cmd::run("sc.exe", &["delete", &id]).expect_ok();
}

// Step 5: an install interrupted before the metadata write is recoverable =============================================

#[skuld::test(requires = [support::elevated], labels = [ELEVATED])]
fn interrupted_install_is_recoverable() {
    let mgr = ScmManager::new();
    let id = support::random_test_id();
    let guard = ServiceGuard::new(&id);
    let spec = mk_spec(&id, fixture_command(&id, 1, 1, "plain"), BTreeMap::new());

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

// Step 6: env on type: managed (spec §8 must-verify #5) ================================================================

#[skuld::test(requires = [support::elevated], labels = [ELEVATED])]
fn managed_kind_environment_availability() {
    const VAR: &str = "GOETIA_SCM_ENV_PROBE";
    const VALUE: &str = "goetia-managed-env-probe-value";

    let mgr = ScmManager::new();
    let id = support::random_test_id();
    let _guard = ServiceGuard::new(&id);
    let started = ConnectBack::listen();

    let mut env = BTreeMap::new();
    env.insert(VAR.to_string(), VALUE.to_string());
    let spec = mk_spec(&id, fixture_command(&id, started.port(), 1, &format!("env:{VAR}")), env);

    mgr.install(&spec, false).expect("install");
    mgr.start(&spec.id).expect("start");
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

// Step 7: a real account gets SeServiceLogonRight ======================================================================

struct LocalUserGuard {
    name: String,
}

impl LocalUserGuard {
    fn create(name: &str, password: &str) -> Self {
        support::cmd::run("net.exe", &["user", name, password, "/add"]).expect_ok();
        Self { name: name.to_string() }
    }
}

impl Drop for LocalUserGuard {
    fn drop(&mut self) {
        let del = support::cmd::run("net.exe", &["user", &self.name, "/delete"]);
        if !del.ok() {
            eprintln!("LocalUserGuard[{}]: cleanup failed: {del}", self.name);
        }
    }
}

#[skuld::test(requires = [support::elevated], labels = [ELEVATED])]
fn real_account_gets_service_logon_right() {
    // Windows local (SAM) account names are capped at 20 characters.
    let suffix = format!("{:08x}", rand::random::<u32>());
    let account = format!("gt{suffix}");
    let password = "Goetia!Test1234";
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
    let mgr = ScmManager::new();
    let target = id_of(&id);
    mgr.start(&target).unwrap_or_else(|e| {
        panic!("service under a real account failed to start (SeServiceLogonRight likely not granted): {e}")
    });
    let status = mgr.status(&target).expect("status");
    assert_eq!(status.state, State::Running);
    let _ = mgr.stop(&target);
}

// Round-tripping restart: on-failure (the Some(fa) branch of apply_failure_actions/read_failure_actions) ==============

#[skuld::test(requires = [support::elevated], labels = [ELEVATED])]
fn restart_on_failure_round_trips_failure_actions() {
    let mgr = ScmManager::new();
    let id = support::random_test_id();
    let _guard = ServiceGuard::new(&id);
    let spec = common::mk_spec_full(
        &id,
        fixture_command(&id, 1, 1, "plain"),
        BTreeMap::new(),
        User::Root,
        Restart::OnFailure,
        Some(Duration::from_secs(3)),
    );

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

// Ownership::OursUnreadable: a blob that decodes but whose account can no longer be resolved =========================

#[skuld::test(requires = [support::elevated], labels = [ELEVATED])]
fn deleted_account_makes_the_service_oursunreadable() {
    let suffix = format!("{:08x}", rand::random::<u32>());
    let account = format!("gt{suffix}");
    let password = "Goetia!Test1234";
    let user_guard = LocalUserGuard::create(&account, password);
    let sid = common::sid_string_for_account(&account);

    let id = support::random_test_id();
    let guard = ServiceGuard::new(&id);

    // `user: {id: <sid>}` — a `User::Name` account never fails to
    // re-resolve (`identity::resolve` does no lookup for it at all), so
    // only the SID path can reach `Ownership::OursUnreadable` once the
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

    let installed = ScmManager::new().list().expect("list after the account is gone");
    let entry = installed
        .into_iter()
        .find(|entry| match entry {
            Installed::Ours { spec, .. } => spec.id.as_str() == id,
            Installed::OursUnreadable { name, .. } => name == &id,
        })
        .unwrap_or_else(|| panic!("{id} disappeared from list entirely instead of becoming OursUnreadable"));
    match entry {
        Installed::OursUnreadable { reason, .. } => {
            assert!(!reason.is_empty());
        }
        Installed::Ours { .. } => panic!("expected OursUnreadable once the account backing `user.id` is deleted"),
    }
}
