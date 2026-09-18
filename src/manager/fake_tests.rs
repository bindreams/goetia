use std::collections::BTreeMap;
use std::time::Duration;

use super::*;
use crate::manager::Budget;
use crate::manager::conformance;
use crate::spec::{Id, Kind, Restart, User};

// Fixtures ============================================================================================================

fn mk(id: &str) -> DaemonSpec {
    DaemonSpec {
        id: Id::try_from(id).unwrap(),
        name: id.to_string(),
        command: vec!["daemon".to_owned()],
        cwd: None,
        env: BTreeMap::new(),
        user: User::Root,
        restart: Restart::OnFailure,
        restart_delay: None,
        logs: None,
        kind: Kind::Simple,
    }
}

// The deliverable =====================================================================================================

#[skuld::test]
fn fake_passes_conformance() {
    let fake = Fake::new();
    fake.seed_foreign(conformance::FOREIGN_ID, "some pre-existing, unrelated service\n");
    fake.install_then_hand_edit(&mk(conformance::HAND_EDITED_ID), "# hand-added directive\n");
    // The fake's artifacts are in-memory strings and are always readable, so
    // an unobtainable one has to be modelled rather than provoked — see
    // `Fake::seed_opaque`.
    fake.seed_opaque(conformance::UNDETERMINED_ID);

    conformance::run(&fake, &mk);
}

// Behavior the conformance scenarios do not exercise ==================================================================

#[skuld::test]
fn list_excludes_foreign_entries() {
    let fake = Fake::new();
    fake.seed_foreign("stranger", "not a goetia artifact at all\n");

    let listed = fake.list().unwrap();
    assert!(
        listed.is_empty(),
        "a foreign entry must not appear in list(): {listed:?}"
    );
}

/// `pid` must mean the same thing in `list` as in `status`: the number
/// `status` would report for this id at this moment, not an independently
/// derived value. Checked both while running (both `Some`, and equal) and
/// once stopped (both `None`).
#[skuld::test]
fn list_reports_the_same_pid_as_status() {
    let fake = Fake::new();
    let spec = mk("pid-agreement");
    fake.install(&spec, false).unwrap();
    fake.start(&spec.id, Budget::DEFAULT).unwrap();

    let status = fake.status(&spec.id).unwrap();
    let listed = fake.list().unwrap();
    let entry = listed
        .iter()
        .find_map(|e| match e {
            Installed::Ours { spec: s, pid, .. } if s.id == spec.id => Some(*pid),
            _ => None,
        })
        .expect("just-installed entry must be present and readable");
    assert!(status.pid.is_some(), "a running daemon must report a pid");
    assert_eq!(
        entry, status.pid,
        "list's pid must agree with status's pid while running"
    );

    fake.stop(&spec.id, Budget::DEFAULT).unwrap();
    let status = fake.status(&spec.id).unwrap();
    let listed = fake.list().unwrap();
    let entry = listed
        .iter()
        .find_map(|e| match e {
            Installed::Ours { spec: s, pid, .. } if s.id == spec.id => Some(*pid),
            _ => None,
        })
        .expect("just-stopped entry must be present and readable");
    assert!(status.pid.is_none(), "a stopped daemon must report no pid");
    assert_eq!(
        entry, status.pid,
        "list's pid must agree with status's pid once stopped"
    );
}

#[skuld::test]
fn list_reports_unreadable_entries_without_erroring() {
    let fake = Fake::new();
    fake.seed_foreign("corrupt", format!("{FAKE_MARKER}\nSpec: not-valid-base64!!!\n"));
    fake.install_then_hand_edit(&mk("readable"), "# extra\n");

    let listed = fake.list().unwrap();

    let unreadable = listed
        .iter()
        .find(|entry| matches!(entry, Installed::OursUnreadable { name, .. } if name == "corrupt"));
    assert!(
        unreadable.is_some(),
        "a marked-but-undecodable entry must appear as OursUnreadable: {listed:?}"
    );
    let readable = listed
        .iter()
        .find(|entry| matches!(entry, Installed::Ours { spec, .. } if spec.id.as_str() == "readable"));
    assert!(
        readable.is_some(),
        "one bad entry must not take down the rest of list(): {listed:?}"
    );
}

