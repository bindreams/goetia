//! What goetia asks of the caller's authority on Windows. Two questions, which an unelevated
//! administrator answers the same way and which differ for other tokens:
//! - `is_elevated`, a capability the CLI asks: can this process open SCM with every right.
//! - [`token_has_administrators`], a fact about the token that the library needs, `cli` feature or
//!   not: is Administrators enabled in it.

use std::io;

use windows_sys::Win32::Foundation::HANDLE;
use windows_sys::Win32::Security::CheckTokenMembership;

use crate::backend::scm::shim::acl::administrators;

/// Whether the current process can open SCM with `ALL_ACCESS`.
///
/// Probes the capability rather than the token: opening SCM with
/// `ALL_ACCESS` is exactly what a mutating subcommand needs and exactly
/// what a filtered (non-elevated) admin token refuses. The same technique
/// `tests/support/mod.rs`'s `elevated()` precondition already uses. What a
/// token that has Administrators enabled but is refused SCM (a policy,
/// a different SCM security descriptor) is told is that it is not elevated;
/// whether the token has Administrators is [`token_has_administrators`]. Only the CLI asks.
#[cfg(feature = "cli")]
pub fn is_elevated() -> bool {
    use windows_service::service_manager::{ServiceManager, ServiceManagerAccess};

    ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::ALL_ACCESS).is_ok()
}

/// Whether the calling thread's token (the process's, for a thread that does not impersonate) has
/// Administrators enabled, which is what an object owned by Administrators needs of its creator.
/// A filtered administrator's token has the group disabled.
pub(crate) fn token_has_administrators() -> io::Result<bool> {
    has_administrators_in(std::ptr::null_mut())
}

/// [`token_has_administrators`] for `token`, an impersonation token, or the thread's own for NULL.
fn has_administrators_in(token: HANDLE) -> io::Result<bool> {
    let mut sid = administrators();
    let mut member = 0;
    // SAFETY: `sid` is a valid binary SID that lives for the call; `member` is a valid out-pointer;
    // `token` is NULL or a live impersonation token handle.
    let ok = unsafe { CheckTokenMembership(token, sid.as_mut_ptr().cast(), &mut member) };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(member != 0)
}

#[cfg(test)]
#[path = "elevation_tests.rs"]
mod elevation_tests;
