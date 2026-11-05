//! Policy assertions runnable against any [`ServiceManager`].
//!
//! [`run`] is the deliverable, not the fake it is first exercised against:
//! every real backend calls it too, from its own elevated integration test, so a real backend is
//! checked against the exact same behavioral contract as the fake. The install policy is not part
//! of what a backend can get wrong: it lives once, in `crate::manager::flow`, which is the only
//! caller of [`crate::decide::decide`].
//!
//! ## Cleanup
//!
//! `run` installs several services over the course of its scenarios, which on a
//! real backend are real units, plists or SCM services on the host running it.
//! It tracks every id it installs and uninstalls each before returning, via an
//! RAII guard so a mid-scenario assertion failure still cleans up. Ids are
//! process- and call-unique (a pid plus a monotonic counter), not
//! cryptographically random: sufficient given cleanup runs on normal return and
//! on panic. The guard knows each fresh id before `install` runs, so an
//! `install` that fails after writing is still cleaned up; the seeded ids the
//! caller installs before `run` are the caller's to guard. Cleanup does not run
//! if the process is killed or aborts, which leaves a straggler to remove by
//! hand.
//!
//! ## The seeded ids
//!
//! Four of the scenarios ([`refuses_foreign_even_with_force`],
//! [`foreign_refuses_every_verb`], [`conflict_requires_force`] and
//! [`an_unclassifiable_id_is_never_silently_absent`]) need state that cannot be
//! produced through [`ServiceManager`]'s own methods: a foreign (unmarked)
//! service, a hand-edited Goetia artifact, and an artifact no read can obtain.
//! The **caller** must have put `mgr` into that state at three fixed, reserved
//! ids before calling `run`:
//!
//! - [`FOREIGN_ID`]: something is installed at this id entirely outside
//!   Goetia (a hand-written unit file / plist / registry key carrying no
//!   Goetia marker). `run` never writes to or removes it; cleanup is the
//!   caller's.
//! - [`HAND_EDITED_ID`]: `mk(HAND_EDITED_ID)` has been installed through
//!   `mgr.install()`, and the artifact then mutated outside Goetia so it no
//!   longer matches its own embedded spec (e.g. an extra directive).
//!   `conflict_requires_force` forces an overwrite here, so `run`'s cleanup
//!   includes it; the caller seeds it once per call to `run`.
//! - [`UNDETERMINED_ID`]: an artifact whose bytes this process cannot obtain
//!   is at this id, for [`an_unclassifiable_id_is_never_silently_absent`].
//!   That scenario asserts every verb *refuses* the id, so `run` never writes
//!   to or removes it; cleanup is the caller's. The per-platform seed is in
//!   that scenario's doc comment.
//!
//! [`fake::Fake`] exposes `seed_foreign`/`install_then_hand_edit`/`seed_opaque`
//! for this; a real backend's integration test uses direct filesystem or
//! registry access.
//!
//! [`ServiceManager`]: super::ServiceManager
//! [`fake::Fake`]: super::fake::Fake

use std::sync::atomic::{AtomicU64, Ordering};

use super::{Budget, Installed, ServiceManager, State};
use crate::error::Error;
use crate::spec::{DaemonSpec, Id};

/// See the module doc comment. Reserved: no other scenario uses this id.
pub const FOREIGN_ID: &str = "goetia-conformance-foreign";

/// See the module doc comment. Reserved: no other scenario uses this id.
pub const HAND_EDITED_ID: &str = "goetia-conformance-hand-edited";

/// See [`an_unclassifiable_id_is_never_silently_absent`], the one scenario
/// that reads this id. Reserved: no other scenario uses it.
pub const UNDETERMINED_ID: &str = "goetia-conformance-undetermined";

/// RAII cleanup for every id `run`'s scenarios install. `Drop` cannot return
/// a `Result`, so an uninstall failure is logged to stderr: silence would
/// surface later only as a mysterious straggler service.
struct Cleanup<'a> {
    mgr: &'a dyn ServiceManager,
    ids: Vec<Id>,
}

