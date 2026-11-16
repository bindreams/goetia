//! The pin's open modes, over files in a temporary directory.

use std::fs::{self, OpenOptions};
use std::io;

use super::*;

#[skuld::test]
fn a_pin_with_a_handle_open_is_held() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("pin-a.pin");
    let holder = create_pin(&path).unwrap();

    assert!(is_held(&path).unwrap());
    drop(holder);
    assert!(!path.exists(), "a pin goes when its holder closes it");
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

/// Measured: a file whose delete is pending (its holder closed it while another handle was open) fails an
/// open with access denied, as a file with no entry for the account does, not with its own error. So
/// neither says a handle holds it: both are the caller's to judge.
#[skuld::test]
fn a_pin_whose_delete_is_pending_fails_the_open_with_access_denied() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("pin-a.pin");
    let holder = create_pin(&path).unwrap();
    let reader = OpenOptions::new().read(true).open(&path).unwrap();
    drop(holder);

    let failed = is_held(&path).unwrap_err();

    assert_eq!(failed.kind(), io::ErrorKind::PermissionDenied, "{failed:?}");
    drop(reader);
}
