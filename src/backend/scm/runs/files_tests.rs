//! The pin's open modes, over files in a temporary directory.

use std::fs::{self, OpenOptions};
use std::io;
use std::os::windows::fs::OpenOptionsExt as _;

use windows_sys::Win32::Storage::FileSystem::{FILE_SHARE_DELETE, FILE_SHARE_WRITE};

use super::*;
use crate::backend::scm::test_support::delete_pending;

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

/// Measured: a file whose delete is pending fails a delete with access denied, not with its own error, so
/// it says nothing about a handle: any failure but a sharing violation is the caller's to judge.
#[skuld::test]
fn a_file_whose_delete_is_pending_fails_with_access_denied() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("pin-a.pin");
    let reader = delete_pending(&path);

    let failed = is_held(&path).unwrap_err();

    assert_eq!(failed.kind(), io::ErrorKind::PermissionDenied, "{failed:?}");
    drop(reader);
}
