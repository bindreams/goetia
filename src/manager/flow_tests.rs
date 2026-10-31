use super::*;
use crate::decide::{self, ExternalConfig, Outcome};
use crate::error::Error;
use crate::manager::fake::fake_tests::mk;
use crate::manager::fake::{Fake, generate};
use crate::manager::{Installed, ServiceManager};
use crate::spec::{DaemonSpec, Id, User};

// Fixtures ============================================================================================================

fn mk_as(id: &str, user: &str) -> DaemonSpec {
    DaemonSpec {
        user: User::Name(user.to_string()),
        ..mk(id)
    }
}

/// What `Fake::render` fails with for a seeded unresolvable account.
fn unresolvable(user: &str) -> String {
    format!("account `{user}` cannot be resolved (seeded)")
}

// install =============================================================================================================

#[skuld::test]
fn install_retries_a_raced_write_until_it_lands() {
    let fake = Fake::new();
    fake.seed_write_races("frpc", 2);

    let outcome = fake.install(&mk("frpc"), false).unwrap();

    assert_eq!(outcome, Outcome::Create);
    assert_eq!(fake.writes(), 3, "two raced writes, then the one that landed");
    fake.status(&Id::try_from("frpc").unwrap())
        .expect("the artifact must be present");
    let installs = fake
        .guarded()
        .into_iter()
        .filter(|(verb, ..)| *verb == "install")
        .count();
    assert_eq!(installs, 1, "one install scope is held across every retry");
}

#[skuld::test]
fn install_refuses_an_unresolvable_account_with_uninstall_advice() {
    let fake = Fake::new();
    fake.install(&mk_as("frpc", "gone"), false).unwrap();
    fake.seed_unresolvable_account("gone");

    let outcome = fake.install(&mk_as("frpc", "other"), false).unwrap();

    assert_eq!(
        outcome,
        Outcome::RefuseUnreadable {
            reason: format!("cannot regenerate the embedded spec: {}", unresolvable("gone")),
            recovery: Some(decide::unreadable_recovery("frpc")),
        }
    );
}

#[skuld::test]
fn install_refuses_an_undecodable_blob_with_uninstall_advice() {
    let fake = Fake::new();
    fake.seed_unreadable("frpc");

    let outcome = fake.install(&mk("frpc"), false).unwrap();

    match outcome {
        Outcome::RefuseUnreadable { recovery, .. } => {
            assert_eq!(recovery, Some(decide::unreadable_recovery("frpc")));
        }
        other => panic!("expected RefuseUnreadable, got {other:?}"),
    }
}

#[skuld::test]
fn install_refuses_a_failed_comparison_with_its_error_alone() {
    let fake = Fake::new();
    fake.install(&mk("frpc"), false).unwrap();
    fake.seed_installed_fails("frpc", "the unit could not be read back");

    let outcome = fake.install(&mk("frpc"), false).unwrap();

    match outcome {
        Outcome::RefuseUnreadable { reason, recovery } => {
            assert!(reason.contains("the unit could not be read back"), "{reason}");
            assert_eq!(recovery, None);
        }
        other => panic!("expected RefuseUnreadable, got {other:?}"),
    }
}

/// The write must notice the id changed since it was read. A flow that retried the write with the
/// stale read would overwrite what appeared.
#[skuld::test]
fn a_write_that_loses_to_a_foreign_entry_refuses_and_overwrites_nothing() {
    let fake = Fake::new();
    fake.seed_write_interloper("frpc", "some pre-existing, unrelated service\n");

    let outcome = fake.install(&mk("frpc"), false).unwrap();

    assert!(matches!(outcome, Outcome::RefuseForeign { .. }), "{outcome:?}");
    assert_eq!(fake.writes(), 1, "the raced write is not retried over a foreign entry");
    let after = fake.preview_install(&mk("frpc")).unwrap();
    assert!(matches!(after, Outcome::RefuseForeign { .. }), "{after:?}");
}