#[skuld::test]
fn uninstall_removes_entry() {
    let fake = Fake::new();
    let spec = mk("removable");
    fake.install(&spec, false).unwrap();

    fake.uninstall(&spec.id).unwrap();

    assert!(fake.list().unwrap().is_empty());
    assert!(fake.status(&spec.id).is_err(), "status after uninstall must error");
}

#[skuld::test]
fn operations_on_an_unknown_id_error() {
    let fake = Fake::new();
    let id = Id::try_from("never-installed").unwrap();

    assert!(fake.uninstall(&id).is_err());
    assert!(fake.enable(&id).is_err());
    assert!(fake.disable(&id).is_err());
    assert!(fake.start(&id, Budget::DEFAULT).is_err());
    assert!(fake.stop(&id, Budget::DEFAULT).is_err());
    assert!(fake.status(&id).is_err());
}

/// A foreign entry (present, but not Goetia's) must be refused by every
/// verb, not just `install` — "goetia never touches a service it did not
/// create" is not an install-only rule. Regression coverage: every verb,
/// not just `install`, must classify ownership before acting.
#[skuld::test]
fn mutating_verbs_refuse_a_foreign_id() {
    let fake = Fake::new();
    fake.seed_foreign("stranger", "not a goetia artifact at all\n");
    let id = Id::try_from("stranger").unwrap();

    assert!(fake.uninstall(&id).is_err(), "uninstall must refuse a foreign id");
    assert!(fake.enable(&id).is_err(), "enable must refuse a foreign id");
    assert!(fake.disable(&id).is_err(), "disable must refuse a foreign id");
    assert!(
        fake.start(&id, Budget::DEFAULT).is_err(),
        "start must refuse a foreign id"
    );
    assert!(
        fake.stop(&id, Budget::DEFAULT).is_err(),
        "stop must refuse a foreign id"
    );
    assert!(fake.status(&id).is_err(), "status must refuse a foreign id");
    // Refusing it must not have removed it either.
    assert_eq!(
        fake.list().unwrap().len(),
        0,
        "a foreign entry is excluded from list(), not deleted"
    );
}

/// The marker alone is proof of ownership: an entry Goetia marked but can
/// no longer decode is still *ours*, so `uninstall` — the documented
/// recovery for `decide::Outcome::RefuseUnreadable` — must still work on
/// it, unlike a truly foreign entry.
#[skuld::test]
fn uninstall_accepts_an_unreadable_entry() {
    let fake = Fake::new();
    fake.seed_foreign("corrupt", format!("{FAKE_MARKER}\nSpec: not-valid-base64!!!\n"));
    let id = Id::try_from("corrupt").unwrap();

    fake.uninstall(&id)
        .expect("uninstall must accept a marked-but-undecodable entry");
}

// Residual artifacts ==================================================================================================

/// The class `seed_residual_artifact` exists for: an id whose primary artifact is gone but which
/// the platform still acts on. `NotInstalled` is the one error `cli::uninstall` renders as success,
/// so it must be reserved for an id that is genuinely empty.
#[skuld::test]
fn absence_over_a_residual_artifact_is_not_not_installed() {
    let fake = Fake::new();
    fake.seed_residual_artifact("leftover");
    let id = Id::try_from("leftover").unwrap();

    for (verb, result) in [
        ("uninstall", fake.uninstall(&id)),
        ("enable", fake.enable(&id)),
        ("disable", fake.disable(&id)),
        ("start", fake.start(&id, Budget::DEFAULT)),
        ("stop", fake.stop(&id, Budget::DEFAULT)),
        ("status", fake.status(&id).map(drop)),
    ] {
        match result {
            Err(Error::Foreign { .. }) => {}
            other => panic!("{verb} on a residual artifact must be Foreign, got {other:?}"),
        }
    }
}

