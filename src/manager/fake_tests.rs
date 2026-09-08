use std::collections::BTreeMap;

use super::*;
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

    conformance::run(&fake, &mk);
}

// Behavior the six conformance scenarios do not exercise ==============================================================

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
    fake.start(&spec.id).unwrap();

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

    fake.stop(&spec.id).unwrap();
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
    assert!(fake.start(&id).is_err());
    assert!(fake.stop(&id).is_err());
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
    assert!(fake.start(&id).is_err(), "start must refuse a foreign id");
    assert!(fake.stop(&id).is_err(), "stop must refuse a foreign id");
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
        ("start", fake.start(&id)),
        ("stop", fake.stop(&id)),
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
        ("start", fake.start(&spec.id)),
        ("stop", fake.stop(&spec.id)),
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
/// here checks it — the shadowing test above never calls `list`, and the
/// two `list` tests use ids that are opaque *or* installed, never both — so
/// without this, `list` can report one id twice, as unclassifiable and as
/// goetia's at once.
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