/// What the interloper wrote is read afresh and decided on, whichever of `decide`'s rows it hits.
#[skuld::test]
fn a_write_that_loses_to_an_ours_entry_follows_that_entry() {
    let theirs = mk("frpc");
    let mine = mk_as("frpc", "svc");
    let hand_edited = format!("{}# hand-added directive\n", generate(&theirs));

    // Same spec: nothing is left to write.
    let fake = Fake::new();
    fake.seed_write_interloper("frpc", generate(&theirs));
    assert_eq!(fake.install(&theirs, false).unwrap(), Outcome::UpToDate);
    assert_eq!(fake.writes(), 1);

    // A clean entry of another spec: updated, and the update lands.
    let fake = Fake::new();
    fake.seed_write_interloper("frpc", generate(&theirs));
    let outcome = fake.install(&mine, false).unwrap();
    assert!(matches!(outcome, Outcome::Update { .. }), "{outcome:?}");
    assert_eq!(fake.writes(), 2, "the raced write, then the one over the new state");
    assert_eq!(fake.preview_install(&mine).unwrap(), Outcome::UpToDate);

    // A hand-edited entry: refused as a conflict, and the edit survives.
    let fake = Fake::new();
    fake.seed_write_interloper("frpc", hand_edited);
    let outcome = fake.install(&mine, false).unwrap();
    assert!(matches!(outcome, Outcome::Conflict { .. }), "{outcome:?}");
    assert_eq!(fake.writes(), 1);
    let after = fake.preview_install(&mine).unwrap();
    assert!(matches!(after, Outcome::Conflict { .. }), "{after:?}");
}

/// The recovery for configuration a write will not remove reaches the user from `install` and
/// `preview_install`; only `--force` over what a write does remove goes through.
#[skuld::test]
fn external_configuration_is_carried_into_the_outcome() {
    let recovery = "remove /etc/drop-in by hand".to_string();

    let hand_edited = Fake::new();
    hand_edited.install_then_hand_edit(&mk("frpc"), "# hand-added directive\n");
    hand_edited.seed_external(
        "frpc",
        ExternalConfig::Unclearable {
            recovery: recovery.clone(),
        },
    );
    for outcome in [
        hand_edited.install(&mk_as("frpc", "svc"), false).unwrap(),
        hand_edited.preview_install(&mk_as("frpc", "svc")).unwrap(),
    ] {
        match outcome {
            Outcome::Conflict {
                unclearable_recovery, ..
            } => assert_eq!(unclearable_recovery, Some(recovery.clone())),
            other => panic!("expected Conflict, got {other:?}"),
        }
    }

    // A stale entry that carries external configuration is a conflict unless forced.
    let stale = Fake::new();
    stale.seed_stale(&mk("frpc"), "0.0.1");
    stale.seed_external("frpc", ExternalConfig::Clearable);
    for outcome in [
        stale.install(&mk("frpc"), false).unwrap(),
        stale.preview_install(&mk("frpc")).unwrap(),
    ] {
        match outcome {
            Outcome::Conflict {
                unclearable_recovery, ..
            } => assert_eq!(unclearable_recovery, None),
            other => panic!("expected Conflict, got {other:?}"),
        }
    }
    let forced = stale.install(&mk("frpc"), true).unwrap();
    assert!(matches!(forced, Outcome::Stale { .. }), "{forced:?}");
}

// status and list =====================================================================================================

#[skuld::test]
fn status_reports_an_unresolvable_account_as_an_error() {
    let fake = Fake::new();
    fake.install(&mk_as("frpc", "gone"), false).unwrap();
    fake.seed_unresolvable_account("gone");

    let err = fake.status(&Id::try_from("frpc").unwrap()).unwrap_err();

    match err {
        Error::Other(message) => assert_eq!(
            message,
            format!("`frpc`'s embedded spec cannot be regenerated: {}", unresolvable("gone"))
        ),
        other => panic!("expected Error::Other, got {other:?}"),
    }
}

#[skuld::test]
fn list_reports_an_unresolvable_account_as_ours_unreadable() {
    let fake = Fake::new();
    fake.install(&mk_as("frpc", "gone"), false).unwrap();
    fake.seed_unresolvable_account("gone");

    let listed = fake.list().unwrap();

    assert_eq!(
        listed,
        [Installed::OursUnreadable {
            name: "frpc".to_string(),
            reason: format!(
                "marked ours, but its embedded spec cannot be regenerated: {}",
                unresolvable("gone")
            ),
        }]
    );
}

#[skuld::test]
fn list_reports_a_failed_live_status_as_ours_unreadable() {
    let fake = Fake::new();
    fake.install(&mk("frpc"), false).unwrap();
    fake.install(&mk("other"), false).unwrap();
    fake.seed_live_status_fails("frpc", "state unavailable");

    let listed = fake.list().unwrap();

    assert_eq!(
        listed.len(),
        2,
        "one entry's failure must not take down the others: {listed:?}"
    );
    assert!(
        listed.contains(&Installed::OursUnreadable {
            name: "frpc".to_string(),
            reason: "marked ours, but its live state could not be queried: state unavailable".to_string(),
        }),
        "{listed:?}"
    );
    assert!(
        listed
            .iter()
            .any(|e| matches!(e, Installed::Ours { spec, .. } if spec.id.as_str() == "other")),
        "{listed:?}"
    );
    match fake.status(&Id::try_from("frpc").unwrap()) {
        Err(Error::Other(message)) => assert_eq!(message, "state unavailable"),
        other => panic!("status of the same id must return the error, got {other:?}"),
    }
}

