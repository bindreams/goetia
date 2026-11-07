use std::io;

use windows_sys::Win32::Foundation::{ERROR_ACCESS_DENIED, ERROR_INVALID_OWNER};

use super::*;

/// The lock's owner is Administrators, which a token without it enabled (an unelevated one) cannot
/// give an object: `CreateMutexW` fails with `ERROR_INVALID_OWNER`, which is a need for elevation.
#[skuld::test]
fn an_owner_the_token_cannot_give_is_a_need_for_elevation() {
    let error = create_failed(io::Error::from_raw_os_error(ERROR_INVALID_OWNER as i32));

    let Error::ElevationRequired { subcommand } = &error else {
        panic!("expected a need for elevation, got {error:?}");
    };
    assert!(subcommand.contains(NAME), "{subcommand}");
    assert!(!error.is_indeterminate());
}

/// Any other failure is the OS's, as it was.
#[skuld::test]
fn any_other_failure_to_create_the_lock_is_the_os_error() {
    for code in [ERROR_ACCESS_DENIED, 1] {
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
