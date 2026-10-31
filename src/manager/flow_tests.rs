use std::collections::BTreeMap;

use super::*;
use crate::decide::{self, Outcome};
use crate::error::Error;
use crate::manager::fake::Fake;
use crate::manager::{Installed, ServiceManager};
use crate::spec::{DaemonSpec, Id, Kind, Restart, User};

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

#[skuld::test]
fn install_refuses_foreign_without_comparing() {
    let fake = Fake::new();
    fake.seed_foreign("frpc", "some pre-existing, unrelated service\n");
    fake.seed_installed_fails("frpc", "must not be asked");

    let outcome = fake.install(&mk("frpc"), false).unwrap();

    assert!(matches!(outcome, Outcome::RefuseForeign { .. }), "{outcome:?}");
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