#[skuld::test]
fn list_reports_a_name_that_is_not_an_id_without_querying_it() {
    let fake = Fake::new();
    let name = "not a valid id";
    fake.seed_foreign(name, generate(&mk("frpc")));
    fake.install(&mk("other"), false).unwrap();
    let invalid = Id::try_from(name).unwrap_err();

    let listed = fake.list().unwrap();

    assert_eq!(listed.len(), 2, "{listed:?}");
    assert!(
        listed.contains(&Installed::OursUnreadable {
            name: name.to_string(),
            reason: format!("marked ours, but its name is not a valid daemon id: {invalid}"),
        }),
        "{listed:?}"
    );
    assert!(
        listed
            .iter()
            .any(|e| matches!(e, Installed::Ours { spec, .. } if spec.id.as_str() == "other")),
        "{listed:?}"
    );
}

#[skuld::test]
fn list_omits_an_entry_that_vanished_before_its_live_query() {
    let fake = Fake::new();
    fake.install(&mk("frpc"), false).unwrap();
    fake.install(&mk("other"), false).unwrap();
    fake.seed_live_status_gone("frpc");

    let listed = fake.list().unwrap();

    assert_eq!(listed.len(), 1, "{listed:?}");
    assert!(
        matches!(&listed[0], Installed::Ours { spec, .. } if spec.id.as_str() == "other"),
        "{listed:?}"
    );
}

#[skuld::test]
fn list_aborts_on_no_manager() {
    let fake = Fake::new();
    fake.install(&mk("frpc"), false).unwrap();
    fake.seed_no_manager();

    let result = fake.list();

    assert!(matches!(result, Err(Error::NoManager { .. })), "{result:?}");
}

#[skuld::test]
fn status_and_gate_carry_a_backend_foreign_recovery() {
    let fake = Fake::new();
    fake.seed_residual_artifact("leftover");
    fake.seed_residual_recovery("advised", "remove the leftover by hand");

    for (id, recovery) in [
        ("leftover", decide::foreign_recovery("leftover")),
        ("advised", "remove the leftover by hand".to_string()),
    ] {
        let id = Id::try_from(id).unwrap();
        for (what, result) in [
            ("status", fake.status(&id).map(drop)),
            ("owned", owned(&fake, &id).map(drop)),
        ] {
            match result {
                Err(Error::Foreign { recovery: got, .. }) => assert_eq!(got, recovery, "{what} of {id}"),
                other => panic!("{what} of {id} must be Foreign, got {other:?}"),
            }
        }
    }
}

// owned ===============================================================================================================

#[skuld::test]
fn owned_maps_absent_foreign_and_marked() {
    let fake = Fake::new();
    fake.seed_foreign("stranger", "not a goetia artifact\n");
    fake.install(&mk("ours"), false).unwrap();
    fake.seed_unreadable("corrupt");
    let id = |s: &str| Id::try_from(s).unwrap();

    assert!(matches!(owned(&fake, &id("nobody")), Err(Error::NotInstalled { .. })));
    assert!(matches!(owned(&fake, &id("stranger")), Err(Error::Foreign { .. })));
    assert!(owned(&fake, &id("ours")).is_ok());
    assert!(
        owned(&fake, &id("corrupt")).is_ok(),
        "the marker alone is proof of ownership"
    );
}

/// A verb takes the snapshot the gate yields, so the blanket impl alone gates every verb: on an id
/// that is not goetia's each says why, and the backend is never asked to act.
#[skuld::test]
fn every_verb_on_an_id_that_is_not_ours_reports_why_and_performs_nothing() {
    let fake = Fake::new();
    fake.seed_native_restart();
    fake.seed_foreign("stranger", "not a goetia artifact\n");
    let absent = Id::try_from("nobody").unwrap();
    let foreign = Id::try_from("stranger").unwrap();
    let budget = crate::manager::Budget::DEFAULT;
    let mgr: &dyn ServiceManager = &fake;
    type Verb<'a> = (&'a str, &'a dyn Fn(&Id) -> Result<()>);
    let verbs: [Verb; 9] = [
        ("uninstall", &|id| mgr.uninstall(id)),
        ("enable", &|id| mgr.enable(id)),
        ("disable", &|id| mgr.disable(id)),
        ("start", &|id| mgr.start(id, budget)),
        ("stop", &|id| mgr.stop(id, budget)),
        ("kill", &|id| mgr.kill(id)),
        ("request_restart", &|id| {
            mgr.request_restart(id).expect("this fake restarts natively")
        }),
        ("request_start_after_stop", &|id| mgr.request_start_after_stop(id)),
        ("status", &|id| mgr.status(id).map(drop)),
    ];

    for (verb, run) in verbs {
        let result = run(&absent);
        assert!(
            matches!(&result, Err(Error::NotInstalled { id }) if id == "nobody"),
            "{verb} of an absent id: {result:?}"
        );
        let result = run(&foreign);
        assert!(
            matches!(&result, Err(Error::Foreign { id, .. }) if id == "stranger"),
            "{verb} of a foreign id: {result:?}"
        );
    }

    assert_eq!(fake.calls(), [], "no verb reached the backend");
    assert!(fake.guarded().is_empty());
    assert_eq!(fake.writes(), 0);
    let after = fake.preview_install(&mk("stranger")).unwrap();
    assert!(matches!(after, Outcome::RefuseForeign { .. }), "{after:?}");
}

