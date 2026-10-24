//! Without elevation: a token that does not hold `SeDebugPrivilege` must come out as "not enabled",
//! never as an error, so the open that follows speaks for itself. The enabled path needs a token
//! that holds the privilege and is pinned by the elevated `kill` tests in `tests/scm_integration`.

use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};

use windows_sys::Win32::Foundation::ERROR_NO_TOKEN;
use windows_sys::Win32::Security::{
    CreateRestrictedToken, DISABLE_MAX_PRIVILEGE, DuplicateTokenEx, SecurityImpersonation, TOKEN_ADJUST_PRIVILEGES,
    TOKEN_DUPLICATE, TOKEN_QUERY, TokenImpersonation,
};
use windows_sys::Win32::System::Threading::{GetCurrentProcess, GetCurrentThread, OpenProcessToken, OpenThreadToken};

use super::{DebugPrivilege, enable_in};

/// A copy of this process's token with every privilege removed, as an unelevated user has.
fn token_without_privileges() -> OwnedHandle {
    // SAFETY: plain Win32 calls with pointers to locals that outlive them.
    unsafe {
        let mut process = std::ptr::null_mut();
        assert_ne!(
            OpenProcessToken(GetCurrentProcess(), TOKEN_DUPLICATE | TOKEN_QUERY, &mut process),
            0,
            "OpenProcessToken: {}",
            std::io::Error::last_os_error()
        );
        let process = OwnedHandle::from_raw_handle(process);
        let mut restricted = std::ptr::null_mut();
        assert_ne!(
            CreateRestrictedToken(
                process.as_raw_handle(),
                DISABLE_MAX_PRIVILEGE,
                0,
                std::ptr::null(),
                0,
                std::ptr::null(),
                0,
                std::ptr::null(),
                &mut restricted,
            ),
            0,
            "CreateRestrictedToken: {}",
            std::io::Error::last_os_error()
        );
        let restricted = OwnedHandle::from_raw_handle(restricted);
        // The restricted token's own handle does not carry the right to adjust it.
        let mut adjustable = std::ptr::null_mut();
        assert_ne!(
            DuplicateTokenEx(
                restricted.as_raw_handle(),
                TOKEN_ADJUST_PRIVILEGES | TOKEN_QUERY,
                std::ptr::null(),
                SecurityImpersonation,
                TokenImpersonation,
                &mut adjustable,
            ),
            0,
            "DuplicateTokenEx: {}",
            std::io::Error::last_os_error()
        );
        OwnedHandle::from_raw_handle(adjustable)
    }
}

fn thread_has_token() -> bool {
    let mut token = std::ptr::null_mut();
    // SAFETY: a pointer to a local; a returned token is owned and closed below.
    let ok = unsafe { OpenThreadToken(GetCurrentThread(), TOKEN_QUERY, 1, &mut token) };
    if ok != 0 {
        // SAFETY: a valid handle `OpenThreadToken` just returned.
        drop(unsafe { OwnedHandle::from_raw_handle(token) });
        return true;
    }
    let e = std::io::Error::last_os_error();
    assert_eq!(e.raw_os_error(), Some(ERROR_NO_TOKEN as i32), "OpenThreadToken: {e}");
    false
}

#[skuld::test]
fn a_token_without_the_privilege_is_not_enabled_and_not_an_error() {
    let token = token_without_privileges();

    let enabled = enable_in(token.as_raw_handle());

    assert!(!enabled.expect("a missing privilege is not an error"));
}

#[skuld::test]
fn enabling_and_letting_go_leaves_the_thread_as_it_was() {
    assert!(
        !thread_has_token(),
        "a test thread starts without an impersonation token"
    );

    drop(DebugPrivilege::enable());

    assert!(!thread_has_token());
}