impl Drop for Cleanup<'_> {
    fn drop(&mut self) {
        for id in &self.ids {
            if let Err(e) = self.mgr.uninstall(id) {
                eprintln!("manager::conformance cleanup: uninstall({id}) failed: {e}");
            }
        }
    }
}

/// Run every policy assertion in this module against `mgr`.
///
/// `mk(id)` must build a valid, installable [`DaemonSpec`] with `id` as its
/// id. Every id `run` uses is freshly generated or one of
/// [`FOREIGN_ID`]/[`HAND_EDITED_ID`]/[`UNDETERMINED_ID`]; the module doc
/// comment says what the caller must have arranged at those three.
pub fn run(mgr: &dyn ServiceManager, mk: &dyn Fn(&str) -> DaemonSpec) {
    let mut cleanup = Cleanup { mgr, ids: Vec::new() };
    // Registered up front, not inside `conflict_requires_force`: the caller has
    // already installed something here, so it needs cleanup even if a scenario
    // that runs earlier panics.
    cleanup
        .ids
        .push(Id::try_from(HAND_EDITED_ID).expect("HAND_EDITED_ID is a valid Id by construction"));

    install_is_idempotent(mgr, mk, &mut cleanup.ids);
    install_does_not_start(mgr, mk, &mut cleanup.ids);
    install_does_not_enable(mgr, mk, &mut cleanup.ids);
    reinstall_preserves_enablement(mgr, mk, &mut cleanup.ids);
    start_and_stop_are_idempotent(mgr, mk, &mut cleanup.ids);
    starting_with_no_budget_is_accepted_and_establishes_nothing(mgr, mk, &mut cleanup.ids);
    a_start_request_after_a_confirmed_stop_is_accepted(mgr, mk, &mut cleanup.ids);
    kill_of_a_service_that_is_not_running_says_so(mgr, mk, &mut cleanup.ids);
    list_and_status_agree_on_pid(mgr, mk, &mut cleanup.ids);
    refuses_foreign_even_with_force(mgr, mk);
    foreign_refuses_every_verb(mgr, mk);
    conflict_requires_force(mgr, mk);
    an_unclassifiable_id_is_never_silently_absent(mgr, mk);

    // `cleanup` drops here, and on unwinding from a failed assertion.
}

/// This module is compiled into the library (not `#[cfg(test)]`-gated) so that
/// each real backend's integration test, a separate crate linking only
/// `goetia`'s public API, can call [`run`]. That rules out a dev-only
/// randomness crate; a process-wide counter plus the pid is unique enough,
/// since callers clean up what they create on normal return and on panic.
static COUNTER: AtomicU64 = AtomicU64::new(0);

pub(crate) fn fresh_id(label: &str) -> String {
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("goetia-conformance-{label}-{pid:x}-{n:x}", pid = std::process::id())
}

/// A spec with a fresh id, registered in `cleanup` before it is returned, so an
/// `install` that fails after writing is still uninstalled.
fn fresh_spec(mk: &dyn Fn(&str) -> DaemonSpec, label: &str, cleanup: &mut Vec<Id>) -> DaemonSpec {
    let spec = mk(&fresh_id(label));
    cleanup.push(spec.id.clone());
    spec
}

fn install_is_idempotent(mgr: &dyn ServiceManager, mk: &dyn Fn(&str) -> DaemonSpec, cleanup: &mut Vec<Id>) {
    let spec = fresh_spec(mk, "idempotent", cleanup);

    let first = mgr.install(&spec, false).expect("first install");
    assert!(
        matches!(first, crate::decide::Outcome::Create),
        "a fresh id must Create, got {first:?}"
    );

    let second = mgr.install(&spec, false).expect("second install, identical spec");
    assert!(
        matches!(second, crate::decide::Outcome::UpToDate),
        "installing the same spec twice must be a no-op the second time, got {second:?}"
    );
}

