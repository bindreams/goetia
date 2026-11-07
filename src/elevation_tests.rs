use std::io;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};

use windows_sys::Win32::Security::{
    CreateRestrictedToken, DuplicateTokenEx, SID_AND_ATTRIBUTES, SecurityImpersonation, TOKEN_DUPLICATE,
    TOKEN_IMPERSONATE, TOKEN_QUERY, TokenImpersonation,
};
use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

use super::*;
use crate::backend::scm::shim::acl::administrators;

/// An impersonation copy of this process's token, with Administrators disabled when `disable` says
/// so (a filtered, unelevated administrator's token has it in that state).
fn token_copy(disable: bool) -> OwnedHandle {
    // SAFETY: plain Win32 calls with pointers to locals that outlive them.
    unsafe {
        let mut process = std::ptr::null_mut();
        assert_ne!(
            OpenProcessToken(GetCurrentProcess(), TOKEN_DUPLICATE | TOKEN_QUERY, &mut process),
            0,
            "OpenProcessToken: {}",
            io::Error::last_os_error()
        );
        let process = OwnedHandle::from_raw_handle(process);
        let mut source = process.as_raw_handle();
        let restricted;
        let mut sid = administrators();
        if disable {
            let disabled = SID_AND_ATTRIBUTES {
                Sid: sid.as_mut_ptr().cast(),
                Attributes: 0,
            };
            let mut raw = std::ptr::null_mut();
            assert_ne!(
                CreateRestrictedToken(
                    source,
                    0,
                    1,
                    &disabled,
                    0,
                    std::ptr::null(),
                    0,
                    std::ptr::null(),
                    &mut raw,
                ),
                0,
                "CreateRestrictedToken: {}",
                io::Error::last_os_error()
            );
            restricted = OwnedHandle::from_raw_handle(raw);
            source = restricted.as_raw_handle();
        }
        let mut copy = std::ptr::null_mut();
        assert_ne!(
            DuplicateTokenEx(
                source,
                TOKEN_QUERY | TOKEN_IMPERSONATE,
                std::ptr::null(),
                SecurityImpersonation,
                TokenImpersonation,
                &mut copy,
            ),
            0,
            "DuplicateTokenEx: {}",
            io::Error::last_os_error()
        );
        OwnedHandle::from_raw_handle(copy)
    }
}

/// A token whose Administrators group is disabled has no Administrators, whatever it belongs to:
/// that is what an unelevated administrator's token is, and what an elevated one is not.
#[skuld::test]
fn a_token_with_administrators_disabled_does_not_have_them() {
    let token = token_copy(true);

    assert!(!has_administrators_in(token.as_raw_handle()).unwrap());
}

/// The token of the process is asked the same way whether it is named or not.
#[skuld::test]
fn the_process_token_answers_as_its_copy_does() {
    let copy = token_copy(false);

    assert_eq!(
        token_has_administrators().unwrap(),
        has_administrators_in(copy.as_raw_handle()).unwrap()
    );
}

/// Opening SCM with `ALL_ACCESS` needs Administrators enabled, so a process that can has them.
#[skuld::test]
fn a_process_that_opens_scm_with_all_access_has_administrators_enabled() {
    if is_elevated() {
        assert!(token_has_administrators().unwrap());
    }
}
