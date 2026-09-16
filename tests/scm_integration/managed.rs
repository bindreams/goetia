//! `type: managed` integration tests against a real SCM. See the module doc
//! comment on `goetia::backend::scm::manager` for the five traps these
//! cover, and `fixture.rs` for the daemon process every spec here runs.

use std::collections::BTreeMap;
use std::process::Command;
use std::time::Duration;

use goetia::backend::scm::manager::ScmManager;
use goetia::decide::Outcome;
use goetia::manager::conformance;
use goetia::manager::{Budget, Installed, ServiceManager as _, State};
use goetia::spec::{AccountId, DaemonSpec, Id, Restart, User};
use windows_service::service::ServiceAccess;
use windows_service::service_manager::{ServiceManager as WinServiceManager, ServiceManagerAccess};
use winreg::RegKey;
use winreg::enums::{HKEY_LOCAL_MACHINE, KEY_READ};

use crate::common::{self, conformance_mk, fixture_command, mk_spec};
use crate::deny::Denied;
use crate::install_helper::INSTALL_AS;
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
fn install_plain(mgr: &ScmManager, id: &str) -> DaemonSpec {
    let spec = mk_spec(id, fixture_command(id, 1, 1, "plain"), BTreeMap::new());
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
/// argument to the command line. Changes `render()`'s `Arguments` line,
/// which the embedded blob cannot explain — the Windows analogue of a
/// unit-file `MemoryMax=8G` a spec-level diff would render invisible.
fn hand_edit(id: &str) {
    let mutated = format!("\"{}\" --hand-edited", support::current_exe_str());
    support::cmd::run("sc.exe", &["config", id, "binPath=", &mutated]).expect_ok();
}

// Step 1: conformance =================================================================================================

/// `UNIT_DIR_EXCLUSIVE`: seeding `UNDETERMINED_ID` denies an elevated reader one service's
/// `Parameters` for the length of the run, which puts an aggregate entry in every concurrent
/// `list()` — the very answer this file's host-wide assertions are about.
#[skuld::test(requires = [support::elevated], labels = [ELEVATED, UNIT_DIR_EXCLUSIVE], serial = UNIT_DIR_EXCLUSIVE)]
fn scm_passes_conformance() {
    seed_foreign(conformance::FOREIGN_ID);
    let _foreign_guard = ServiceGuard::new(conformance::FOREIGN_ID);

    let mgr = ScmManager::new();
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

#[skuld::test(requires = [support::elevated], labels = [ELEVATED, UNIT_DIR_EXCLUSIVE], serial = UNIT_DIR_EXCLUSIVE)]
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

    let mgr = ScmManager::new();
    let installed = mgr.list().expect("list");
    assert!(
        installed.iter().all(|entry| match entry {
            Installed::Ours { spec, .. } => spec.id.as_str() != id,
            Installed::OursUnreadable { name, .. } => name != &id,
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
    mgr.start(&spec.id, Budget::DEFAULT).expect("start");
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
    mgr.start(&spec.id, Budget::DEFAULT).expect("start");
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

// Step 6: env on type: managed (spec §8 must-verify #5) ===============================================================

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

// Step 7: a real account gets SeServiceLogonRight =====================================================================

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
    let mgr = ScmManager::new();
    let target = id_of(&id);
    mgr.start(&target, Budget::DEFAULT).unwrap_or_else(|e| {
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
    let mgr = ScmManager::new();
    let id = support::random_test_id();
    let _guard = ServiceGuard::new(&id);
    let spec = common::mk_spec_as(
        &id,
        fixture_command(&id, 1, 1, "plain"),
        BTreeMap::new(),
        User::Id(AccountId::Sid("S-1-5-19".to_string())),
    );

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
    let mgr = ScmManager::new();
    let id = support::random_test_id();
    let _guard = ServiceGuard::new(&id);
    let spec = common::mk_spec_as(
        &id,
        fixture_command(&id, 1, 1, "plain"),
        BTreeMap::new(),
        User::Name("LocalService".to_string()),
    );

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
    let mgr = ScmManager::new();
    let id = support::random_test_id();
    let _guard = ServiceGuard::new(&id);
    // Held past the `install`/`start`/`stop` calls below and dropped (which
    // deletes the copy) only once nothing further needs to launch it.
    let exe = common::WorldReadableExe::new(&id);
    let started = ConnectBack::listen();
    let stopped = ConnectBack::listen();
    let spec = common::mk_spec_as(
        &id,
        common::fixture_command_with_exe(&exe.path_str(), &id, started.port(), stopped.port(), "plain"),
        BTreeMap::new(),
        User::Name("LocalService".to_string()),
    );

    mgr.install(&spec, false).expect("install as LocalService");
    // The real claim this feature makes: not just that the account string
    // is syntactically acceptable to `CreateServiceW` (`install` already
    // proves that), but that it is an identity Windows can actually launch
    // a process under.
    mgr.start(&spec.id, Budget::DEFAULT).expect("start as LocalService");
    started.accept("the fixture to report SERVICE_RUNNING under LocalService");
    let status = mgr.status(&spec.id).expect("status while running");
    assert_eq!(status.state, State::Running);

    mgr.stop(&spec.id, Budget::DEFAULT).expect("stop");
    stopped.accept("the fixture to handle SERVICE_CONTROL_STOP under LocalService");
}

#[skuld::test(requires = [support::elevated], labels = [ELEVATED])]
fn local_system_by_name_installs_like_root() {
    let mgr = ScmManager::new();
    let id = support::random_test_id();
    let _guard = ServiceGuard::new(&id);
    let by_name = common::mk_spec_as(
        &id,
        fixture_command(&id, 1, 1, "plain"),
        BTreeMap::new(),
        User::Name("LocalSystem".to_string()),
    );
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
    let by_root = mk_spec(&root_id, fixture_command(&root_id, 1, 1, "plain"), BTreeMap::new());
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

    let mgr = ScmManager::new();
    let target = id_of(&id);
    mgr.start(&target, Budget::DEFAULT).unwrap_or_else(|e| {
        panic!("service under LocalService failed to start (a stale password likely reached CreateServiceW): {e}")
    });
    let status = mgr.status(&target).expect("status");
    assert_eq!(status.state, State::Running);
    let _ = mgr.stop(&target, Budget::DEFAULT);
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

// Ownership::OursUnreadable: a blob that decodes but whose account can no longer be resolved ==========================

#[skuld::test(requires = [support::elevated], labels = [ELEVATED])]
fn deleted_account_makes_the_service_oursunreadable() {
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
            Installed::Undetermined { name, .. } => name.as_deref() == Some(id.as_str()),
        })
        .unwrap_or_else(|| panic!("{id} disappeared from list entirely instead of becoming OursUnreadable"));
    match entry {
        Installed::OursUnreadable { reason, .. } => {
            assert!(!reason.is_empty());
        }
        Installed::Ours { .. } | Installed::Undetermined { .. } => {
            panic!("expected OursUnreadable once the account backing `user.id` is deleted")
        }
    }
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
    let mgr = ScmManager::new();
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
/// `Kind::Unreadable` would put goetia's name and `uninstall`'s advice on what may be a stranger's
/// service.
#[skuld::test(requires = [support::elevated], labels = [ELEVATED, UNIT_DIR_EXCLUSIVE], serial = UNIT_DIR_EXCLUSIVE)]
fn status_of_a_service_whose_parameters_deny_reading_is_undetermined() {
    let mgr = ScmManager::new();
    let id = support::random_test_id();
    let guard = ServiceGuard::new(&id);
    let spec = install_plain(&mgr, guard.id());
    mgr.status(&spec.id)
        .expect("status before the denial: the marker reads back fine");

    let _denied = Denied::parameters(&id);

    // The deny ACE really took. Administrators do not bypass an explicit deny, but a test that
    // passed because the ACE never applied would certify nothing at all.
    let path = format!(r"{}\{id}\Parameters", support::SCM_SERVICES_KEY);
    let blocked = RegKey::predef(HKEY_LOCAL_MACHINE)
        .open_subkey_with_flags(&path, KEY_READ)
        .err()
        .unwrap_or_else(|| panic!(r"the deny ACE did not apply: HKLM\{path} is still readable"));
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
/// match `Err(_)` and hand the entry a static sentence instead, which `service_detail`'s own doc
/// comment calls useless for diagnosing a real Win32 failure.
///
/// `an_elevated_list_leaves_nothing_undetermined` is what makes the count exactly one here: this
/// denial is the only unreadable `Parameters` on the host, so the aggregate takes its named form.
#[skuld::test(requires = [support::elevated], labels = [ELEVATED, UNIT_DIR_EXCLUSIVE], serial = UNIT_DIR_EXCLUSIVE)]
fn list_reports_a_denied_parameters_read_with_the_cause_it_established() {
    let mgr = ScmManager::new();
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
        .unwrap_or_else(|| panic!("a service whose marker was never read is neither claimed nor omitted: {listed:?}"));
    assert!(
        reason.contains(&format!(r"{}\{id}\Parameters", support::SCM_SERVICES_KEY)),
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
/// this path — and therefore `status` and `discover` — still claiming ownership it never proved.
#[skuld::test(requires = [support::elevated], labels = [ELEVATED, UNIT_DIR_EXCLUSIVE], serial = UNIT_DIR_EXCLUSIVE)]
fn status_of_a_service_whose_object_denies_querying_is_undetermined() {
    let mgr = ScmManager::new();
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
        panic!("`NotInstalled` would claim absence, `Unreadable` ownership; neither was established: {err:?}");
    };
    assert_eq!(reported, &id);
    assert!(
        recovery.contains("re-run as Administrator"),
        "a denial is the one cause elevation fixes: {recovery}"
    );
}