/// The contradiction the class is really about: `uninstall` reporting the id empty while `install`
/// on that same id refuses it as foreign. Both verbs must describe one state the same way.
#[skuld::test]
fn install_and_uninstall_agree_about_a_residual_artifact() {
    let fake = Fake::new();
    let spec = mk("leftover");
    fake.seed_residual_artifact(spec.id.as_str());

    let outcome = fake
        .install(&spec, false)
        .expect("install classifies rather than errors");
    assert!(matches!(outcome, Outcome::RefuseForeign { .. }), "{outcome:?}");
    assert!(
        matches!(fake.uninstall(&spec.id), Err(Error::Foreign { .. })),
        "uninstall must not call the id empty when install calls it foreign"
    );

    // Not even under `--force`: adopting what goetia did not write is what `RefuseForeign` refuses.
    let forced = fake
        .install(&spec, true)
        .expect("forced install classifies rather than errors");
    assert!(matches!(forced, Outcome::RefuseForeign { .. }), "{forced:?}");
}

/// Residue is not a daemon, so `list` reports nothing for it — matching systemd's `list`, which
/// skips a `<id>.service.d` directory because it is not a fragment.
#[skuld::test]
fn list_omits_an_id_that_has_only_a_residual_artifact() {
    let fake = Fake::new();
    fake.seed_residual_artifact("leftover");

    assert!(fake.list().unwrap().is_empty());
}

// Opaque artifacts ====================================================================================================

/// Every verb's answer for `spec`'s id, labelled — including `install` and
/// `preview_install`, the two that never reach `Store::get` and so are
/// exactly the ones a partial implementation leaves disagreeing with the
/// other six.
fn every_verb(fake: &Fake, spec: &DaemonSpec) -> Vec<(&'static str, Result<()>)> {
    vec![
        ("status", fake.status(&spec.id).map(drop)),
        ("uninstall", fake.uninstall(&spec.id)),
        ("enable", fake.enable(&spec.id)),
        ("disable", fake.disable(&spec.id)),
        ("start", fake.start(&spec.id, Budget::DEFAULT)),
        ("stop", fake.stop(&spec.id, Budget::DEFAULT)),
        ("install", fake.install(spec, false).map(drop)),
        ("preview_install", fake.preview_install(spec).map(drop)),
    ]
}

/// An artifact whose bytes could not be read proves neither presence nor
/// absence, so it is neither `Ours` nor `OursUnreadable` — the latter would
/// put goetia's name on what may be a stranger's service.
#[skuld::test]
fn list_reports_an_opaque_entry_as_undetermined() {
    let fake = Fake::new();
    fake.seed_opaque("opaque");

    let listed = fake.list().unwrap();

    let reason = listed
        .iter()
        .find_map(|entry| match entry {
            Installed::Undetermined { name, reason } if name.as_deref() == Some("opaque") => Some(reason),
            _ => None,
        })
        .unwrap_or_else(|| panic!("an unreadable artifact must appear as Undetermined: {listed:?}"));
    assert!(!reason.is_empty(), "Undetermined must say why: {listed:?}");
    assert!(
        !listed
            .iter()
            .any(|entry| matches!(entry, Installed::Ours { .. } | Installed::OursUnreadable { .. })),
        "an unread artifact must not be claimed as goetia's: {listed:?}"
    );
}

/// The silent-skip regression this variant exists to prevent: one entry
/// goetia could not read must not remove itself, nor anything else, from
/// the enumeration.
#[skuld::test]
fn an_opaque_entry_is_never_omitted_from_list() {
    let fake = Fake::new();
    fake.install(&mk("healthy"), false).unwrap();
    fake.seed_opaque("opaque");

    let listed = fake.list().unwrap();

    assert_eq!(listed.len(), 2, "both entries must be reported: {listed:?}");
}

