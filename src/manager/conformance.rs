//! Policy assertions runnable against any [`ServiceManager`].
//!
//! [`run`] is the deliverable, not the fake it is first exercised against:
//! every real backend — systemd, launchd, SCM — calls it too, from its own elevated
//! integration test, so a real backend's `install` is checked against the
//! exact same behavioral contract as the fake's — the whole point of routing
//! every implementation through [`crate::decide::decide`] instead of letting
//! each restate the policy table.
//!
//! ## Cleanup
//!
//! `run` installs several services over the course of its scenarios. On a
//! real backend's integration test, that means real systemd units / launchd
//! plists / SCM services on whatever host is running it — so `run` tracks
//! every id it installs and uninstalls each one before returning, via an
//! RAII guard so a mid-scenario assertion failure still cleans up rather
//! than leaving stragglers on a persistent runner. Ids are process- and
//! call-unique (a pid plus a monotonic counter), not cryptographically
//! random — sufficient given cleanup runs every time, including on panic.
//!
//! ## The seeded ids
//!
//! Four of the scenarios ([`refuses_foreign_even_with_force`],
//! [`foreign_refuses_every_verb`], [`conflict_requires_force`] and
//! [`an_unclassifiable_id_is_never_silently_absent`]) need state that cannot be
//! produced through [`ServiceManager`]'s own methods: a foreign (unmarked)
//! service, a hand-edited Goetia artifact, and an artifact no read can obtain.
//! `run` therefore does *not* create these itself — it requires the **caller**
//! to have already put `mgr` into that state at three fixed, reserved ids
//! before calling `run`:
//!
//! - [`FOREIGN_ID`]: `mgr` already has *something* installed at this id,
//!   through means entirely outside Goetia (a hand-written unit file /
//!   plist / registry key carrying no Goetia marker at all). `run` never
//!   writes to or removes this id — cleanup is the caller's responsibility,
//!   the same as the seeding was.
//! - [`HAND_EDITED_ID`]: `mk(HAND_EDITED_ID)` has already been installed
//!   through `mgr.install()`, and the resulting artifact has then been
//!   mutated outside Goetia so it no longer matches what regenerating its
//!   own embedded spec would produce (e.g. an extra hand-added directive).
//!   `run`'s own `conflict_requires_force` scenario forces an overwrite
//!   here, so this one *is* included in `run`'s own cleanup — the caller
//!   only needs to seed it once per call to `run`.
//! - [`UNDETERMINED_ID`]: an artifact whose bytes this process cannot
//!   obtain is already at this id, for
//!   [`an_unclassifiable_id_is_never_silently_absent`]. That scenario
//!   asserts every verb *refuses* the id, so `run` never writes to or
//!   removes it either — cleanup is the caller's, as for [`FOREIGN_ID`].
//!   Which artifact seeds it is per platform and stated in that scenario's
//!   own doc comment; every backend can seed it from the same elevated
//!   context it installs from.
//!
//! [`fake::Fake`] exposes
//! `seed_foreign`/`install_then_hand_edit`/`seed_opaque` for exactly this; a
//! real backend's integration test does the equivalent with direct
//! filesystem/registry access, which it already has as elevated test code.
//!
//! [`ServiceManager`]: super::ServiceManager
//! [`fake::Fake`]: super::fake::Fake

use std::sync::atomic::{AtomicU64, Ordering};

use super::{Budget, Installed, ServiceManager, State};
use crate::error::Error;
use crate::spec::{DaemonSpec, Id};

/// See the module doc comment. Reserved: no other scenario in `run` uses
/// this id.
pub const FOREIGN_ID: &str = "goetia-conformance-foreign";

/// See the module doc comment. Reserved: no other scenario in `run` uses
/// this id.
pub const HAND_EDITED_ID: &str = "goetia-conformance-hand-edited";

/// See [`an_unclassifiable_id_is_never_silently_absent`], the one scenario
/// that reads this id. Reserved: no other scenario uses it.
pub const UNDETERMINED_ID: &str = "goetia-conformance-undetermined";

