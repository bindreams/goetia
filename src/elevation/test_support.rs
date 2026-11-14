//! Tokens for tests that act as an unelevated user, and a guard that ends the impersonation. Used by
//! this module's tests and by `backend::scm::kill::privilege`'s.
//!
//! Under the `testing` feature alone only the `SeDebugPrivilege` probe uses it.
#![cfg_attr(not(test), allow(dead_code))]

use std::io;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};

use windows_sys::Win32::Foundation::ERROR_NO_TOKEN;
use windows_sys::Win32::Security::{
    CreateRestrictedToken, DISABLE_MAX_PRIVILEGE, DuplicateTokenEx, SID_AND_ATTRIBUTES, SecurityImpersonation,
    TOKEN_ADJUST_PRIVILEGES, TOKEN_DUPLICATE, TOKEN_IMPERSONATE, TOKEN_QUERY, TokenImpersonation,
};
use windows_sys::Win32::System::Threading::{
    GetCurrentProcess, GetCurrentThread, OpenProcessToken, OpenThreadToken, SetThreadToken,
};

use crate::backend::scm::shim::acl::Descriptor;
use crate::backend::scm::sid::Sid;
use crate::error::within;

/// This process's token, opened with `access`.
pub(crate) fn open_process_token(access: u32) -> io::Result<OwnedHandle> {
    let mut process = std::ptr::null_mut();
    // SAFETY: a pointer to a local; a returned token is owned below.
    if unsafe { OpenProcessToken(GetCurrentProcess(), access, &mut process) } == 0 {
        return Err(within("OpenProcessToken", io::Error::last_os_error()));
    }
    // SAFETY: a valid handle `OpenProcessToken` just returned and nothing else owns.
    Ok(unsafe { OwnedHandle::from_raw_handle(process) })
}

/// This process's token, opened to be copied.
fn process_token() -> io::Result<OwnedHandle> {
    open_process_token(TOKEN_DUPLICATE | TOKEN_QUERY)
}

/// The permissions of a token copy: every right to Everyone, so to the user the thread impersonates
/// whatever its groups. A copy made with the source's default permissions grants its own user and
/// Administrators, and an elevated administrator's default grants Administrators alone; a thread
/// that impersonates a copy with Administrators denied is then refused the copy it is asked to open
/// (`CheckTokenMembership` with no token opens the thread's). A copy is a handle in this process,
/// named nowhere, so it is no wider an opening than the handle itself.
const TOKEN_COPY: &str = "D:(A;;GA;;;WD)";

/// An impersonation copy of `source`, opened with `access`, that the thread impersonating it can open
/// on any host (see [`TOKEN_COPY`]).
fn impersonation_copy(source: &OwnedHandle, access: u32) -> io::Result<OwnedHandle> {
    let permissions = Descriptor::from_sddl(TOKEN_COPY)
        .map_err(|e| within("the permissions of a token copy", io::Error::other(e)))?;
    let attributes = permissions.attributes();
    let mut copy = std::ptr::null_mut();
    // SAFETY: `source` is a live token handle with `TOKEN_DUPLICATE`; `attributes` points at
    // `permissions`, which is live for the call; `copy` is a local.
    let ok = unsafe {
        DuplicateTokenEx(
            source.as_raw_handle(),
            access,
            &attributes,
            SecurityImpersonation,
            TokenImpersonation,
            &mut copy,
        )
    };
    if ok == 0 {
        return Err(within("DuplicateTokenEx", io::Error::last_os_error()));
    }
    // SAFETY: a valid handle `DuplicateTokenEx` just returned and nothing else owns.
    Ok(unsafe { OwnedHandle::from_raw_handle(copy) })
}

/// An impersonation copy of this process's token, which holds whatever privileges the process does.
pub(crate) fn process_token_copy(access: u32) -> io::Result<OwnedHandle> {
    impersonation_copy(&process_token()?, access)
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
    let process = process_token().expect("the process token");
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
    impersonation_copy(&restricted, access).expect("a copy of the restricted token")
}

/// A copy of this process's token with every privilege removed, as an unelevated user has.
pub(crate) fn token_without_privileges() -> OwnedHandle {
    restricted_token_copy(DISABLE_MAX_PRIVILEGE, &mut [], TOKEN_ADJUST_PRIVILEGES | TOKEN_QUERY)
}

/// The calling thread's own token, opened with `access`, if it has one (a test thread starts
/// without).
pub(crate) fn open_thread_token(access: u32) -> Option<OwnedHandle> {
    let mut token = std::ptr::null_mut();
    // SAFETY: a pointer to a local; a returned token is owned below.
    let ok = unsafe { OpenThreadToken(GetCurrentThread(), access, 1, &mut token) };
    if ok != 0 {
        // SAFETY: a valid handle `OpenThreadToken` just returned and nothing else owns.
        return Some(unsafe { OwnedHandle::from_raw_handle(token) });
    }
    let e = io::Error::last_os_error();
    assert_eq!(e.raw_os_error(), Some(ERROR_NO_TOKEN as i32), "OpenThreadToken: {e}");
    None
}

/// Puts back the token the calling thread impersonated before [`Self::impersonate`], or none, when
/// dropped, so a failing assert leaves the test thread as it found it.
pub(crate) struct ThreadTokenReset {
    /// The thread's token before, which must outlive the impersonation.
    previous: Option<OwnedHandle>,
}

impl ThreadTokenReset {
    /// The calling thread impersonates `token`, which needs `TOKEN_IMPERSONATE`, until the guard
    /// drops.
    pub(crate) fn impersonate(token: &OwnedHandle) -> Self {
        let previous = open_thread_token(TOKEN_IMPERSONATE);
        // SAFETY: a null thread means this one; `token` is a live handle.
        assert_ne!(
            unsafe { SetThreadToken(std::ptr::null(), token.as_raw_handle()) },
            0,
            "SetThreadToken: {}",
            io::Error::last_os_error()
        );
        Self { previous }
    }
}

impl Drop for ThreadTokenReset {
    fn drop(&mut self) {
        let previous = self
            .previous
            .as_ref()
            .map_or(std::ptr::null_mut(), |p| p.as_raw_handle());
        // SAFETY: a null thread means this one; `previous` is a live token with `TOKEN_IMPERSONATE`,
        // or null, which ends the impersonation.
        unsafe { SetThreadToken(std::ptr::null(), previous) };
    }
}