/// `NotInstalled` certifies absence and `Foreign` certifies someone else's
/// presence; a failed read establishes neither, so every verb must say so
/// rather than pick the nearest.
#[skuld::test]
fn every_verb_refuses_an_opaque_id_as_undetermined() {
    let fake = Fake::new();
    let spec = mk("opaque");
    fake.seed_opaque(spec.id.as_str());

    for (verb, result) in every_verb(&fake, &spec) {
        match result {
            Err(Error::Undetermined { id, reason, .. }) => {
                assert_eq!(id, "opaque", "{verb} must name the id it could not read");
                assert!(!reason.is_empty(), "{verb} must say why");
            }
            other => panic!("{verb} on an unreadable artifact must be Undetermined, got {other:?}"),
        }
    }
}

/// The failed read is what happened *first*: it is the read that would have
/// found the entry, so a stale entry at the same id cannot answer for it.
#[skuld::test]
fn an_opaque_id_shadows_an_entry_at_the_same_id() {
    let fake = Fake::new();
    let spec = mk("shadowed");
    fake.install(&spec, false).unwrap();
    fake.seed_opaque(spec.id.as_str());

    for (verb, result) in every_verb(&fake, &spec) {
        assert!(
            matches!(result, Err(Error::Undetermined { .. })),
            "{verb} must not answer from the entry the failed read would have found, got {result:?}"
        );
    }
}

/// `list` must agree with `Store::get` about a shadowed id: exactly one
/// entry, and not the `Ours` the failed read would have found. Nothing else
/// reaches it — the shadowing test above never calls `list`, and every other
/// test that does (the two opaque-`list` tests, and `fake_passes_conformance`
/// through `conformance::run`) uses ids that are opaque *or* installed, never
/// both — so without this, `list` can report one id twice, as unclassifiable
/// and as goetia's at once.
#[skuld::test]
fn an_opaque_id_is_listed_once_and_not_as_ours() {
    let fake = Fake::new();
    let spec = mk("shadowed");
    fake.install(&spec, false).unwrap();
    fake.seed_opaque(spec.id.as_str());

    let listed = fake.list().unwrap();

    assert_eq!(listed.len(), 1, "a shadowed id must be reported once: {listed:?}");
    assert!(
        matches!(&listed[0], Installed::Undetermined { name, .. } if name.as_deref() == Some("shadowed")),
        "list must report the failed read, not the entry that read would have found: {listed:?}"
    );
}

/// Both ways out, and not the third: `uninstall` is `RefuseUnreadable`'s
/// remedy and certifies ownership, which is precisely what was not
/// established here.
#[skuld::test]
fn the_opaque_recovery_names_both_causes_and_not_uninstall() {
    let fake = Fake::new();
    let id = Id::try_from("opaque").unwrap();
    fake.seed_opaque(id.as_str());

    let Err(Error::Undetermined { recovery, .. }) = fake.status(&id) else {
        panic!("status on an unreadable artifact must be Undetermined");
    };

    assert!(
        recovery.contains("re-run") && recovery.contains("enough privilege"),
        "recovery must offer re-running with more privilege: {recovery}"
    );
    assert!(
        recovery.contains("any privilege level"),
        "recovery must also cover an artifact no privilege level can read: {recovery}"
    );
    assert!(
        !recovery.contains("uninstall"),
        "uninstall certifies ownership this read never established: {recovery}"
    );
}

// The call log ========================================================================================================

/// `restart_does_not_start_after_a_stop_that_timed_out` asserts on
/// the **absence** of a call, which no state read can express: the fake's
/// `start` on an already-`Running` entry is idempotent and leaves the state
/// byte-identical, so "it was never called" and "it was called and changed
/// nothing" are the same observation from the outside. Observing the call
/// that changed nothing is the whole point of the log.
#[skuld::test]
fn the_fake_records_the_calls_it_was_asked_to_make() {
    let fake = Fake::new();
    let first = mk("first");
    let second = mk("second");
    fake.install(&first, false).unwrap();
    fake.install(&second, false).unwrap();

    fake.start(&first.id, Budget::DEFAULT).unwrap();
    // Already running: idempotent, changes nothing, and is still recorded.
    fake.start(&first.id, Budget::DEFAULT).unwrap();
    fake.stop(&first.id, Budget::DEFAULT).unwrap();
    fake.start(&second.id, Budget::DEFAULT).unwrap();

    assert_eq!(
        fake.calls(),
        vec![
            ("start", "first".to_string()),
            ("start", "first".to_string()),
            ("stop", "first".to_string()),
            ("start", "second".to_string()),
        ]
    );
}

