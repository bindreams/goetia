//! What the library asks of the caller's token on Windows: [`token_has_administrators`]. The CLI's
//! "is the process elevated" is `cosca::elevation::is_elevated`, which reads `TokenIsElevated` of
//! the process token; it does not report whether Administrators is enabled, and does not follow a
//! thread that impersonates.

use std::io;

use windows_sys::Win32::Security::CheckTokenMembership;

use crate::backend::scm::shim::acl::administrators;

/// Whether the calling thread's token (the process's, for a thread that does not impersonate) has
/// Administrators enabled, which is what an object owned by Administrators needs of its creator.
/// A filtered administrator's token has the group disabled.
pub(crate) fn token_has_administrators() -> io::Result<bool> {
    let mut sid = administrators();
    let mut member = 0;
    // SAFETY: `sid` is a valid binary SID that lives for the call; `member` is a valid out-pointer;
    // a NULL token is the calling thread's.
    let ok = unsafe { CheckTokenMembership(std::ptr::null_mut(), sid.as_mut_ptr().cast(), &mut member) };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(member != 0)
}

#[cfg(test)]
pub(crate) mod test_support;

#[cfg(test)]
#[path = "elevation_tests.rs"]
mod elevation_tests;
