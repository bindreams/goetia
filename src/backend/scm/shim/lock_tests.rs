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

/// A caller whose token lacks Administrators that opens a lock an elevated process made is refused
/// by its DACL, which admits only SYSTEM and Administrators. A caller whose token has them, refused,
/// was shut out by whoever made the lock.
#[skuld::test]
fn access_denied_is_a_need_for_elevation_only_for_a_token_without_administrators() {
    let denied = Some(ERROR_ACCESS_DENIED as i32);

    assert_eq!(failure_kind(denied, false), FailureKind::NeedsElevation);
    assert_eq!(failure_kind(denied, true), FailureKind::ForeignLock);
}

/// A denial for a token with Administrators enabled is the lock's permissions shutting them out,
/// whoever owns it: the sentence says that and names the owner when it was read, and says nothing of
/// trust.
#[skuld::test]
fn a_denied_administrator_is_told_the_locks_permissions_shut_them_out() {
    let denied = || io::Error::from_raw_os_error(ERROR_ACCESS_DENIED as i32);

    let readable = failure(denied(), || Ok(true), || Some(r"EVE\eve".to_string()));
    let unreadable = failure(denied(), || Ok(true), || None);

    assert_eq!(
        readable.to_string(),
        "the install lock `Global\\goetia-shim` is owned by EVE\\eve and its permissions shut Administrators out, so \
         this process cannot open it: close every handle to it, by ending the processes that have it open"
    );
    assert_eq!(
        unreadable.to_string(),
        "the install lock `Global\\goetia-shim` has permissions that shut Administrators out, so this process cannot \
         open it, and its owner could not be read: close every handle to it, by ending the processes that have it open"
    );
}

/// A denied token without Administrators needs elevation, and its owner is not read.
#[skuld::test]
fn a_denied_token_without_administrators_needs_elevation() {
    let error = failure(
        io::Error::from_raw_os_error(ERROR_ACCESS_DENIED as i32),
        || Ok(false),
        || panic!("the owner is read only for a caller the lock shut out"),
    );

    assert!(matches!(error, Error::ElevationRequired { .. }), "{error:?}");
}

/// The token is asked about only for a denial: no other code depends on it.
#[skuld::test]
fn the_token_is_asked_only_for_a_denial() {
    let error = failure(
        io::Error::from_raw_os_error(ERROR_INVALID_OWNER as i32),
        || panic!("the token is not asked about for a code that does not depend on it"),
        || panic!("no owner is read"),
    );

    assert!(matches!(error, Error::ElevationRequired { .. }), "{error:?}");
}

/// A token that cannot be asked leaves the denial and the failure to ask, in that order: neither
/// is guessed at.
#[skuld::test]
fn a_token_that_cannot_be_asked_reports_both_failures() {
    let error = failure(
        io::Error::from_raw_os_error(ERROR_ACCESS_DENIED as i32),
        || Err(io::Error::other("no token")),
        || panic!("no owner is read"),
    );

    let Error::Both { first, then } = &error else {
        panic!("expected both failures, got {error:?}");
    };
    assert!(matches!(**first, Error::Os { .. }), "{first:?}");
    assert!(then.to_string().contains("no token"), "{then}");
}

/// Every other code is the OS's, whoever the caller is.
#[skuld::test]
fn any_other_code_is_the_os_failure() {
    for administrators in [false, true] {
        for code in [Some(ERROR_NOT_ENOUGH_MEMORY as i32), Some(1), None] {
            assert_eq!(
                failure_kind(code, administrators),
                FailureKind::Os,
                "{code:?}, administrators {administrators}"
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