// scan ================================================================================================================

type Names = std::vec::IntoIter<std::io::Result<String>>;

fn named(items: Vec<std::io::Result<String>>) -> std::io::Result<Names> {
    Ok(items.into_iter())
}

fn op_and_cause(op: &'static str, e: &std::io::Error) -> String {
    format!("{op} failed: {e}")
}

/// Collecting into a `Result` instead would discard every name enumerated before a later one
/// faulted, which reports each of them absent.
#[skuld::test]
fn scan_keeps_entries_before_a_mid_pass_fault() {
    let items = vec![
        Ok("named-before-the-fault".to_string()),
        Err(std::io::Error::other("injected mid-scan failure")),
        Ok("never-reached".to_string()),
    ];
    let mut kept = Vec::new();

    let reported = scan(
        "the place",
        named(items),
        |names| Box::new(names),
        op_and_cause,
        |name| kept.push(name),
    );

    assert_eq!(kept, ["named-before-the-fault"], "the pass stops at the fault");
    match reported {
        Some(Installed::Undetermined { name: None, reason }) => {
            assert!(reason.contains("the place"), "{reason}");
            assert!(
                reason.contains("enumerate failed: injected mid-scan failure"),
                "{reason}"
            );
        }
        other => panic!("a pass that stopped early must say what it did not reach, got {other:?}"),
    }
}

/// Absence is established: an empty scan with nothing outstanding, not an unfinished one.
#[skuld::test]
fn scan_treats_a_missing_location_as_empty() {
    let mut kept = Vec::new();

    let reported = scan::<Names, String>(
        "the place",
        Err(std::io::ErrorKind::NotFound.into()),
        |names| Box::new(names),
        op_and_cause,
        |name| kept.push(name),
    );

    assert!(kept.is_empty(), "{kept:?}");
    assert_eq!(reported, None);
}

/// The outcome that must never be confused with the one above: a location that is there and will
/// not open leaves the pass unfinished, so no id may be concluded absent.
#[skuld::test]
fn scan_reports_a_location_that_will_not_open() {
    let mut kept = Vec::new();

    let reported = scan::<Names, String>(
        "the place",
        Err(std::io::ErrorKind::PermissionDenied.into()),
        |names| Box::new(names),
        op_and_cause,
        |name| kept.push(name),
    );

    assert!(kept.is_empty(), "{kept:?}");
    match reported {
        Some(Installed::Undetermined { name: None, reason }) => {
            assert!(reason.contains("the place"), "{reason}");
            assert!(reason.contains("open failed:"), "{reason}");
        }
        other => panic!("a location that exists and will not open establishes no absence, got {other:?}"),
    }
}

#[skuld::test]
fn scan_that_finishes_reports_nothing() {
    let mut kept = Vec::new();

    let reported = scan(
        "the place",
        named(vec![Ok("a".to_string()), Ok("b".to_string())]),
        |names| Box::new(names),
        op_and_cause,
        |name| kept.push(name),
    );

    assert_eq!(kept, ["a", "b"]);
    assert_eq!(reported, None);
}

/// The text of a directory pass that stopped, which every directory-scanning backend hands
/// `scan` as its `detail`. Injected here because no real `readdir` fails on request.
#[cfg(unix)]
#[skuld::test]
fn a_directory_pass_that_stopped_is_worded_by_where_it_stopped() {
    let e = std::io::Error::other("injected mid-scan failure");

    assert_eq!(
        directory_detail("enumerate", &e),
        "failed to read a directory entry: injected mid-scan failure"
    );
    assert_eq!(
        directory_detail("open", &e),
        "failed to read directory: injected mid-scan failure"
    );
}
