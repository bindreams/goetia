//! Tokens for tests that act as an unelevated user, and a guard that ends the impersonation. Used by
//! this module's tests and by `backend::scm::kill::privilege`'s.

use std::io;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};

use windows_sys::Win32::Security::{
    CreateRestrictedToken, DISABLE_MAX_PRIVILEGE, DuplicateTokenEx, SID_AND_ATTRIBUTES, SecurityImpersonation,
    TOKEN_ADJUST_PRIVILEGES, TOKEN_DUPLICATE, TOKEN_QUERY, TokenImpersonation,
};
use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken, SetThreadToken};

use crate::backend::scm::shim::acl::Sid;

/// This process's token, opened to be copied.
fn process_token() -> OwnedHandle {
    let mut process = std::ptr::null_mut();
    // SAFETY: a pointer to a local; a returned token is owned below.
    assert_ne!(
        unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_DUPLICATE | TOKEN_QUERY, &mut process) },
        0,
        "OpenProcessToken: {}",
        io::Error::last_os_error()
    );
    // SAFETY: a valid handle `OpenProcessToken` just returned and nothing else owns.
    unsafe { OwnedHandle::from_raw_handle(process) }
}

/// An impersonation copy of `source`, opened with `access`.
fn impersonation_copy(source: &OwnedHandle, access: u32) -> OwnedHandle {
    let mut copy = std::ptr::null_mut();
    // SAFETY: `source` is a live token handle with `TOKEN_DUPLICATE`; `copy` is a local.
    assert_ne!(
        unsafe {
            DuplicateTokenEx(
                source.as_raw_handle(),
                access,
                std::ptr::null(),
                SecurityImpersonation,
                TokenImpersonation,
                &mut copy,
            )
        },
        0,
        "DuplicateTokenEx: {}",
        io::Error::last_os_error()
    );
    // SAFETY: a valid handle `DuplicateTokenEx` just returned and nothing else owns.
    unsafe { OwnedHandle::from_raw_handle(copy) }
}

/// An impersonation copy of this process's token, which holds whatever privileges the process does.
pub(crate) fn process_token_copy(access: u32) -> OwnedHandle {
    impersonation_copy(&process_token(), access)
}

/// An impersonation copy of this process's token, opened with `access`, restricted by `flags` (as
/// `CreateRestrictedToken`'s) and with each of `disable` disabled: an unelevated administrator's
/// token is this process's with Administrators disabled.
pub(crate) fn restricted_token_copy(flags: u32, disable: &mut [Sid], access: u32) -> OwnedHandle {
    let disabled: Vec<SID_AND_ATTRIBUTES> = disable
        .iter_mut()
        .map(|sid| SID_AND_ATTRIBUTES {
            Sid: sid.as_mut_ptr().cast(),
            Attributes: 0,
        })
        .collect();
    let process = process_token();
    let mut restricted = std::ptr::null_mut();
    // SAFETY: `process` is a live token handle with `TOKEN_DUPLICATE`; `disabled` points at SIDs
    // that live for the call, and is passed as NULL when empty; `restricted` is a local.
    assert_ne!(
        unsafe {
            CreateRestrictedToken(
                process.as_raw_handle(),
                flags,
                disabled.len() as u32,
                if disabled.is_empty() {
                    std::ptr::null()
                } else {
                    disabled.as_ptr()
                },
                0,
                std::ptr::null(),
                0,
                std::ptr::null(),
                &mut restricted,
            )
        },
        0,
        "CreateRestrictedToken: {}",
        io::Error::last_os_error()
    );
    // SAFETY: a valid handle `CreateRestrictedToken` just returned and nothing else owns.
    let restricted = unsafe { OwnedHandle::from_raw_handle(restricted) };
    // The restricted token's own handle does not carry the rights `access` asks for.
    impersonation_copy(&restricted, access)
}

/// A copy of this process's token with every privilege removed, as an unelevated user has.
pub(crate) fn token_without_privileges() -> OwnedHandle {
    restricted_token_copy(DISABLE_MAX_PRIVILEGE, &mut [], TOKEN_ADJUST_PRIVILEGES | TOKEN_QUERY)
}

/// Removes any impersonation from the calling thread when dropped, so a failing assert leaves the
/// test thread as it found it (a test thread starts without a token).
pub(crate) struct ThreadTokenReset;

impl ThreadTokenReset {
    /// The calling thread impersonates `token`, which needs `TOKEN_IMPERSONATE`, until the guard
    /// drops.
    pub(crate) fn impersonate(token: &OwnedHandle) -> Self {
        // SAFETY: a null thread means this one; `token` is a live handle.
        assert_ne!(
            unsafe { SetThreadToken(std::ptr::null(), token.as_raw_handle()) },
            0,
            "SetThreadToken: {}",
            io::Error::last_os_error()
        );
        Self
    }
}

impl Drop for ThreadTokenReset {
    fn drop(&mut self) {
        // SAFETY: a null thread means this one; a null token ends the impersonation.
        unsafe { SetThreadToken(std::ptr::null(), std::ptr::null_mut()) };
    }
}
