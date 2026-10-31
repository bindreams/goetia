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

// changes_loaded_job ==================================================================================================

/// An `Update` (also what `--force` turns a `Conflict` into) or a `Stale` artifact, which `decide`
/// returns for a version mismatch before it compares anything else, so it can carry a changed spec.
#[skuld::test]
fn a_changed_rendering_changes_the_loaded_job() {
    let spec = spec_fixture();
    let on_disk = rendered(&spec, "root");
    let desired = rendered(&changed(&spec), "root");
    assert!(changes_loaded_job(&on_disk, &desired));
}

/// A release that changes the generator changes what launchd reads though the spec is the same.
#[skuld::test]
fn the_same_spec_with_another_rendering_changes_the_loaded_job() {
    let spec = spec_fixture();
    let on_disk = rendered(&spec, "root").replace("<key>Label</key>", "<key>Label </key>");
    let desired = rendered(&spec, "root");
    assert!(changes_loaded_job(&on_disk, &desired));
}

/// The same `user: {id: ...}` resolves to another account name when the uid's name has changed.
#[skuld::test]
fn another_resolved_account_changes_the_loaded_job() {
    let spec = spec_fixture();
    let on_disk = rendered(&spec, "svc-old");
    let desired = rendered(&spec, "svc-new");
    assert!(changes_loaded_job(&on_disk, &desired));
}

/// A pure version bump: only the embedded metadata differs.
#[skuld::test]
fn an_identical_rendering_with_another_version_does_not() {
    let spec = spec_fixture();
    let desired = rendered(&spec, "root");
    let on_disk = desired.replace(&format!("Version: {}", crate::version()), "Version: 0.0.1");
    assert_ne!(on_disk, desired);
    assert!(!changes_loaded_job(&on_disk, &desired));
}

/// Launchd reads nothing of `name`, so the loaded job is as the new plist would have it.
#[skuld::test]
fn a_spec_change_the_rendering_does_not_show_does_not() {
    let spec = spec_fixture();
    let on_disk = rendered(&spec, "root");
    let desired = rendered(&renamed(&spec), "root");
    assert_ne!(on_disk, desired);
    assert!(!changes_loaded_job(&on_disk, &desired));
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

fn restore_failure() -> Error {
    Error::Io {
        path: "/Library/LaunchDaemons/frpc.plist".into(),
        source: std::io::Error::from_raw_os_error(5),
    }
}

/// The job was not loaded at the first check and is loaded now: goetia cannot tell which definition
/// it read, so the install is in doubt (exit `4`), not a success and not a refusal. The plist is
/// back as it was, so the remedy is the one for a refusal: stop, install again, start.
#[skuld::test]
fn a_job_loaded_after_the_write_is_in_doubt_and_names_the_remedy() {
    let e = loaded_after_write(&Id::try_from("frpc").unwrap(), Ok(()));
    assert!(matches!(e, Error::LoadedJobInDoubt { .. }), "{e:?}");
    let message = e.to_string();
    assert!(message.contains("`frpc`"), "{message}");
    assert!(message.contains("goetia daemon stop frpc"), "{message}");
    assert!(message.contains("goetia daemon start frpc"), "{message}");
    assert!(message.contains("re-run install"), "{message}");
    assert!(message.contains("plist is back as it was"), "{message}");
}

/// A plist that could not be put back is not "back": the message carries the failure, which says
/// what is on disk, and never claims the restore.
#[skuld::test]
fn a_job_in_doubt_whose_plist_could_not_be_restored_carries_the_failure() {
    let e = loaded_after_write(&Id::try_from("frpc").unwrap(), Err(restore_failure()));
    assert!(matches!(e, Error::LoadedJobInDoubt { .. }), "{e:?}");
    let message = e.to_string();
    assert!(message.contains("could not be put back"), "{message}");
    assert!(message.contains("/Library/LaunchDaemons/frpc.plist"), "{message}");
    assert!(message.contains("goetia daemon stop frpc"), "{message}");
    assert!(message.contains("goetia daemon start frpc"), "{message}");
    assert!(!message.contains("plist is back as it was"), "{message}");
}

// state_unknown_after_write ===========================================================================================

/// The plist was written and the second read failed: whether the job is loaded is open, which is the
/// same exit `4` and the same remedy, and the message carries the read's own failure.
#[skuld::test]
fn an_unreadable_recheck_is_in_doubt_and_names_the_remedy_and_the_failure() {
    let read = Error::CommandFailed {
        command: "launchctl print system/frpc".to_owned(),
        stderr: "Could not print: 5: Input/output error".to_owned(),
    };
    let e = state_unknown_after_write(&Id::try_from("frpc").unwrap(), &read, Ok(()));
    assert!(matches!(e, Error::LoadedStateInDoubt { .. }), "{e:?}");
    let message = e.to_string();
    assert!(message.contains("`frpc`"), "{message}");
    assert!(message.contains("goetia daemon stop frpc"), "{message}");
    assert!(message.contains("goetia daemon start frpc"), "{message}");
    assert!(message.contains("Input/output error"), "{message}");
    assert!(message.contains("re-run install"), "{message}");
    assert!(message.contains("plist is back as it was"), "{message}");
}

#[skuld::test]
fn an_unreadable_recheck_whose_plist_could_not_be_restored_carries_both_failures() {
    let read = Error::CommandFailed {
        command: "launchctl print system/frpc".to_owned(),
        stderr: "Could not print: 5: Input/output error".to_owned(),
    };
    let e = state_unknown_after_write(&Id::try_from("frpc").unwrap(), &read, Err(restore_failure()));
    assert!(matches!(e, Error::LoadedStateInDoubt { .. }), "{e:?}");
    let message = e.to_string();
    assert!(message.contains("Input/output error"), "{message}");
    assert!(message.contains("could not be put back"), "{message}");
    assert!(!message.contains("plist is back as it was"), "{message}");
}

// slot_taken_after_bootout ============================================================================================

/// The stale job is gone and the plist's place was taken by someone else: the message says both, names
/// what is there, and does not claim that nothing was done.
#[skuld::test]
fn a_plist_taken_after_the_bootout_says_what_was_done_and_what_is_there() {
    let path = std::path::Path::new("/Library/Application Support/Goetia/daemons/frpc.plist");
    let e = slot_taken_after_bootout(&Id::try_from("frpc").unwrap(), path, "a directory");
    assert!(matches!(e, Error::StaleJobBootedOut { .. }), "{e:?}");
    let message = e.to_string();
    assert!(message.contains("`frpc`"), "{message}");
    assert!(message.contains("booted out"), "{message}");
    assert!(message.contains("a directory"), "{message}");
    assert!(message.contains(&path.display().to_string()), "{message}");
    assert!(message.contains("re-run install"), "{message}");
    assert!(!message.contains("nothing was"), "{message}");
}

// changed_during_install ==============================================================================================

/// A plist that changed under `install` after a read: a plain failure that says no plist was written and
/// to run `install` again.
#[skuld::test]
fn a_plist_changed_during_install_is_a_plain_failure_that_says_to_rerun() {
    let path = std::path::Path::new("/Library/Application Support/Goetia/daemons/frpc.plist");
    let e = changed_during_install(&Id::try_from("frpc").unwrap(), path);
    assert!(matches!(e, Error::Other(_)), "{e:?}");
    let message = e.to_string();
    assert!(message.contains("`frpc`"), "{message}");
    assert!(message.contains(&path.display().to_string()), "{message}");
    assert!(message.contains("no plist was written"), "{message}");
    assert!(message.contains("re-run"), "{message}");
}