/// RAII cleanup for every id `run`'s scenarios install. `Drop` cannot
/// return a `Result`, so an uninstall failure during cleanup is logged to
/// stderr rather than propagated — a cleanup failure that reports nothing
/// would surface later only as a mysterious straggler service, long after
/// the run that left it (the same reasoning `tests/support/service_guard.rs`
/// documents for the marker-inertness probes).
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
/// id. Every id `run` uses is either freshly generated (never installed
/// before) or one of
/// [`FOREIGN_ID`]/[`HAND_EDITED_ID`]/[`UNDETERMINED_ID`] — see the module
/// doc comment for what the caller must have already arranged at those
/// three, and for what `run` cleans up on its own.
pub fn run(mgr: &dyn ServiceManager, mk: &dyn Fn(&str) -> DaemonSpec) {
    let mut cleanup = Cleanup { mgr, ids: Vec::new() };
    // Registered up front, not inside `conflict_requires_force`: the caller
    // has already installed something at this id before calling `run` (see
    // the module doc comment), so it needs cleanup regardless of which
    // scenario panics first — including one that runs before
    // `conflict_requires_force` ever gets to register it itself.
    cleanup
        .ids
        .push(Id::try_from(HAND_EDITED_ID).expect("HAND_EDITED_ID is a valid Id by construction"));

    install_is_idempotent(mgr, mk, &mut cleanup.ids);
    install_does_not_start(mgr, mk, &mut cleanup.ids);
    install_does_not_enable(mgr, mk, &mut cleanup.ids);
    reinstall_preserves_enablement(mgr, mk, &mut cleanup.ids);
    start_and_stop_are_idempotent(mgr, mk, &mut cleanup.ids);
    starting_with_no_budget_is_accepted_and_establishes_nothing(mgr, mk, &mut cleanup.ids);
    list_and_status_agree_on_pid(mgr, mk, &mut cleanup.ids);
    refuses_foreign_even_with_force(mgr, mk);
    foreign_refuses_every_verb(mgr, mk);
    conflict_requires_force(mgr, mk);
    an_unclassifiable_id_is_never_silently_absent(mgr, mk);

    // `cleanup` drops here, uninstalling everything pushed above — including
    // on an early return via a panicking assertion, since `Drop` still runs
    // during unwinding.
}

/// This module is compiled into the normal library artifact (not
/// `#[cfg(test)]`-gated) so that each real backend's own integration test —
/// a separate crate under `tests/`, linking only against `goetia`'s public
/// API — can call [`run`]. That rules out a dev-only randomness crate here;
/// a process-wide counter plus the pid is unique enough for this module's
/// only need (an id `run` has never used before), especially now that `run`
/// uninstalls everything it creates rather than leaving it for a future
/// call to collide with.
static COUNTER: AtomicU64 = AtomicU64::new(0);

fn fresh_id(label: &str) -> String {
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("goetia-conformance-{label}-{pid:x}-{n:x}", pid = std::process::id())
}

fn install_is_idempotent(mgr: &dyn ServiceManager, mk: &dyn Fn(&str) -> DaemonSpec, cleanup: &mut Vec<Id>) {
    let spec = mk(&fresh_id("idempotent"));

    let first = mgr.install(&spec, false).expect("first install");
    cleanup.push(spec.id.clone());
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
    let spec = mk(&fresh_id("no-start"));
    mgr.install(&spec, false).expect("install");
    cleanup.push(spec.id.clone());

    let status = mgr.status(&spec.id).expect("status of a just-installed service");
    assert_ne!(
        status.state,
        State::Running,
        "install must not start the service (id {})",
        spec.id
    );
}

fn install_does_not_enable(mgr: &dyn ServiceManager, mk: &dyn Fn(&str) -> DaemonSpec, cleanup: &mut Vec<Id>) {
    let spec = mk(&fresh_id("no-enable"));
    mgr.install(&spec, false).expect("install");
    cleanup.push(spec.id.clone());

    let status = mgr.status(&spec.id).expect("status of a just-installed service");
    assert!(
        !status.enabled,
        "install must not enable the service at boot (id {})",
        spec.id
    );
}

fn reinstall_preserves_enablement(mgr: &dyn ServiceManager, mk: &dyn Fn(&str) -> DaemonSpec, cleanup: &mut Vec<Id>) {
    let fresh = fresh_id("preserve-enable");
    let spec = mk(&fresh);
    mgr.install(&spec, false).expect("install");
    cleanup.push(spec.id.clone());
    mgr.enable(&spec.id).expect("enable");
    assert!(
        mgr.status(&spec.id).expect("status after enable").enabled,
        "enable must actually enable (id {})",
        spec.id
    );

    // Re-run `install` over an id whose spec has since changed. A routine
    // config update (this) must not silently undo a deliberate enable, any
    // more than it should undo a deliberate `disable`.
    let mut changed = mk(&fresh);
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

/// See the module doc comment: the caller must have already put non-Goetia
/// content at [`FOREIGN_ID`] before calling [`run`]. Never writes — `mgr`
/// refuses every attempt — so nothing here needs cleanup.
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

/// "Goetia never touches a service it did not create" (§5) is not an
/// `install`-only rule: every other verb must refuse a foreign id too, or a
/// real backend could `uninstall`/`start`/`stop`/`enable`/`disable`/`status`
/// a stranger's service it merely happens to share an id with. See the
/// module doc comment for what the caller must have arranged at
/// [`FOREIGN_ID`] before calling [`run`].
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
    assert!(mgr.status(&spec.id).is_err(), "status must refuse a foreign id");
}

