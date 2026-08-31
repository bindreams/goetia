//! Policy assertions runnable against any [`ServiceManager`].
//!
//! [`run`] is the deliverable, not the fake it is first exercised against:
//! every real backend (Tasks 11-13) calls it too, from its own elevated
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
//! ## The two seeded ids
//!
//! Two of the six scenarios ([`refuses_foreign_even_with_force`] and
//! [`conflict_requires_force`]) need state that cannot be produced through
//! [`ServiceManager`]'s own methods: a foreign (unmarked) service, and a
//! hand-edited Goetia artifact. `run` therefore does *not* create these
//! itself — it requires the **caller** to have already put `mgr` into that
//! state at two fixed, reserved ids before calling `run`:
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
//!
//! [`fake::Fake`] exposes `seed_foreign`/`install_then_hand_edit` for
//! exactly this; a real backend's integration test does the equivalent with
//! direct filesystem/registry access, which it already has as elevated test
//! code.
//!
//! [`ServiceManager`]: super::ServiceManager
//! [`fake::Fake`]: super::fake::Fake

use std::sync::atomic::{AtomicU64, Ordering};

use super::{ServiceManager, State};
use crate::spec::{DaemonSpec, Id};

/// See the module doc comment. Reserved: no other scenario in `run` uses
/// this id.
pub const FOREIGN_ID: &str = "goetia-conformance-foreign";

/// See the module doc comment. Reserved: no other scenario in `run` uses
/// this id.
pub const HAND_EDITED_ID: &str = "goetia-conformance-hand-edited";

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
/// before) or one of [`FOREIGN_ID`]/[`HAND_EDITED_ID`] — see the module doc
/// comment for what the caller must have already arranged at those two, and
/// for what `run` cleans up on its own.
pub fn run(mgr: &dyn ServiceManager, mk: &dyn Fn(&str) -> DaemonSpec) {
    let mut cleanup = Cleanup { mgr, ids: Vec::new() };

    install_is_idempotent(mgr, mk, &mut cleanup.ids);
    install_does_not_start(mgr, mk, &mut cleanup.ids);
    install_does_not_enable(mgr, mk, &mut cleanup.ids);
    reinstall_preserves_enablement(mgr, mk, &mut cleanup.ids);
    refuses_foreign_even_with_force(mgr, mk);
    foreign_refuses_every_verb(mgr, mk);
    conflict_requires_force(mgr, mk, &mut cleanup.ids);

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
    assert!(mgr.start(&spec.id).is_err(), "start must refuse a foreign id");
    assert!(mgr.stop(&spec.id).is_err(), "stop must refuse a foreign id");
    assert!(mgr.status(&spec.id).is_err(), "status must refuse a foreign id");
}

/// See the module doc comment: the caller must have already installed
/// `mk(HAND_EDITED_ID)` through `mgr.install()` and then hand-edited the
/// resulting artifact before calling [`run`].
fn conflict_requires_force(mgr: &dyn ServiceManager, mk: &dyn Fn(&str) -> DaemonSpec, cleanup: &mut Vec<Id>) {
    let spec = mk(HAND_EDITED_ID);

    let outcome = mgr
        .install(&spec, false)
        .expect("install over a hand-edited artifact must not error");
    match outcome {
        crate::decide::Outcome::Conflict { artifact_diff } => {
            assert!(!artifact_diff.is_empty(), "conflict must carry a non-empty diff");
        }
        other => panic!("expected Conflict without force, got {other:?}"),
    }

    let forced = mgr
        .install(&spec, true)
        .expect("install with force over a hand-edited artifact must not error");
    // Force resolves the conflict by writing: from here on this id is ours
    // to clean up, same as every id `run` created outright.
    cleanup.push(spec.id.clone());
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
