use std::io;
use std::thread;

use windows_sys::Win32::Foundation::{ERROR_ACCESS_DENIED, ERROR_INVALID_OWNER, ERROR_NOT_ENOUGH_MEMORY};
use windows_sys::Win32::Storage::FileSystem::SYNCHRONIZE;
use windows_sys::Win32::System::Threading::MUTEX_MODIFY_STATE;

use super::*;
use crate::backend::test_elevation::{ELEVATED, elevated};

/// Carried by every test that takes the machine-wide lock `Global\goetia-shim`, which wait for each
/// other: one's probe of who holds it must not see another's hold. Shared with
/// `tests/shim_integration` and `tests/scm_integration` by label *name*, which skuld serialises on
/// across processes, as the lock is across them.
#[skuld::label]
const UNIT_DIR_EXCLUSIVE: skuld::Label;

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
/// is denied, for a cause the code does not say.
#[skuld::test]
fn access_denied_is_a_need_for_elevation_only_for_a_token_without_administrators() {
    let denied = Some(ERROR_ACCESS_DENIED as i32);

    assert_eq!(failure_kind(denied, false), FailureKind::NeedsElevation);
    assert_eq!(failure_kind(denied, true), FailureKind::Denied);
}

/// A denial for a token with Administrators enabled is told as that, with the owner when it was
/// read: the lock's permissions are one cause, not the established one.
#[skuld::test]
fn a_denied_administrator_is_told_only_what_is_known() {
    let denied = || io::Error::from_raw_os_error(ERROR_ACCESS_DENIED as i32);

    let readable = failure(denied(), || Ok(true), || Some(r"EVE\eve".to_string()));
    let unreadable = failure(denied(), || Ok(true), || None);

    assert_eq!(
        readable.to_string(),
        "access to the install lock `Global\\goetia-shim`, which is owned by EVE\\eve, was denied to a token with \
         Administrators enabled: either the lock's permissions or the token's integrity level or privileges refuse \
         it. If the lock is someone else's, close every handle to it, by ending the processes that have it open"
    );
    assert_eq!(
        unreadable.to_string(),
        "access to the install lock `Global\\goetia-shim` was denied to a token with Administrators enabled, and its \
         owner could not be read: either the lock's permissions or the token's integrity level or privileges refuse \
         it. If the lock is someone else's, close every handle to it, by ending the processes that have it open"
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
    assert_eq!(
        then.report().to_string(),
        "failed to check whether the token has Administrators enabled: no token"
    );
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

/// The real lock, opened to wait on it, and so kept in existence for as long as the handle lives: a
/// named mutex goes with its last handle, and a probe that opens it by name after that fails.
fn open_lock() -> OwnedHandle {
    let name = wide_null(NAME);
    // SAFETY: `name` is NUL-terminated.
    let raw = unsafe { OpenMutexW(SYNCHRONIZE | MUTEX_MODIFY_STATE, 0, name.as_ptr()) };
    assert!(!raw.is_null(), "open the lock: {}", io::Error::last_os_error());
    // SAFETY: a valid handle `OpenMutexW` just returned and nothing else owns.
    unsafe { OwnedHandle::from_raw_handle(raw) }
}

/// A holder that exits without releasing leaves the mutex abandoned, which the next to take it
/// owns all the same.
#[skuld::test(requires = [elevated], labels = [ELEVATED, UNIT_DIR_EXCLUSIVE], serial = UNIT_DIR_EXCLUSIVE)]
fn a_lock_whose_holder_exited_without_releasing_it_is_taken() {
    // Keeps the mutex in existence once the lock that made it is dropped, and until the test ends.
    let _alive = {
        let _created = InstallLock::acquire().unwrap();
        open_lock()
    };
    thread::spawn(|| {
        let held = open_lock();
        // SAFETY: `held` is a live handle with `SYNCHRONIZE`.
        assert_eq!(
            unsafe { WaitForSingleObject(held.as_raw_handle(), INFINITE) },
            WAIT_OBJECT_0
        );
        // Never released: the thread's exit abandons the mutex, which `_alive` keeps in existence.
    })
    .join()
    .unwrap();

    let taken = InstallLock::acquire();

    assert!(taken.is_ok(), "{:?}", taken.err());
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