/// See the module doc comment: the caller must have already installed
/// `mk(HAND_EDITED_ID)` through `mgr.install()` and then hand-edited the
/// resulting artifact before calling [`run`]. Cleanup for this id is
/// registered by `run` itself, up front — see there.
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

    // The hand-edit is really gone, not merely papered over: installing the
    // same spec again, still without force, must now be a clean no-op.
    let after = mgr.install(&spec, false).expect("install after force");
    assert!(
        matches!(after, crate::decide::Outcome::UpToDate),
        "force must actually overwrite the hand-edit, got {after:?}"
    );
}

/// `daemon restart`'s `stop` then `start` depends on `stop` succeeding on a
/// service that was never started, and real managers disagree by default
/// (`launchctl bootout`/`ControlService(STOP)` on an inactive service both
/// fail; `systemctl stop` does not) — see `ServiceManager::stop`'s doc
/// comment. Every backend must paper over that difference the same way, or
/// `restart` works on Linux and errors on macOS/Windows for the exact same
/// installed-but-never-started daemon.
fn start_and_stop_are_idempotent(mgr: &dyn ServiceManager, mk: &dyn Fn(&str) -> DaemonSpec, cleanup: &mut Vec<Id>) {
    let spec = mk(&fresh_id("idempotent-start-stop"));
    mgr.install(&spec, false).expect("install");
    cleanup.push(spec.id.clone());

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

/// `Budget::Immediate` issues the request and returns, establishing nothing
/// — so **no assertion about the resulting state follows it here**, and that
/// omission is the scenario, not a gap in it. The request has been accepted
/// and the service may be anywhere between not yet forked and already
/// running; which of those a backend happens to observe is the scheduler's
/// choice, so asserting either would be asserting a race.
///
/// What is left assertable, and is asserted: that every backend *accepts*
/// the budget, and that a subsequent `Budget::DEFAULT` start still reaches
/// `Running`. The second half is what stops a backend satisfying this by
/// making `Immediate` a no-op that quietly leaves the service unstartable.
///
/// **No scenario here asserts the expiry rule** — that an expiry is
/// `Error::WaitTimeout` and never `Ok(())` — against a real backend, and
/// that is a decision rather than a gap. Asserting it would need a daemon
/// that provably fails to reach running on all three platforms, which no
/// portable `DaemonSpec` can express; the alternative, a real daemon under
/// a budget small enough to expire, asserts whichever outcome the scheduler
/// happened to pick, which is the bet this project forbids. `Fake` carries
/// the rule instead, deriving each expiry from the budget without sleeping
/// (`fake_tests.rs`: `a_stalled_start_times_out_under_a_bounded_budget` and
/// its stop mirror, plus the two `Unbounded` refusals). So a real backend
/// that swallowed an expiry as `Ok` would pass every scenario in this file
/// — that is understood, and the reason there is nothing to find here.
fn starting_with_no_budget_is_accepted_and_establishes_nothing(
    mgr: &dyn ServiceManager,
    mk: &dyn Fn(&str) -> DaemonSpec,
    cleanup: &mut Vec<Id>,
) {
    let spec = mk(&fresh_id("no-budget-start"));
    mgr.install(&spec, false).expect("install");
    cleanup.push(spec.id.clone());

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

/// `pid` must mean the same thing in `list` as in `status` — the number
/// that backend's own `status()` would report for this id at this moment —
/// on every real backend, not just the fake. Checked while stopped, while
/// running, and after stopping again, so a backend that only agrees at one
/// of those three moments (e.g. a `list` that caches a value from install
/// time) still fails this.
fn list_and_status_agree_on_pid(mgr: &dyn ServiceManager, mk: &dyn Fn(&str) -> DaemonSpec, cleanup: &mut Vec<Id>) {
    let spec = mk(&fresh_id("pid-agreement"));
    mgr.install(&spec, false).expect("install");
    cleanup.push(spec.id.clone());

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
/// The caller must have already put an artifact whose bytes this process
/// cannot obtain at [`UNDETERMINED_ID`]. That every verb refuses — `install
/// --force` included — is what this asserts rather than assumes, so it
/// registers no cleanup of its own; the caller's own cleanup for
/// [`UNDETERMINED_ID`] covers a backend that writes anyway.
///
/// # The state, and why every caller can seed it
///
/// A read that *did not complete* — which is not the same as a read that was
/// denied, and the difference is what lets this be one of [`run`]'s scenarios
/// rather than an assertion each suite restates below its own privilege
/// boundary. Elevation dissolves a denial: on systemd and launchd every read
/// is DAC-gated and root holds `CAP_DAC_OVERRIDE`, so a mode seeds nothing in
/// a suite that has to be root to install anything at all. An errno that is
/// not about permission dissolves for nobody:
///
/// - **systemd** — a regular file where `<id>.service.d` belongs. `read_dir`
///   answers `ENOTDIR`, and `residue` — the predicate every verb asks — is
///   left unable to say whether anything occupies the id.
/// - **launchd** — a self-referential symlink at the plist path. `stat`
///   answers `ELOOP`, so `obtain` gets no bytes, while `symlink_metadata`
///   still sees the entry, which is exactly "something is there and nothing
///   about it was established".
/// - **SCM** — a `Deny`/`ReadKey` ACE over `Parameters`, which stops an
///   Administrator too (`tests/scm_integration/deny.rs`).
///
/// A seed whose *bytes arrive* is not this state and never was: a non-UTF-8
/// fragment or plist is `Foreign` — established, omitted from `list`, refused
/// by every verb with a different error — so seeding one here asserts the
/// wrong contract while looking correct.
///
/// Seeding [`UNDETERMINED_ID`] changes `list`'s answer for the whole host, not
/// only for that id: it raises `daemon list`'s exit code, and on Windows it
/// collapses into an aggregate entry. A caller whose suite also asserts a
/// *clean* host-wide listing has to serialize the two against each other — the
/// `UNIT_DIR_EXCLUSIVE` label the integration suites share by name.
///
/// A backend satisfies the `list` half by naming the id in an
/// [`Installed::Undetermined`] entry *or* by emitting any aggregate one. The
/// aggregate counts because a null name stands for ids the entry could not
/// separate, so it may stand for this one — the same rule that forbids
/// concluding absence from it. SCM emits only that form, since a listing can
/// have hundreds of denied services and one entry cannot carry their names.
/// The per-id half below is exact on every backend, so accepting the
/// aggregate does not weaken the scenario to nothing.
pub fn an_unclassifiable_id_is_never_silently_absent(mgr: &dyn ServiceManager, mk: &dyn Fn(&str) -> DaemonSpec) {
    let spec = mk(UNDETERMINED_ID);
    let id = &spec.id;

    let listed = mgr
        .list()
        .expect("one id goetia cannot classify must not take the whole listing down");
    assert!(
        listed.iter().any(|entry| match entry {
            Installed::Undetermined { name, .. } => name.is_none() || name.as_deref() == Some(id.as_str()),
            Installed::Ours { .. } | Installed::OursUnreadable { .. } => false,
        }),
        "id {id} is neither reported undetermined nor covered by an aggregate entry, so list() \
         silently says it does not exist: {listed:?}"
    );
    assert!(
        !listed.iter().any(|entry| match entry {
            Installed::Ours { spec, .. } => spec.id == *id,
            Installed::OursUnreadable { name, .. } => name == id.as_str(),
            Installed::Undetermined { .. } => false,
        }),
        "the read that would have classified id {id} never completed, so no ownership was \
         established and none may be claimed: {listed:?}"
    );

    // `NotInstalled` would certify absence and `Foreign` a stranger's
    // presence; this read established neither, and a verb answering either
    // would write off — or act on — an id goetia cannot classify.
    for (verb, result) in [
        ("status", mgr.status(id).map(drop)),
        ("uninstall", mgr.uninstall(id)),
        ("enable", mgr.enable(id)),
        ("disable", mgr.disable(id)),
        ("start", mgr.start(id, Budget::DEFAULT)),
        ("stop", mgr.stop(id, Budget::DEFAULT)),
    ] {
        assert!(
            matches!(&result, Err(Error::Undetermined { .. })),
            "{verb} on an id goetia cannot classify must be Undetermined, got {result:?}"
        );
    }

    // The two verbs that *write*, `--force` included.
    // [`refuses_foreign_even_with_force`] makes this point one epistemic step
    // further in: refusing to clobber what goetia has *proved* is a
    // stranger's is worth little if it will clobber what it could not read at
    // all. `force` must not even be consulted here — the read that would have
    // supplied `decide`'s inputs never completed, so there is no classified
    // artifact for it to override.
    for (verb, result) in [
        ("install", mgr.install(&spec, false).map(drop)),
        ("install with force", mgr.install(&spec, true).map(drop)),
        ("preview_install", mgr.preview_install(&spec).map(drop)),
    ] {
        assert!(
            matches!(&result, Err(Error::Undetermined { .. })),
            "{verb} over an id goetia cannot classify must be Undetermined, got {result:?}"
        );
    }
}

/// Two assertions off one `list()`, which is why they share a function: SCM's
/// enumerates every service on the host, so a second call to make the split
/// look tidier would cost a real sweep.
///
/// `installed` is every id installed through `mgr` so far — `run`'s own, plus
/// the [`HAND_EDITED_ID`] the *caller* installed before `run` was called.
/// Each must be in `list()` as `Ours`, not just `id`: an enumeration that
/// drops one it could read is indistinguishable from one where that daemon
/// does not exist.
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
