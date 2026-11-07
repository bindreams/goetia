use std::io;

use windows_sys::Win32::Foundation::{ERROR_ACCESS_DENIED, ERROR_INVALID_OWNER, ERROR_NOT_ENOUGH_MEMORY};

use super::*;

/// The lock's owner is Administrators, which a token without it enabled (an unelevated one) cannot
/// give an object: `CreateMutexW` fails with `ERROR_INVALID_OWNER`. That is a need for elevation.
#[skuld::test]
fn a_lock_the_token_cannot_own_is_a_need_for_elevation() {
    let error = create_failed(io::Error::from_raw_os_error(ERROR_INVALID_OWNER as i32));

    assert!(matches!(error, Error::ElevationRequired { .. }), "{error:?}");
    assert_eq!(
        error.to_string(),
        "creating or opening the install lock `Global\\goetia-shim` requires elevation (root/Administrator): re-run \
         as root or Administrator"
    );
    assert!(!error.is_indeterminate());
}

/// An unelevated caller that opens a lock an elevated process made is refused by its DACL, which
/// admits only SYSTEM and Administrators. An elevated caller refused was shut out by whoever made
/// the lock: that is a foreign lock, not a need for elevation.
#[skuld::test]
fn access_denied_is_a_need_for_elevation_only_for_a_caller_without_administrators() {
    let denied = Some(ERROR_ACCESS_DENIED as i32);

    assert_eq!(failure_kind(denied, false), FailureKind::NeedsElevation);
    assert_eq!(failure_kind(denied, true), FailureKind::ForeignLock);
}

/// Every other code is the OS's, whoever the caller is.
#[skuld::test]
fn any_other_code_is_the_os_failure() {
    for elevated in [false, true] {
        for code in [Some(ERROR_NOT_ENOUGH_MEMORY as i32), Some(1), None] {
            assert_eq!(
                failure_kind(code, elevated),
                FailureKind::Os,
                "{code:?}, elevated {elevated}"
            );
        }
    }
    assert_eq!(
        failure_kind(Some(ERROR_INVALID_OWNER as i32), true),
        FailureKind::NeedsElevation
    );
}

/// Any other failure is the OS's, as it was.
#[skuld::test]
fn any_other_failure_to_create_the_lock_is_the_os_error() {
    for code in [ERROR_NOT_ENOUGH_MEMORY, 1] {
        let error = create_failed(io::Error::from_raw_os_error(code as i32));

        let Error::Os { what, source, .. } = &error else {
            panic!("expected the OS error, got {error:?}");
        };
        assert!(what.contains(NAME), "{what}");
        assert_eq!(
            source.downcast_ref::<io::Error>().and_then(io::Error::raw_os_error),
            Some(code as i32)
        );
    }
}
