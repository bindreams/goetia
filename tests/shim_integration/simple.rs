//! The `type: simple` conformance suite and round-trip tests, kept here
//! because they need a built shim. See `tests/scm_integration/managed.rs`
//! for the `type: managed` analogues these mirror.

use goetia::decide::Outcome;
use goetia::manager::conformance;
use goetia::manager::{Installed, ServiceManager as _, State};

use crate::common::{conformance_mk, fixture_command, mk_spec};
use crate::deny::Denied;
use crate::support::{self, ELEVATED, ServiceGuard};

/// The same cross-process lock `tests/scm_integration/managed.rs` declares,
/// shared with it by label *name*: both binaries drive the one SCM on the host,
/// and `list`'s aggregate `undetermined` entry is an answer about all of it. A
/// negative assertion here is unsound while any test anywhere denies a
/// `Parameters` read, so they take turns rather than race.
#[skuld::label]
const UNIT_DIR_EXCLUSIVE: skuld::Label;

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
/// argument to the shim's own command line. For `type: simple` this changes
/// `render()`'s `Arguments` line (normally just `[<id>]`) the same way it
/// does for `type: managed`'s own command — the embedded blob cannot
/// explain it either way.
fn hand_edit(id: &str) {
    let shim = std::env::var("GOETIA_SHIM_PATH").expect("set by tests/shim_integration.rs's own main()");
    let mutated = format!("\"{shim}\" {id} --hand-edited");
    support::cmd::run("sc.exe", &["config", id, "binPath=", &mutated]).expect_ok();
}

// Step 1: conformance =================================================================================================

/// `UNIT_DIR_EXCLUSIVE`: seeding `UNDETERMINED_ID` denies an elevated reader one service's
/// `Parameters` for the length of the run, which puts an aggregate entry in every concurrent
/// `list()` — including the ones `tests/scm_integration/managed.rs` asserts are clean.
#[skuld::test(requires = [support::elevated], labels = [ELEVATED, UNIT_DIR_EXCLUSIVE], serial = UNIT_DIR_EXCLUSIVE)]
fn simple_passes_conformance() {
    let mgr = goetia::backend::scm::manager::ScmManager::new();

    seed_foreign(conformance::FOREIGN_ID);
    let _foreign_guard = ServiceGuard::new(conformance::FOREIGN_ID);

    mgr.install(&conformance_mk(conformance::HAND_EDITED_ID), false)
        .expect("seed install for the hand-edit scenario");
    hand_edit(conformance::HAND_EDITED_ID);

    // Seed `UNDETERMINED_ID`, exactly as `tests/scm_integration/managed.rs` does: install
    // normally, then deny reading the one key that carries the marker.
    let _undetermined_guard = ServiceGuard::new(conformance::UNDETERMINED_ID);
    mgr.install(&conformance_mk(conformance::UNDETERMINED_ID), false)
        .expect("seed install for the undetermined scenario");
    let _undetermined_denied = Denied::parameters(conformance::UNDETERMINED_ID);

    conformance::run(&mgr, &conformance_mk);
}

// Step 2: round trip, start/stop/status, uninstall, list ==============================================================

#[skuld::test(requires = [support::elevated], labels = [ELEVATED])]
fn install_then_list_round_trips_without_the_source_file() {
    let mgr = goetia::backend::scm::manager::ScmManager::new();
    let id = support::random_test_id();
    let _guard = ServiceGuard::new(&id);
    let spec = mk_spec(&id, fixture_command("report", &["1"]));

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
    let mgr = goetia::backend::scm::manager::ScmManager::new();
    let id = support::random_test_id();
    let _guard = ServiceGuard::new(&id);
    let started = support::ConnectBack::listen();
    let spec = mk_spec(&id, fixture_command("report", &[&started.port().to_string()]));
    mgr.install(&spec, false).expect("install");

    mgr.start(&spec.id).expect("start");
    started.accept("the fixture (spawned by the shim) to report running");
    let status = mgr.status(&spec.id).expect("status while running");
    assert_eq!(status.state, State::Running);
    let pid = status.pid.expect("type: simple reports the shim's own pid");

    // `Status.pid`'s own doc comment (`src/manager.rs`) promises this is
    // `goetia-shim.exe`'s pid, not the supervised daemon's — SCM knows no
    // other process for this service. Confirm it names the shim, not the
    // fixture.
    let tasklist = support::cmd::run("tasklist.exe", &["/FI", &format!("PID eq {pid}"), "/FO", "CSV", "/NH"]);
    assert!(
        tasklist.ok() && tasklist.stdout.to_lowercase().contains("goetia-shim.exe"),
        "pid {pid} reported by status() is not goetia-shim.exe:\n{tasklist}"
    );

    mgr.stop(&spec.id).expect("stop");
    let status = mgr.status(&spec.id).expect("status after stop");
    assert_eq!(status.state, State::Stopped);
}

#[skuld::test(requires = [support::elevated], labels = [ELEVATED, UNIT_DIR_EXCLUSIVE], serial = UNIT_DIR_EXCLUSIVE)]
fn uninstall_leaves_nothing() {
    let mgr = goetia::backend::scm::manager::ScmManager::new();
    let id = support::random_test_id();
    let spec = mk_spec(&id, fixture_command("report", &["1"]));
    mgr.install(&spec, false).expect("install");

    mgr.uninstall(&spec.id).expect("uninstall");

    // No `ServiceGuard` here on purpose — see
    // `tests/scm_integration/managed.rs`'s identical test for why.
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

    let mgr = goetia::backend::scm::manager::ScmManager::new();
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