fn install_does_not_start(mgr: &dyn ServiceManager, mk: &dyn Fn(&str) -> DaemonSpec, cleanup: &mut Vec<Id>) {
    let spec = fresh_spec(mk, "no-start", cleanup);
    mgr.install(&spec, false).expect("install");

    let status = mgr.status(&spec.id).expect("status of a just-installed service");
    assert_ne!(
        status.state,
        State::Running,
        "install must not start the service (id {})",
        spec.id
    );
}

fn install_does_not_enable(mgr: &dyn ServiceManager, mk: &dyn Fn(&str) -> DaemonSpec, cleanup: &mut Vec<Id>) {
    let spec = fresh_spec(mk, "no-enable", cleanup);
    mgr.install(&spec, false).expect("install");

    let status = mgr.status(&spec.id).expect("status of a just-installed service");
    assert!(
        !status.enabled,
        "install must not enable the service at boot (id {})",
        spec.id
    );
}

fn reinstall_preserves_enablement(mgr: &dyn ServiceManager, mk: &dyn Fn(&str) -> DaemonSpec, cleanup: &mut Vec<Id>) {
    let spec = fresh_spec(mk, "preserve-enable", cleanup);
    mgr.install(&spec, false).expect("install");
    mgr.enable(&spec.id).expect("enable");
    assert!(
        mgr.status(&spec.id).expect("status after enable").enabled,
        "enable must actually enable (id {})",
        spec.id
    );

    // A routine config update must not undo a deliberate enable (or disable).
    let mut changed = mk(spec.id.as_str());
    changed
        .env
        .insert("GOETIA_CONFORMANCE_CHANGED".to_string(), "1".to_string());
    let outcome = mgr.install(&changed, false).expect("reinstall over a changed spec");
    assert!(
        matches!(outcome, crate::decide::Outcome::Update { .. }),
        "a changed spec over an unmodified artifact must Update, got {outcome:?}"
    );

    assert!(
        mgr.status(&spec.id).expect("status after reinstall").enabled,
        "reinstall must not change boot-enablement (id {})",
        spec.id
    );
}

/// The caller must have put non-Goetia content at [`FOREIGN_ID`] (see the
/// module doc comment). Never writes, so nothing here needs cleanup.
fn refuses_foreign_even_with_force(mgr: &dyn ServiceManager, mk: &dyn Fn(&str) -> DaemonSpec) {
    let spec = mk(FOREIGN_ID);

    for force in [false, true] {
        let outcome = mgr
            .install(&spec, force)
            .unwrap_or_else(|e| panic!("install over a foreign id must not error, got {e}"));
        match outcome {
            crate::decide::Outcome::RefuseForeign { recovery } => {
                assert!(!recovery.is_empty(), "recovery must be non-empty (force={force})");
            }
            other => panic!("expected RefuseForeign regardless of force={force}, got {other:?}"),
        }
    }
}

/// "Goetia never touches a service it did not create" is not an `install`-only
/// rule: every other verb must refuse a foreign id too, or a backend could act
/// on a stranger's service that merely shares an id. The caller must have
/// arranged [`FOREIGN_ID`] (see the module doc comment).
fn foreign_refuses_every_verb(mgr: &dyn ServiceManager, mk: &dyn Fn(&str) -> DaemonSpec) {
    let spec = mk(FOREIGN_ID);

    assert!(mgr.uninstall(&spec.id).is_err(), "uninstall must refuse a foreign id");
    assert!(mgr.enable(&spec.id).is_err(), "enable must refuse a foreign id");
    assert!(mgr.disable(&spec.id).is_err(), "disable must refuse a foreign id");
    assert!(
        mgr.start(&spec.id, Budget::DEFAULT).is_err(),
        "start must refuse a foreign id"
    );
    assert!(
        mgr.stop(&spec.id, Budget::DEFAULT).is_err(),
        "stop must refuse a foreign id"
    );
    assert!(
        mgr.request_start_after_stop(&spec.id).is_err(),
        "request_start_after_stop must refuse a foreign id"
    );
    let killed = mgr.kill(&spec.id);
    assert!(
        matches!(killed.as_ref().err().map(Error::cause), Some(Error::Foreign { id, .. }) if id == spec.id.as_str()),
        "kill must refuse a foreign id as Foreign, not as NotRunning or anything else: {killed:?}"
    );
    assert!(
        mgr.request_restart(&spec.id).is_none_or(|r| r.is_err()),
        "request_restart must refuse a foreign id"
    );
    assert!(mgr.status(&spec.id).is_err(), "status must refuse a foreign id");
}

