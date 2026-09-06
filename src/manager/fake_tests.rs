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