// Stalled entries =====================================================================================================

/// The expiry is **derived from the budget, never lived**: nothing here
/// sleeps, which is what makes every timeout test in the tree deterministic
/// rather than a bet on a scheduler.
#[skuld::test]
fn a_stalled_start_times_out_under_a_bounded_budget() {
    let fake = Fake::new();
    let spec = mk("stalls");
    fake.install(&spec, false).unwrap();
    fake.seed_start_stalls(spec.id.as_str());

    let result = fake.start(&spec.id, Budget::Bounded(Duration::from_secs(10)));

    match result {
        Err(Error::WaitTimeout { id, awaited, .. }) => {
            assert_eq!(id, "stalls");
            assert_eq!(awaited, "running");
        }
        other => panic!("a stalled start under a bounded budget must time out, got {other:?}"),
    }
    assert_ne!(
        fake.status(&spec.id).unwrap().state,
        State::Running,
        "a start that timed out must not have fabricated the confirmation"
    );
}

#[skuld::test]
fn a_stalled_stop_times_out_under_a_bounded_budget() {
    let fake = Fake::new();
    let spec = mk("stalls");
    fake.install(&spec, false).unwrap();
    fake.start(&spec.id, Budget::DEFAULT).unwrap();
    fake.seed_stop_stalls(spec.id.as_str());

    match fake.stop(&spec.id, Budget::Bounded(Duration::from_secs(10))) {
        Err(Error::WaitTimeout { id, awaited, .. }) => {
            assert_eq!(id, "stalls");
            assert_eq!(awaited, "stopped");
        }
        other => panic!("a stalled stop under a bounded budget must time out, got {other:?}"),
    }
    assert_eq!(
        fake.status(&spec.id).unwrap().state,
        State::Running,
        "a stop that timed out must not have fabricated the confirmation"
    );
}

/// "Requested, not confirmed" is the honest model of a budget that waits for
/// nothing, and leaving the state alone is what says so.
#[skuld::test]
fn a_stalled_start_is_ok_under_no_budget_and_leaves_the_state_alone() {
    let fake = Fake::new();
    let spec = mk("stalls");
    fake.install(&spec, false).unwrap();
    fake.seed_start_stalls(spec.id.as_str());

    for budget in [Budget::Immediate, Budget::Bounded(Duration::ZERO)] {
        fake.start(&spec.id, budget)
            .unwrap_or_else(|e| panic!("{budget:?} establishes nothing, so it cannot fail: {e}"));
        assert_ne!(
            fake.status(&spec.id).unwrap().state,
            State::Running,
            "{budget:?} confirmed nothing, so it must not have changed the state"
        );
    }
}

#[skuld::test]
fn a_stalled_stop_is_ok_under_no_budget_and_leaves_the_state_alone() {
    let fake = Fake::new();
    let spec = mk("stalls");
    fake.install(&spec, false).unwrap();
    fake.start(&spec.id, Budget::DEFAULT).unwrap();
    fake.seed_stop_stalls(spec.id.as_str());

    for budget in [Budget::Immediate, Budget::Bounded(Duration::ZERO)] {
        fake.stop(&spec.id, budget)
            .unwrap_or_else(|e| panic!("{budget:?} establishes nothing, so it cannot fail: {e}"));
        assert_eq!(
            fake.status(&spec.id).unwrap().state,
            State::Running,
            "{budget:?} confirmed nothing, so it must not have changed the state"
        );
    }
}