/// The caller must have installed `mk(HAND_EDITED_ID)` and hand-edited the
/// artifact (see the module doc comment). `run` registers its cleanup up front.
fn conflict_requires_force(mgr: &dyn ServiceManager, mk: &dyn Fn(&str) -> DaemonSpec) {
    let spec = mk(HAND_EDITED_ID);

    let outcome = mgr
        .install(&spec, false)
        .expect("install over a hand-edited artifact must not error");
    match outcome {
        crate::decide::Outcome::Conflict { artifact_diff, .. } => {
            assert!(!artifact_diff.is_empty(), "conflict must carry a non-empty diff");
        }
        other => panic!("expected Conflict without force, got {other:?}"),
    }

    let forced = mgr
        .install(&spec, true)
        .expect("install with force over a hand-edited artifact must not error");
    assert!(
        !matches!(forced, crate::decide::Outcome::Conflict { .. }),
        "force must resolve the conflict, got {forced:?}"
    );

    // The hand-edit is really gone: the same spec again, without force, is a no-op.
    let after = mgr.install(&spec, false).expect("install after force");
    assert!(
        matches!(after, crate::decide::Outcome::UpToDate),
        "force must actually overwrite the hand-edit, got {after:?}"
    );
}

/// `daemon restart` depends on `stop` succeeding on a service that was never
/// started, and real managers differ by default (see `ServiceManager::stop`).
/// Every backend must paper over that, or `restart` of an installed but
/// never-started daemon works on Linux and errors on macOS and Windows.
fn start_and_stop_are_idempotent(mgr: &dyn ServiceManager, mk: &dyn Fn(&str) -> DaemonSpec, cleanup: &mut Vec<Id>) {
    let spec = fresh_spec(mk, "idempotent-start-stop", cleanup);
    mgr.install(&spec, false).expect("install");

    mgr.stop(&spec.id, Budget::DEFAULT)
        .expect("stop on a never-started service must be Ok, not an error");
    mgr.start(&spec.id, Budget::DEFAULT).expect("start");
    assert_eq!(
        mgr.status(&spec.id).expect("status after start").state,
        State::Running,
        "a start under a budget returns only once the service manager reports the service \
         running, so this ONE status() read must already say so — no retry and no second chance, \
         which is the whole difference the wait makes (id {})",
        spec.id
    );

    mgr.start(&spec.id, Budget::DEFAULT)
        .expect("start on an already-running service must be Ok, not an error");
    mgr.stop(&spec.id, Budget::DEFAULT).expect("stop");
    assert_ne!(
        mgr.status(&spec.id).expect("status after stop").state,
        State::Running,
        "a stop under a budget mirrors it: one status() read, already settled (id {})",
        spec.id
    );

    mgr.stop(&spec.id, Budget::DEFAULT)
        .expect("stop on an already-stopped service must be Ok, not an error");
}

