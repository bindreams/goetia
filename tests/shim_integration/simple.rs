//! The `type: simple` conformance suite and round-trip tests, kept here
//! because they need a built shim. See `tests/scm_integration/managed.rs`
//! for the `type: managed` analogues these mirror.

use std::collections::BTreeMap;

use goetia::backend::scm::manager::Scm;
use goetia::decide::Outcome;
use goetia::error::Error;
use goetia::manager::conformance;
use goetia::manager::{Budget, Installed, ServiceManager as _, State};
use goetia::shim::{CheckPermissions, StandaloneShim};
use windows_service::service::ServiceAccess;
use windows_service::service_manager::{ServiceManager, ServiceManagerAccess};
use windows_sys::Win32::System::Services::{
    SERVICE_NOTIFY_STOP_PENDING, SERVICE_NOTIFY_STOPPED, SERVICE_STOP_PENDING, SERVICE_STOPPED,
};

use crate::common::{
    LogDir, ShimGuard, UNIT_DIR_EXCLUSIVE, arm_and_wait, conformance_mk, fixture_command, manager, simple_spec,
};
use crate::deny::Denied;
use crate::support::{self, ELEVATED};

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

/// A hand-edit `sc config` can make without touching `Parameters`: change the display name. The
/// host command is not a candidate: `regenerate` keeps the installed one, so changing it is never a
/// conflict, while `DisplayName` is in the registration's `Display` and the blob cannot explain it.
fn hand_edit(id: &str) {
    let name = format!("{id} (hand-edited)");
    support::cmd::run("sc.exe", &["config", id, "DisplayName=", &name]).expect_ok();
}

// Step 1: conformance =================================================================================================

/// `UNIT_DIR_EXCLUSIVE`: seeding `UNDETERMINED_ID` denies an elevated reader one service's
/// `Parameters` for the length of the run, which puts an `Undetermined` entry in every concurrent
/// `list()` — including the ones `tests/scm_integration/managed.rs` asserts are clean.
#[skuld::test(requires = [support::elevated], labels = [ELEVATED, UNIT_DIR_EXCLUSIVE], serial = UNIT_DIR_EXCLUSIVE)]
fn simple_passes_conformance() {
    let logs = LogDir::new();
    let mgr = manager();
    // A foreign service runs no shim, so it needs no `ShimGuard`.
    let _foreign = support::ServiceGuard::new(conformance::FOREIGN_ID);
    // These guards drop after `run`'s uninstall and remove the pre-decode logs; make each before
    // its id's first write.
    let guards = std::cell::RefCell::new(BTreeMap::new());
    let guard = |id: &str| {
        if id == conformance::FOREIGN_ID {
            return;
        }
        guards
            .borrow_mut()
            .entry(id.to_string())
            .or_insert_with(|| ShimGuard::new(id));
    };
    let mk = |id: &str| {
        guard(id);
        conformance_mk(&logs, id)
    };

    seed_foreign(conformance::FOREIGN_ID);

    mgr.install(&mk(conformance::HAND_EDITED_ID), false)
        .expect("seed install for the hand-edit scenario");
    hand_edit(conformance::HAND_EDITED_ID);

    // Seed `UNDETERMINED_ID`, exactly as `tests/scm_integration/managed.rs` does: install
    // normally, then deny reading the one key that carries the marker.
    mgr.install(&mk(conformance::UNDETERMINED_ID), false)
        .expect("seed install for the undetermined scenario");
    let _undetermined_denied = Denied::parameters(conformance::UNDETERMINED_ID);

    conformance::run(&mgr, &mk);
}

// Step 2: round trip, start/stop/status, uninstall, list ==============================================================

