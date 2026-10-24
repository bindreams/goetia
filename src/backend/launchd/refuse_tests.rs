use std::collections::BTreeMap;

use super::*;
use crate::blob::{self, Blob};
use crate::spec::{Kind, Restart, User};

fn spec_fixture() -> DaemonSpec {
    DaemonSpec {
        id: Id::try_from("frpc").unwrap(),
        name: "FRP tunnel client".to_owned(),
        command: vec!["frpc".to_owned()],
        cwd: None,
        env: BTreeMap::new(),
        user: User::Root,
        restart: Restart::Always,
        restart_delay: None,
        logs: None,
        kind: Kind::Simple,
    }
}

fn installed(spec: &DaemonSpec) -> Ownership {
    Ownership::Ours {
        blob: Blob {
            schema: blob::SCHEMA,
            version: "0.0.1".to_owned(),
            spec: spec.clone(),
        },
        regenerated: String::new(),
    }
}

fn changed(spec: &DaemonSpec) -> DaemonSpec {
    let mut changed = spec.clone();
    changed.command.push("--verbose".to_owned());
    changed
}

fn update() -> Outcome {
    Outcome::Update {
        spec_diff: String::new(),
    }
}

fn stale() -> Outcome {
    Outcome::Stale {
        from_version: "0.0.1".to_owned(),
    }
}

// changes_loaded_job ==================================================================================================

/// `Update` is also what `--force` turns a `Conflict` into, so this covers the forced overwrite.
#[skuld::test]
fn an_update_changes_the_loaded_job() {
    let spec = spec_fixture();
    assert!(changes_loaded_job(&update(), &installed(&spec), &changed(&spec)));
}

/// `decide` returns `Stale` for a version mismatch before it compares anything else, so a stale
/// artifact can carry a changed spec; the write then changes what the next bootstrap runs.
#[skuld::test]
fn a_stale_artifact_with_a_changed_spec_changes_the_loaded_job() {
    let spec = spec_fixture();
    assert!(changes_loaded_job(&stale(), &installed(&spec), &changed(&spec)));
}

#[skuld::test]
fn a_stale_artifact_with_the_same_spec_does_not() {
    let spec = spec_fixture();
    assert!(!changes_loaded_job(&stale(), &installed(&spec), &spec));
}

#[skuld::test]
fn outcomes_that_write_nothing_over_an_installed_plist_do_not() {
    let spec = spec_fixture();
    let found = installed(&spec);
    let changed = changed(&spec);
    for outcome in [
        Outcome::UpToDate,
        Outcome::Conflict {
            artifact_diff: String::new(),
            unclearable_recovery: None,
        },
        Outcome::RefuseForeign {
            recovery: String::new(),
        },
        Outcome::RefuseUnreadable {
            reason: String::new(),
            recovery: String::new(),
        },
    ] {
        assert!(!changes_loaded_job(&outcome, &found, &changed), "{outcome:?}");
    }
}

/// `Create` writes a new artifact; the stale job under its label is booted out, not refused.
#[skuld::test]
fn a_create_does_not() {
    assert!(!changes_loaded_job(
        &Outcome::Create,
        &Ownership::Absent,
        &spec_fixture()
    ));
}

// loaded_refusal ======================================================================================================

#[skuld::test]
fn the_refusal_names_the_daemon_and_the_remedy() {
    let message = loaded_refusal(&Id::try_from("frpc").unwrap()).to_string();
    assert!(message.contains("`frpc`"), "{message}");
    assert!(message.contains("goetia daemon stop frpc"), "{message}");
    assert!(message.contains("not installed"), "{message}");
}

// loaded_after_write ==================================================================================================

/// The job was not loaded at the first check and is loaded now: goetia cannot tell which definition
/// it read, so the install is in doubt (exit `4`), not a success and not a refusal.
#[skuld::test]
fn a_job_loaded_after_the_write_is_in_doubt_and_names_the_remedy() {
    let e = loaded_after_write(&Id::try_from("frpc").unwrap());
    assert!(matches!(e, Error::LoadedJobInDoubt { .. }), "{e:?}");
    let message = e.to_string();
    assert!(message.contains("`frpc`"), "{message}");
    assert!(message.contains("goetia daemon stop frpc"), "{message}");
    assert!(message.contains("goetia daemon start frpc"), "{message}");
}
