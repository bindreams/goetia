use std::collections::BTreeMap;

use super::*;
use crate::backend::Identity;
use crate::spec::{DaemonSpec, Kind, Restart, User};

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

fn identity(user: &str) -> Identity {
    Identity { user: user.to_owned() }
}

/// The plist `install` would write for `spec` as `user`.
fn rendered(spec: &DaemonSpec, user: &str) -> String {
    generate::plist(spec, &identity(user))
}

fn changed(spec: &DaemonSpec) -> DaemonSpec {
    let mut changed = spec.clone();
    changed.command.push("--verbose".to_owned());
    changed
}

/// A spec change the plist does not render: `name` is in the embedded blob only.
fn renamed(spec: &DaemonSpec) -> DaemonSpec {
    DaemonSpec {
        name: "another name".to_owned(),
        ..spec.clone()
    }
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
fn an_update_that_changes_the_rendering_changes_the_loaded_job() {
    let spec = spec_fixture();
    let on_disk = rendered(&spec, "root");
    let desired = rendered(&changed(&spec), "root");
    assert!(changes_loaded_job(&update(), Some(&on_disk), &desired));
}

/// `decide` returns `Stale` for a version mismatch before it compares anything else, so a stale
/// artifact can carry a changed spec; the write then changes what the next bootstrap runs.
#[skuld::test]
fn a_stale_artifact_with_a_changed_rendering_changes_the_loaded_job() {
    let spec = spec_fixture();
    let on_disk = rendered(&spec, "root");
    let desired = rendered(&changed(&spec), "root");
    assert!(changes_loaded_job(&stale(), Some(&on_disk), &desired));
}

/// A release that changes the generator changes what launchd reads though the spec is the same.
#[skuld::test]
fn a_stale_artifact_with_the_same_spec_but_another_rendering_changes_the_loaded_job() {
    let spec = spec_fixture();
    let on_disk = rendered(&spec, "root").replace("<key>Label</key>", "<key>Label </key>");
    let desired = rendered(&spec, "root");
    assert!(changes_loaded_job(&stale(), Some(&on_disk), &desired));
}

/// The same `user: {id: ...}` resolves to another account name when the uid's name has changed.
#[skuld::test]
fn another_resolved_account_changes_the_loaded_job() {
    let spec = spec_fixture();
    let on_disk = rendered(&spec, "svc-old");
    let desired = rendered(&spec, "svc-new");
    assert!(changes_loaded_job(&stale(), Some(&on_disk), &desired));
    assert!(changes_loaded_job(&update(), Some(&on_disk), &desired));
}

/// A pure version bump: only the embedded metadata differs.
#[skuld::test]
fn a_stale_artifact_with_an_identical_rendering_does_not() {
    let spec = spec_fixture();
    let desired = rendered(&spec, "root");
    let on_disk = desired.replace(&format!("Version: {}", crate::version()), "Version: 0.0.1");
    assert_ne!(on_disk, desired);
    assert!(!changes_loaded_job(&stale(), Some(&on_disk), &desired));
}

/// Launchd reads nothing of `name`, so the loaded job is as the new plist would have it.
#[skuld::test]
fn an_update_whose_rendering_is_identical_does_not() {
    let spec = spec_fixture();
    let on_disk = rendered(&spec, "root");
    let desired = rendered(&renamed(&spec), "root");
    assert_ne!(on_disk, desired);
    assert!(!changes_loaded_job(&update(), Some(&on_disk), &desired));
}

#[skuld::test]
fn outcomes_that_write_nothing_over_an_installed_plist_do_not() {
    let spec = spec_fixture();
    let on_disk = rendered(&spec, "root");
    let desired = rendered(&changed(&spec), "root");
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
        assert!(!changes_loaded_job(&outcome, Some(&on_disk), &desired), "{outcome:?}");
    }
}

/// `Create` writes a new artifact; a job under its label is booted out before the write, not refused.
#[skuld::test]
fn a_create_does_not() {
    assert!(!changes_loaded_job(
        &Outcome::Create,
        None,
        &rendered(&spec_fixture(), "root")
    ));
}

// loaded_refusal ======================================================================================================

#[skuld::test]
fn the_refusal_names_the_daemon_and_the_remedy() {
    let message = loaded_refusal(&Id::try_from("frpc").unwrap()).to_string();
    assert!(message.contains("`frpc`"), "{message}");
    assert!(message.contains("goetia daemon stop frpc"), "{message}");
    assert!(message.contains("not installed"), "{message}");
    assert!(
        message.contains("plist"),
        "it speaks of the plist, not only the spec: {message}"
    );
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

// state_unknown_after_write ===========================================================================================

/// The plist is written and the second read failed: whether the job is loaded is open, which is the
/// same exit `4` and the same remedy, and the message carries the read's own failure.
#[skuld::test]
fn an_unreadable_recheck_is_in_doubt_and_names_the_remedy_and_the_failure() {
    let read = Error::CommandFailed {
        command: "launchctl print system/frpc".to_owned(),
        stderr: "Could not print: 5: Input/output error".to_owned(),
    };
    let e = state_unknown_after_write(&Id::try_from("frpc").unwrap(), &read);
    assert!(matches!(e, Error::LoadedStateInDoubt { .. }), "{e:?}");
    let message = e.to_string();
    assert!(message.contains("`frpc`"), "{message}");
    assert!(message.contains("goetia daemon stop frpc"), "{message}");
    assert!(message.contains("goetia daemon start frpc"), "{message}");
    assert!(message.contains("Input/output error"), "{message}");
}
