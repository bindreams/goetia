//! The `type: simple` conformance suite and round-trip tests, kept here
//! because they need a built shim. See `tests/scm_integration/managed.rs`
//! for the `type: managed` analogues these mirror.

use std::collections::BTreeMap;
use std::ops::ControlFlow;

use goetia::backend::scm::manager::Scm;
use goetia::backend::scm::wait::{
    Armed, Observed, QueriedState, ScmActor as _, SystemScmActor, Waited, WantState, wait_until,
};
use goetia::decide::Outcome;
use goetia::manager::conformance;
use goetia::manager::{Budget, Installed, ServiceManager as _, State};
use windows_service::service::ServiceAccess;
use windows_service::service_manager::{ServiceManager, ServiceManagerAccess};

use crate::common::{LogDir, ShimGuard, conformance_mk, fixture_command, simple_spec};
use crate::deny::Denied;
use crate::support::{self, ELEVATED};

/// The same cross-process lock `tests/scm_integration/managed.rs` declares,
/// shared with it by label *name*: both binaries drive the one SCM on the host,
/// and one denied `Parameters` changes `list`'s answer for all of it. A
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
/// `Parameters` for the length of the run, which puts an `Undetermined` entry in every concurrent
/// `list()` — including the ones `tests/scm_integration/managed.rs` asserts are clean.
#[skuld::test(requires = [support::elevated], labels = [ELEVATED, UNIT_DIR_EXCLUSIVE], serial = UNIT_DIR_EXCLUSIVE)]
fn simple_passes_conformance() {
    let logs = LogDir::new();
    let mgr = Scm::new();
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
    let mgr = Scm::new();
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
    let mgr = Scm::new();
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
    let mgr = Scm::new();
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

// Step 3: STOP_PENDING through teardown ===============================================================================

/// The shim reports `STOP_PENDING`, accepting no controls, from the moment it takes a stop until it
/// reports `STOPPED`; SCM would otherwise show the service as `RUNNING` throughout the teardown.
/// Fails if the shim stops reporting `STOP_PENDING`, or reports it with controls still accepted.
#[skuld::test(requires = [support::elevated], labels = [ELEVATED])]
fn the_shim_reports_stop_pending_then_stopped() {
    let id = support::random_test_id();
    let logs = LogDir::new();
    let mgr = Scm::new();
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

    // Armed while RUNNING, which is outside the mask, so the first delivery is the stop's own.
    let mut actor = SystemScmActor::open(&id).expect("open the service for notifications");
    let armed = actor
        .arm(WantState::Stopped, Budget::Unbounded.start())
        .expect("arm the STOP_PENDING notification");
    assert!(
        matches!(armed, Armed::Registered(s) if s.state == QueriedState::Running),
        "the arm: {armed:?}"
    );
    service.stop().expect("send stop");
    let observed = actor
        .wait_callback(Budget::Unbounded.start())
        .expect("wait for the first state after a stop");
    assert_eq!(observed, Some(Observed::Pending), "the first state after a stop");
    // The shim reports `SERVICE_ACCEPT_STOP` or nothing, so not accepting STOP is `controls_accepted == 0`.
    assert!(!actor.accepts_stop(), "no control is accepted while stopping");
    // STOP_PENDING and STOPPED both accept nothing, so this read is race-free in either state.
    let status = service.query_status().expect("query the status after a stop");
    assert!(
        status.controls_accepted.is_empty(),
        "no control is accepted while stopping: {:?}",
        status.controls_accepted
    );

    // A settled state other than `STOPPED` fails the test instead of hanging it; `STOP_PENDING` waits.
    let waited = wait_until(
        &mut actor,
        WantState::StoppedOnly,
        WantState::StoppedOnly,
        QueriedState::Stopped,
        Budget::Unbounded.start(),
        |sighting| match sighting.status.state {
            QueriedState::Running | QueriedState::Paused => {
                panic!("waiting for Stopped, the service reached {:?}", sighting.status.state)
            }
            _ => Ok(ControlFlow::Continue(())),
        },
    )
    .expect("wait for STOPPED");
    assert_eq!(waited, Waited::Confirmed, "an unbounded wait for STOPPED expired");
}
