//! The pin's open modes, over files in a temporary directory.

use std::fs::{self, OpenOptions};
use std::os::windows::fs::OpenOptionsExt as _;

use windows_sys::Win32::Storage::FileSystem::{FILE_SHARE_DELETE, FILE_SHARE_WRITE};

use super::*;
use crate::backend::scm::test_support::{delete_pending, directory_with_a_file};

#[skuld::test]
fn a_pin_with_a_handle_open_is_held() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("pin-a.pin");
    let holder = create_pin(&path).unwrap();

    assert!(is_held(&path).unwrap());
    drop(holder);
    assert!(!path.exists(), "a pin goes when its holder closes it");
}

/// The holder shares no delete, so another open for `DELETE` (what a delete is) is a sharing violation and
/// the file stays, though it is deleted when the holder closes it.
#[skuld::test]
fn a_held_pin_cannot_be_opened_for_delete_or_removed_by_name() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("pin-a.pin");
    let holder = create_pin(&path).unwrap();

    let opened = OpenOptions::new()
        .access_mode(DELETE)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
        .open(&path)
        .unwrap_err();
    let removed = fs::remove_file(&path).unwrap_err();

    assert!(is_sharing_violation(&opened), "{opened:?}");
    assert!(is_sharing_violation(&removed), "{removed:?}");
    assert!(path.exists());
    drop(holder);
    assert!(!path.exists());
}

#[skuld::test]
fn a_pin_nothing_holds_is_stale_and_goes_and_one_that_is_gone_is_not_held() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("pin-a.pin");
    fs::write(&path, "").unwrap();

    assert!(!is_held(&path).unwrap());
    assert!(!path.exists(), "the stale pin is deleted");
    assert!(!is_held(&path).unwrap(), "a pin that is gone is not held");
}

/// Measured: a file whose delete is pending answers a delete with access denied, as a permission that
/// refuses it does (the Win32 error is 5 for both); only the status under it, `STATUS_DELETE_PENDING`,
/// tells them apart. It is deleted as far as anything can do it: it goes when the last handle closes.
/// A delete that fails for any other reason is a failure.
#[skuld::test]
fn a_file_whose_delete_is_pending_is_deleted_and_any_other_failure_is_a_failure() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("pin-a.pin");
    let reader = delete_pending(&path);

    let pending = delete(&path);
    let pin = is_held(&path);

    assert!(matches!(pending, DeleteOutcome::Deleted));
    assert!(!pin.unwrap(), "a pin whose delete is pending is not held");
    drop(reader);
    assert!(!path.exists());

    let refused = dir.path().join("pin-b.pin");
    directory_with_a_file(&refused);
    assert!(
        matches!(delete(&refused), DeleteOutcome::Failed(_)),
        "a delete that fails for no handle and no pending delete is a failure"
    );
    assert!(is_held(&refused).is_err());
}

#[skuld::test]
fn a_file_a_handle_holds_without_sharing_delete_is_held_and_one_that_is_gone_is_deleted() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("f.tmp");
    let holder = create_temp(&path).unwrap();

    assert!(matches!(delete(&path), DeleteOutcome::Held));
    drop(holder);
    assert!(matches!(delete(&path), DeleteOutcome::Deleted));
    assert!(
        matches!(delete(&path), DeleteOutcome::Deleted),
        "a file that is not there"
    );
}
