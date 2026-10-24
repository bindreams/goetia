//! `SeDebugPrivilege` for the `OpenProcess` in `kill`. An elevated administrator holds it but has
//! it disabled, and without it the process of a `LocalService` service is refused
//! `PROCESS_TERMINATE`. `taskkill` enables it the same way.
//!
//! It is enabled on a copy of the caller's token, installed as the calling thread's impersonation
//! token for the open alone, so the rest of the process never sees the change. A token that does not
//! hold the privilege is left as it is: the open then fails or succeeds on its own terms.

use std::io;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle, RawHandle};

use windows_sys::Win32::Foundation::{ERROR_ACCESS_DENIED, ERROR_NO_TOKEN, ERROR_NOT_ALL_ASSIGNED, GetLastError, LUID};
use windows_sys::Win32::Security::{
    AdjustTokenPrivileges, DuplicateTokenEx, LUID_AND_ATTRIBUTES, LookupPrivilegeValueW, SE_DEBUG_NAME,
    SE_PRIVILEGE_ENABLED, SecurityImpersonation, TOKEN_ADJUST_PRIVILEGES, TOKEN_DUPLICATE, TOKEN_IMPERSONATE,
    TOKEN_PRIVILEGES, TOKEN_QUERY, TokenImpersonation,
};
use windows_sys::Win32::System::Threading::{
    GetCurrentProcess, GetCurrentThread, OpenProcessToken, OpenThreadToken, SetThreadToken,
};

/// While alive, the calling thread runs with the caller's `SeDebugPrivilege` enabled, if the caller
/// holds it. Dropping restores the thread's earlier token.
pub(super) struct DebugPrivilege {
    /// `Some` once the thread token was replaced; the token to put back (`None` inside: none).
    restore: Option<Option<OwnedHandle>>,
    /// Why enabling failed. Absent when it worked and when the token lacks the privilege.
    failure: Option<io::Error>,
}

impl DebugPrivilege {
    /// Enable the privilege for this thread if the caller holds it. A failure leaves the thread
    /// untouched and is kept for [`Self::explain_denial`]; the open that follows reports its own error.
    pub(super) fn enable() -> Self {
        match install() {
            Ok(restore) => Self { restore, failure: None },
            Err(e) => Self {
                restore: None,
                failure: Some(e),
            },
        }
    }

    /// `open`'s error, with the failed privilege step named when the open was denied: the denial may
    /// be the privilege that could not be enabled.
    pub(super) fn explain_denial(&self, open: io::Error) -> io::Error {
        match &self.failure {
            Some(step) if open.raw_os_error() == Some(ERROR_ACCESS_DENIED as i32) => io::Error::new(
                open.kind(),
                format!("{open} (enabling SeDebugPrivilege for the open also failed: {step})"),
            ),
            _ => open,
        }
    }
}

impl Drop for DebugPrivilege {
    fn drop(&mut self) {
        if let Some(previous) = self.restore.take() {
            let handle = previous.as_ref().map_or(std::ptr::null_mut(), |h| h.as_raw_handle());
            // SAFETY: a null thread means this one; `handle` is a token this function opened or null.
            let ok = unsafe { SetThreadToken(std::ptr::null(), handle) };
            if ok == 0 {
                // The thread keeps the privileged token, so this is said in release builds too.
                let e = io::Error::last_os_error();
                eprintln!("{}", restore_notice(&e));
                debug_assert!(false, "SetThreadToken: {e}");
            }
        }
    }
}

/// The warning for a thread token that could not be put back.
fn restore_notice(e: &io::Error) -> String {
    format!("warning: SetThreadToken failed to restore the thread's token after enabling SeDebugPrivilege: {e}")
}

/// Impersonate a privileged copy of the thread's effective token. `Ok(Some(previous))` when
/// installed, `Ok(None)` when the token does not hold the privilege.
fn install() -> io::Result<Option<Option<OwnedHandle>>> {
    // The effective token is the thread's, when it impersonates, else the process's.
    let mut token = std::ptr::null_mut();
    // SAFETY: pointers to locals; a returned token is owned below.
    let previous = if unsafe {
        OpenThreadToken(
            GetCurrentThread(),
            TOKEN_DUPLICATE | TOKEN_QUERY | TOKEN_IMPERSONATE,
            1,
            &mut token,
        )
    } != 0
    {
        // SAFETY: a valid handle `OpenThreadToken` just returned.
        Some(unsafe { OwnedHandle::from_raw_handle(token) })
    } else {
        let e = io::Error::last_os_error();
        if e.raw_os_error() != Some(ERROR_NO_TOKEN as i32) {
            return Err(e);
        }
        None
    };
    let source = match &previous {
        Some(token) => token.as_raw_handle(),
        None => {
            // SAFETY: as above.
            if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_DUPLICATE | TOKEN_QUERY, &mut token) } == 0 {
                return Err(io::Error::last_os_error());
            }
            token
        }
    };
    // Owns the process token opened above, so it closes whichever way this returns.
    let _process = previous
        .is_none()
        // SAFETY: a valid handle `OpenProcessToken` just returned and nothing else owns.
        .then(|| unsafe { OwnedHandle::from_raw_handle(token) });

    let mut copy = std::ptr::null_mut();
    // SAFETY: `source` is a live token with `TOKEN_DUPLICATE`; the rest are locals.
    let ok = unsafe {
        DuplicateTokenEx(
            source,
            TOKEN_ADJUST_PRIVILEGES | TOKEN_IMPERSONATE | TOKEN_QUERY,
            std::ptr::null(),
            SecurityImpersonation,
            TokenImpersonation,
            &mut copy,
        )
    };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: a valid handle `DuplicateTokenEx` just returned.
    let copy = unsafe { OwnedHandle::from_raw_handle(copy) };

    if !enable_in(copy.as_raw_handle())? {
        return Ok(None);
    }
    // SAFETY: a null thread means this one; `copy` has `TOKEN_IMPERSONATE`.
    if unsafe { SetThreadToken(std::ptr::null(), copy.as_raw_handle()) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(Some(previous))
}

/// Enable `SeDebugPrivilege` in `token`, which needs `TOKEN_ADJUST_PRIVILEGES`. `Ok(false)` when
/// the token does not hold it.
fn enable_in(token: RawHandle) -> io::Result<bool> {
    let mut luid = LUID {
        LowPart: 0,
        HighPart: 0,
    };
    // SAFETY: a pointer to a local, and the constant name of a privilege.
    if unsafe { LookupPrivilegeValueW(std::ptr::null(), SE_DEBUG_NAME, &mut luid) } == 0 {
        return Err(io::Error::last_os_error());
    }
    let privileges = TOKEN_PRIVILEGES {
        PrivilegeCount: 1,
        Privileges: [LUID_AND_ATTRIBUTES {
            Luid: luid,
            Attributes: SE_PRIVILEGE_ENABLED,
        }],
    };
    // SAFETY: `token` is live; `privileges` outlives the call and the optional outputs are null.
    let ok = unsafe { AdjustTokenPrivileges(token, 0, &privileges, 0, std::ptr::null_mut(), std::ptr::null_mut()) };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    // The call succeeds for a token that lacks the privilege and says so only here.
    // SAFETY: no arguments.
    Ok(unsafe { GetLastError() } != ERROR_NOT_ALL_ASSIGNED)
}

#[cfg(test)]
#[path = "privilege_tests.rs"]
mod privilege_tests;