#[skuld::test(requires = [support::elevated], labels = [ELEVATED])]
fn install_then_list_round_trips_without_the_source_file() {
    let id = support::random_test_id();
    let logs = LogDir::new();
    let mgr = manager();
    let _guard = ShimGuard::new(&id);
    let spec = simple_spec(&id, fixture_command("report", &["1"]), &logs);

    let outcome = mgr.install(&spec, false).expect("install");
    assert!(matches!(outcome, Outcome::Create), "{outcome:?}");

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
fn start_stop_status_reflect_reality() {
    let id = support::random_test_id();
    let logs = LogDir::new();
    let mgr = manager();
    let _guard = ShimGuard::new(&id);
    let started = support::ConnectBack::listen();
    let spec = simple_spec(&id, fixture_command("report", &[&started.port().to_string()]), &logs);
    mgr.install(&spec, false).expect("install");

    mgr.start(&spec.id, Budget::DEFAULT).expect("start");
    started.accept("the fixture (spawned by the shim) to report running");
    let status = mgr.status(&spec.id).expect("status while running");
    assert_eq!(status.state, State::Running);
    let pid = status.pid.expect("type: simple reports the shim's own pid");

    // `Status.pid`'s own doc comment (`src/manager.rs`) promises this is
    // `goetia-shim.exe`'s pid, not the daemon's — SCM knows no
    // other process for this service. Confirm it names the shim, not the
    // fixture.
    let tasklist = support::cmd::run("tasklist.exe", &["/FI", &format!("PID eq {pid}"), "/FO", "CSV", "/NH"]);
    assert!(
        tasklist.ok() && tasklist.stdout.to_lowercase().contains("goetia-shim.exe"),
        "pid {pid} reported by status() is not goetia-shim.exe:\n{tasklist}"
    );

    mgr.stop(&spec.id, Budget::DEFAULT).expect("stop");
    let status = mgr.status(&spec.id).expect("status after stop");
    assert_eq!(status.state, State::Stopped);
}

#[skuld::test(requires = [support::elevated], labels = [ELEVATED, UNIT_DIR_EXCLUSIVE], serial = UNIT_DIR_EXCLUSIVE)]
fn uninstall_leaves_nothing() {
    let id = support::random_test_id();
    let logs = LogDir::new();
    let mgr = manager();
    let _guard = ShimGuard::new(&id);
    let spec = simple_spec(&id, fixture_command("report", &["1"]), &logs);
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
    // A foreign service runs no shim, so it needs no `ShimGuard`.
    let guard = support::ServiceGuard::new(&id);
    seed_foreign(guard.id());

    let mgr = manager();
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

// The host the manager registers ======================================================================================

/// A host of another version is refused before anything is registered: the copy of the shim has
/// every occurrence of this crate's version in it replaced by a different one of the same length.
#[skuld::test(requires = [support::elevated], labels = [ELEVATED])]
fn install_refuses_host_of_another_version() {
    let id = support::random_test_id();
    let logs = LogDir::new();
    let dir = support::cleanup::DirGuard::new(tempfile::tempdir().expect("tempdir for the host copy").keep());
    let copy = dir.path().join("goetia-shim.exe");
    let _copy = support::cleanup::FileGuard::new(copy.clone());
    let _guard = ShimGuard::new(&id);

    let version = env!("CARGO_PKG_VERSION");
    let other: String = version
        .chars()
        .map(|c| {
            c.to_digit(10)
                .map_or(c, |d| char::from_digit((d + 1) % 10, 10).unwrap())
        })
        .collect();
    assert_ne!(other, version);
    let mut patched = std::fs::read(env!("CARGO_BIN_EXE_goetia-shim")).expect("read the shim");
    let mut replaced = 0;
    let mut at = 0;
    while at + version.len() <= patched.len() {
        if patched[at..at + version.len()] == *version.as_bytes() {
            patched[at..at + version.len()].copy_from_slice(other.as_bytes());
            replaced += 1;
            at += version.len();
        } else {
            at += 1;
        }
    }
    assert!(replaced > 0, "the shim holds its own version");
    std::fs::write(&copy, patched).expect("write the patched copy");

    let mgr = Scm::with_shim(StandaloneShim::new(copy, vec![]), CheckPermissions::Disable);
    let spec = simple_spec(&id, fixture_command("report", &["1"]), &logs);

    match mgr.install(&spec, false) {
        Err(Error::ShimMarkerVersion { found, expected, .. }) => {
            assert_eq!((found, expected), (other, version.to_string()))
        }
        other => panic!("a host of another version must be refused, got {other:?}"),
    }
    let query = support::cmd::run("sc.exe", &["query", &id]);
    assert!(!query.ok(), "no service may be created:\n{query}");
}

/// A manager with no provider judges a service another provider's host runs: `status` and `list`
/// regenerate from the installed registration, whose host and arguments stay as they were.
#[skuld::test(requires = [support::elevated], labels = [ELEVATED, UNIT_DIR_EXCLUSIVE], serial = UNIT_DIR_EXCLUSIVE)]
fn status_through_new_keeps_a_standalone_host() {
    let id = support::random_test_id();
    let logs = LogDir::new();
    let _guard = ShimGuard::new(&id);
    let with_args = Scm::with_shim(
        StandaloneShim::new(
            std::path::PathBuf::from(env!("CARGO_BIN_EXE_goetia-shim")),
            vec!["--probe".to_string()],
        ),
        CheckPermissions::Disable,
    );
    let spec = simple_spec(&id, fixture_command("report", &["1"]), &logs);
    with_args.install(&spec, false).expect("install");
    let installed = support::cmd::run("sc.exe", &["qc", &id]).expect_ok().stdout;
    assert!(installed.contains("--probe"), "{installed}");

    let bare = Scm::new();
    bare.status(&spec.id)
        .expect("status through a manager with no provider");
    let listed = bare.list().expect("list through a manager with no provider");

    assert!(
        listed
            .iter()
            .any(|entry| matches!(entry, Installed::Ours { spec: s, .. } if s.id.as_str() == id)),
        "{id} must be listed as ours: {listed:?}"
    );
    let after = support::cmd::run("sc.exe", &["qc", &id]).expect_ok().stdout;
    assert_eq!(after, installed, "status and list change nothing");
}

// Step 3: STOP_PENDING through teardown ===============================================================================

/// The shim reports `STOP_PENDING`, accepting no controls, from the moment it takes a stop until it
/// reports `STOPPED`; SCM would otherwise show the service as `RUNNING` throughout the teardown.
/// Fails if the shim stops reporting `STOP_PENDING`, or reports it with controls still accepted.
#[skuld::test(requires = [support::elevated], labels = [ELEVATED])]
fn the_shim_reports_stop_pending_then_stopped() {
    let id = support::random_test_id();
    let logs = LogDir::new();
    let mgr = manager();
    let _guard = ShimGuard::new(&id);
    let started = support::ConnectBack::listen();
    let spec = simple_spec(&id, fixture_command("report", &[&started.port().to_string()]), &logs);
    mgr.install(&spec, false).expect("install");
    mgr.start(&spec.id, Budget::DEFAULT).expect("start");
    started.accept("the fixture (spawned by the shim) to report running");

    let scm = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)
        .expect("connect to the service manager");
    let service = scm
        .open_service(&id, ServiceAccess::QUERY_STATUS | ServiceAccess::STOP)
        .expect("open the service");

    // Armed on RUNNING, before the stop, so the first delivery is the stop's own first state.
    let first = arm_and_wait(&id, SERVICE_NOTIFY_STOP_PENDING | SERVICE_NOTIFY_STOPPED)
        .expect("arm the STOP_PENDING notification");
    service.stop().expect("send stop");
    let (state, controls_accepted) = first();
    assert_eq!(state, SERVICE_STOP_PENDING, "the first state after a stop");
    assert_eq!(controls_accepted, 0, "no control is accepted while stopping");

    let (state, _) = arm_and_wait(&id, SERVICE_NOTIFY_STOPPED).expect("arm the STOPPED notification")();
    assert_eq!(state, SERVICE_STOPPED);
}