/// `kill` of a service with no process answers [`Error::NotRunning`] on every backend, both when
/// never started and when started then stopped.
fn kill_of_a_service_that_is_not_running_says_so(
    mgr: &dyn ServiceManager,
    mk: &dyn Fn(&str) -> DaemonSpec,
    cleanup: &mut Vec<Id>,
) {
    let spec = fresh_spec(mk, "kill-not-running", cleanup);
    mgr.install(&spec, false).expect("install");

    let never_started = mgr.kill(&spec.id);
    assert!(
        matches!(never_started.as_ref().err().map(Error::cause), Some(Error::NotRunning { id }) if id == spec.id.as_str()),
        "kill of a never-started service must be NotRunning, got {never_started:?}"
    );

    mgr.start(&spec.id, Budget::DEFAULT).expect("start");
    mgr.stop(&spec.id, Budget::DEFAULT).expect("stop");
    let stopped = mgr.kill(&spec.id);
    assert!(
        matches!(stopped.as_ref().err().map(Error::cause), Some(Error::NotRunning { id }) if id == spec.id.as_str()),
        "kill of a stopped service must be NotRunning, got {stopped:?}"
    );
}

/// `Budget::Immediate` issues the request and returns, establishing nothing,
/// so **no assertion about the resulting state follows it here**: the service
/// may be anywhere between not yet forked and already running, and asserting
/// either would assert a race.
///
/// What is asserted: every backend *accepts* the budget, and a subsequent
/// `Budget::DEFAULT` start still reaches `Running`, which catches an
/// `Immediate` that leaves the service unstartable. It does **not** catch an
/// `Immediate` that is a no-op, and nothing portable can: where a sent request
/// shows up differs per manager. The systemd suite asserts it where that is
/// race-free (`a_start_with_no_budget_reaches_systemd` and its stop mirror).
///
/// **No scenario here asserts the expiry rule** (an expiry is
/// `Error::WaitTimeout`, never `Ok(())`) against a real backend. That would
/// need a daemon that provably fails to reach the awaited state on all three
/// platforms, which no portable `DaemonSpec` can express; a real daemon under
/// a budget small enough to expire would assert whichever outcome the
/// scheduler picked. `Fake` carries the rule (`fake_tests.rs`), and the
/// systemd suite adds a stop that cannot complete
/// (`a_stop_that_can_never_complete_times_out_with_the_job_standing`); on
/// launchd and SCM a backend that swallowed an expiry as `Ok` would still pass
/// every scenario.
fn starting_with_no_budget_is_accepted_and_establishes_nothing(
    mgr: &dyn ServiceManager,
    mk: &dyn Fn(&str) -> DaemonSpec,
    cleanup: &mut Vec<Id>,
) {
    let spec = fresh_spec(mk, "no-budget-start", cleanup);
    mgr.install(&spec, false).expect("install");

    mgr.start(&spec.id, Budget::Immediate)
        .expect("a start with no budget issues the request and returns Ok");

    mgr.start(&spec.id, Budget::DEFAULT)
        .expect("a start that waits must still reach the service manager");
    assert_eq!(
        mgr.status(&spec.id).expect("status after a start that waited").state,
        State::Running,
        "id {}",
        spec.id
    );
}

/// `request_start_after_stop` refuses a service that still answers "already
/// running", and nothing portable can hold every backend to *when* it says
/// that. What every backend must do is accept the start once the stop before
/// it is confirmed — a refusal there would fail every `restart --timeout 0`
/// on a backend without `request_restart` — and leave the service startable.
/// As above, nothing is asserted about the state the request leaves. systemd's
/// `restart --timeout 0` is `request_restart` instead (see
/// `a_restart_with_no_budget_restarts_behind_a_stop_that_has_to_wait`).
fn a_start_request_after_a_confirmed_stop_is_accepted(
    mgr: &dyn ServiceManager,
    mk: &dyn Fn(&str) -> DaemonSpec,
    cleanup: &mut Vec<Id>,
) {
    let spec = fresh_spec(mk, "start-after-stop", cleanup);
    mgr.install(&spec, false).expect("install");
    mgr.start(&spec.id, Budget::DEFAULT).expect("start");
    mgr.stop(&spec.id, Budget::DEFAULT).expect("stop");

    mgr.request_start_after_stop(&spec.id)
        .expect("a start request over a confirmed stop must be accepted");

    mgr.start(&spec.id, Budget::DEFAULT)
        .expect("a start that waits must still reach the service manager");
    assert_eq!(
        mgr.status(&spec.id).expect("status after a start that waited").state,
        State::Running,
        "id {}",
        spec.id
    );
}