/// The fake reads the budget and reports the expiry; it does not live it.
///
/// No tighter bound than the budget itself is asserted, deliberately: any
/// smaller number would be a bet on how fast this machine is, which is the
/// class of assertion this project forbids. A fake that actually slept would
/// not fail this line — it would never reach it, and the suite's own
/// watchdog is what surfaces that.
#[skuld::test]
fn the_fake_never_sleeps() {
    let fake = Fake::new();
    let spec = mk("stalls");
    fake.install(&spec, false).unwrap();
    fake.seed_start_stalls(spec.id.as_str());
    let budget = Duration::from_secs(3600);

    let before = std::time::Instant::now();
    let result = fake.start(&spec.id, Budget::Bounded(budget));
    let elapsed = before.elapsed();

    assert!(matches!(result, Err(Error::WaitTimeout { .. })), "{result:?}");
    assert!(
        elapsed < budget,
        "the fake derived a {budget:?} expiry by living it, taking {elapsed:?}"
    );
}

/// The `Unbounded` hole, and why the fake must refuse rather than
/// approximate. `waits()` is true for `Unbounded`, so a rule phrased as "a
/// stalled entry under a *waiting* budget times out" sends this into
/// `budget::timed_out`, whose own `debug_assert!` requires a `Bounded`
/// budget — so the fake would either trip that assert or report an expiry
/// that provably cannot have happened. The honest model is "hang forever",
/// which is unusable in a test, so the only honest answer left is to refuse.
#[skuld::test]
#[should_panic(expected = "that wait has no end")]
fn a_stalled_start_under_an_unbounded_budget_refuses_rather_than_pretending() {
    let fake = Fake::new();
    let spec = mk("stalls");
    fake.install(&spec, false).unwrap();
    fake.seed_start_stalls(spec.id.as_str());

    let _ = fake.start(&spec.id, Budget::Unbounded);
}

#[skuld::test]
#[should_panic(expected = "that wait has no end")]
fn a_stalled_stop_under_an_unbounded_budget_refuses_rather_than_pretending() {
    let fake = Fake::new();
    let spec = mk("stalls");
    fake.install(&spec, false).unwrap();
    fake.start(&spec.id, Budget::DEFAULT).unwrap();
    fake.seed_stop_stalls(spec.id.as_str());

    let _ = fake.stop(&spec.id, Budget::Unbounded);
}

/// An un-stalled entry reaches `Running` under every budget that waits,
/// `Unbounded` included — the refusal above is about the stall, not about the
/// budget.
#[skuld::test]
fn an_unstalled_entry_starts_under_every_budget_that_waits() {
    for budget in [Budget::Bounded(Duration::from_secs(10)), Budget::Unbounded] {
        let fake = Fake::new();
        let spec = mk("healthy");
        fake.install(&spec, false).unwrap();

        fake.start(&spec.id, budget)
            .unwrap_or_else(|e| panic!("{budget:?}: {e}"));

        assert_eq!(fake.status(&spec.id).unwrap().state, State::Running, "{budget:?}");
    }
}

/// A budget that does not wait establishes nothing on any real backend —
/// systemd's `start --no-block`, SCM's `request_start` and launchd's plain
/// `kickstart` all return with the service anywhere from not yet forked to
/// running. A `Fake` that settled to `Running` here would let a CLI test
/// assert a state no platform guarantees and stay green, so it settles
/// nothing, even for an entry that would start.
#[skuld::test]
fn an_unstalled_entry_is_left_alone_under_a_budget_that_does_not_wait() {
    for budget in [Budget::Immediate, Budget::Bounded(Duration::ZERO)] {
        let fake = Fake::new();
        let spec = mk("healthy");
        fake.install(&spec, false).unwrap();

        fake.start(&spec.id, budget)
            .unwrap_or_else(|e| panic!("{budget:?} accepts the request: {e}"));

        assert_eq!(
            fake.status(&spec.id).unwrap().state,
            State::Stopped,
            "{budget:?} confirmed nothing, so it must not have changed the state"
        );
    }
}