/// `pid` must mean the same thing in `list` as in `status` — what that
/// backend's own `status()` reports for this id at this moment — on every
/// real backend. Checked while stopped, running, and stopped again, so a
/// `list` that caches a value from install time still fails.
fn list_and_status_agree_on_pid(mgr: &dyn ServiceManager, mk: &dyn Fn(&str) -> DaemonSpec, cleanup: &mut Vec<Id>) {
    let spec = fresh_spec(mk, "pid-agreement", cleanup);
    mgr.install(&spec, false).expect("install");

    assert_list_is_complete_and_pid_agrees(mgr, &spec.id, cleanup);

    mgr.start(&spec.id, Budget::DEFAULT).expect("start");
    assert_eq!(
        mgr.status(&spec.id).expect("status after start").state,
        State::Running,
        "id {}",
        spec.id
    );
    assert_list_is_complete_and_pid_agrees(mgr, &spec.id, cleanup);

    mgr.stop(&spec.id, Budget::DEFAULT).expect("stop");
    assert_list_is_complete_and_pid_agrees(mgr, &spec.id, cleanup);
}

/// The silent-skip regression [`Installed::Undetermined`] exists to prevent:
/// an id goetia could not classify is reported as unclassified — never dropped
/// from the listing, never claimed as goetia's — and no verb acts on it.
///
/// The caller must have put an artifact whose bytes this process cannot
/// obtain at [`UNDETERMINED_ID`]. This asserts that every verb refuses it,
/// `install --force` included, so it registers no cleanup of its own; the
/// caller's cleanup covers a backend that writes anyway.
///
/// # The state, and why every caller can seed it
///
/// A read that *did not complete*, not one that was denied: elevation
/// dissolves a denial (root bypasses a file mode), but an errno that is not
/// about permission dissolves for nobody. Seeds:
///
/// - **systemd** — a regular file where `<id>.service.d` belongs; `read_dir`
///   answers `ENOTDIR`, leaving `residue` unable to say whether anything
///   occupies the id.
/// - **launchd** — a self-referential symlink at the plist path; `stat`
///   answers `ELOOP`, so `obtain` gets no bytes while `symlink_metadata` still
///   sees the entry.
/// - **SCM** — a `Deny`/`ReadKey` ACE over `Parameters`, which stops an
///   Administrator too (`tests/scm_integration/deny.rs`).
///
/// A seed whose *bytes arrive* is not this state: a non-UTF-8 fragment or
/// plist is `Foreign` (established, omitted from `list`, refused with a
/// different error), so seeding one asserts the wrong contract.
///
/// Seeding [`UNDETERMINED_ID`] changes `list`'s answer for the whole host, not
/// only for that id. A caller whose suite also asserts a *clean* host-wide
/// listing has to serialize the two against each other: the integration suites
/// share `UNIT_DIR_EXCLUSIVE` by name, and each such test both carries it in
/// `labels` and declares `serial = UNIT_DIR_EXCLUSIVE`.
///
/// A backend satisfies the `list` half by naming the id in an
/// [`Installed::Undetermined`] entry *or* by emitting any aggregate one. The
/// aggregate counts because a null name stands for ids the entry could not
/// separate, so it may stand for this one, the same rule that forbids
/// concluding absence from it. SCM names a single unreadable service, and emits
/// the nameless form when more than one is unreadable (one entry cannot carry
/// their names) or its scan could not finish. The per-id half below is exact on
/// every backend, so accepting the aggregate does not weaken the scenario to
/// nothing.
pub fn an_unclassifiable_id_is_never_silently_absent(mgr: &dyn ServiceManager, mk: &dyn Fn(&str) -> DaemonSpec) {
    let spec = mk(UNDETERMINED_ID);
    let id = &spec.id;

    let listed = mgr
        .list()
        .expect("one id goetia cannot classify must not take the whole listing down");
    assert!(
        listed.iter().any(|entry| match entry {
            Installed::Undetermined { name, .. } => name.is_none() || name.as_deref() == Some(id.as_str()),
            Installed::Ours { .. } | Installed::OursUnusable { .. } => false,
        }),
        "id {id} is neither reported undetermined nor covered by an aggregate entry, so list() \
         silently says it does not exist: {listed:?}"
    );
    assert!(
        !listed.iter().any(|entry| match entry {
            Installed::Ours { spec, .. } => spec.id == *id,
            Installed::OursUnusable { name, .. } => name == id.as_str(),
            Installed::Undetermined { .. } => false,
        }),
        "the read that would have classified id {id} never completed, so no ownership was \
         established and none may be claimed: {listed:?}"
    );

    // `NotInstalled` would certify absence and `Foreign` a stranger's presence;
    // this read established neither.
    for (verb, result) in [
        ("status", mgr.status(id).map(drop)),
        ("uninstall", mgr.uninstall(id)),
        ("enable", mgr.enable(id)),
        ("disable", mgr.disable(id)),
        ("start", mgr.start(id, Budget::DEFAULT)),
        ("stop", mgr.stop(id, Budget::DEFAULT)),
        ("kill", mgr.kill(id)),
    ]
    .into_iter()
    .chain(mgr.request_restart(id).map(|result| ("request_restart", result)))
    {
        assert!(
            matches!(
                result.as_ref().err().map(Error::cause),
                Some(Error::Undetermined { .. })
            ),
            "{verb} on an id goetia cannot classify must be Undetermined, got {result:?}"
        );
    }

    // The verbs that *write*, `--force` included: refusing to clobber what
    // goetia proved is a stranger's is worth little if it clobbers what it could
    // not read. `force` must not be consulted: no classified artifact exists
    // for it to override.
    for (verb, result) in [
        ("install", mgr.install(&spec, false).map(drop)),
        ("install with force", mgr.install(&spec, true).map(drop)),
        ("preview_install", mgr.preview_install(&spec).map(drop)),
    ] {
        assert!(
            matches!(
                result.as_ref().err().map(Error::cause),
                Some(Error::Undetermined { .. })
            ),
            "{verb} over an id goetia cannot classify must be Undetermined, got {result:?}"
        );
    }
}

/// Two assertions off one `list()`, which is why they share a function: SCM's
/// enumerates every service on the host, so a second call would cost a real
/// sweep.
///
/// `installed` is every id installed through `mgr` so far (a registered id whose
/// `install` failed has already panicked the scenario) — `run`'s own, plus
/// the [`HAND_EDITED_ID`] the *caller* installed. Each must be in `list()` as
/// `Ours`, not just `id`: an enumeration that drops one it could read is
/// indistinguishable from one where that daemon does not exist.
fn assert_list_is_complete_and_pid_agrees(mgr: &dyn ServiceManager, id: &Id, installed: &[Id]) {
    let status_pid = mgr.status(id).expect("status").pid;
    let listed = mgr.list().expect("list");
    for other in installed {
        assert!(
            listed
                .iter()
                .any(|entry| matches!(entry, Installed::Ours { spec, .. } if spec.id == *other)),
            "id {other} was installed through mgr and must appear in list() as Ours: {listed:?}"
        );
    }
    let list_pid = listed
        .iter()
        .find_map(|entry| match entry {
            Installed::Ours { spec, pid, .. } if spec.id == *id => Some(*pid),
            _ => None,
        })
        .unwrap_or_else(|| panic!("id {id} must appear in list()"));
    assert_eq!(
        list_pid, status_pid,
        "list's pid must agree with status's pid for id {id}"
    );
}
