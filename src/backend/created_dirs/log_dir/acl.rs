//! Whether a directory has an access control list, which makes its group bits not say who can write it.
//!
//! The group bits of a directory with an ACL are the ACL's mask: a group-write bit says an account the ACL
//! names can write the directory, not that its group can. So the group of such a directory is not
//! trusted. The directory is read through the descriptor the walk holds, never opened again by name or
//! for reading, so that what is judged is the directory the walk passed, and a root that may search a
//! directory and not read it (an NFS export that squashes root, a FUSE file system) still reads its ACL.

use std::io;
use std::os::fd::{AsRawFd as _, BorrowedFd};

/// The extended attributes a file system keeps an ACL in, by the names the Linux kernel gives them. The
/// first exists only where a POSIX ACL has an entry beyond the mode's own; the others (NFSv4, CIFS) exist
/// wherever the file system keeps one, and a directory with one is judged as having an ACL: its group
/// bits are not the group's.
#[cfg(target_os = "linux")]
const ACL_ATTRIBUTES: &[&std::ffi::CStr] = &[c"system.posix_acl_access", c"system.nfs4_acl", c"system.cifs_acl"];

/// Whether `dir` has an ACL: any of [`ACL_ATTRIBUTES`] is on it. No ACL, and a file system that has none,
/// are `false`; any other error is the file system's to say.
///
/// `fgetxattr` refuses an `O_PATH` descriptor, which the walk holds; the descriptor is read as
/// `/proc/self/fd/<n>`, which names the same directory and needs no read permission on it.
#[cfg(target_os = "linux")]
pub(super) fn has_acl(dir: BorrowedFd<'_>) -> io::Result<bool> {
    let path = std::ffi::CString::new(format!("/proc/self/fd/{}", dir.as_raw_fd())).expect("digits hold no NUL");
    has_any(ACL_ATTRIBUTES, |name| attribute_exists(&path, name))
}

/// Whether `exists` says any of `names` is there; the first error it returns is this one's.
#[cfg(target_os = "linux")]
fn has_any(names: &[&std::ffi::CStr], mut exists: impl FnMut(&std::ffi::CStr) -> io::Result<bool>) -> io::Result<bool> {
    for name in names {
        if exists(name)? {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Whether the extended attribute `name` is on what `path` names.
#[cfg(target_os = "linux")]
fn attribute_exists(path: &std::ffi::CStr, name: &std::ffi::CStr) -> io::Result<bool> {
    // SAFETY: both names are NUL-terminated for the call; a null buffer of size 0 asks only whether the
    // attribute is there.
    let size = unsafe { libc::getxattr(path.as_ptr(), name.as_ptr(), std::ptr::null_mut(), 0) };
    if size >= 0 {
        return Ok(true);
    }
    let e = io::Error::last_os_error();
    match e.raw_os_error() {
        Some(libc::ENODATA | libc::EOPNOTSUPP) => Ok(false),
        // The descriptor is open, so only a `/proc` that is not mounted leaves its name missing.
        Some(libc::ENOENT) => Err(io::Error::other(
            "/proc is not mounted, and the descriptor is read through /proc/self/fd",
        )),
        _ => Err(e),
    }
}

/// `ACL_TYPE_EXTENDED` of `<sys/acl.h>`: the ACL macOS keeps, as `chmod +a` writes it.
#[cfg(target_os = "macos")]
const ACL_TYPE_EXTENDED: libc::c_int = 0x100;

#[cfg(target_os = "macos")]
unsafe extern "C" {
    /// `acl_t` is a pointer; `NULL` with `ENOENT` says there is no ACL.
    fn acl_get_fd_np(fd: libc::c_int, acl_type: libc::c_int) -> *mut libc::c_void;
    fn acl_free(obj: *mut libc::c_void) -> libc::c_int;
}

/// Whether `dir` has an extended ACL, read through the descriptor. A directory with none is `false`; any
/// other error is the file system's to say.
#[cfg(target_os = "macos")]
pub(super) fn has_acl(dir: BorrowedFd<'_>) -> io::Result<bool> {
    // SAFETY: `dir` is open for the call.
    let acl = unsafe { acl_get_fd_np(dir.as_raw_fd(), ACL_TYPE_EXTENDED) };
    if acl.is_null() {
        let e = io::Error::last_os_error();
        return match e.raw_os_error() {
            Some(libc::ENOENT) => Ok(false),
            _ => Err(e),
        };
    }
    // SAFETY: `acl` came from `acl_get_fd_np` and is freed once.
    unsafe { acl_free(acl) };
    Ok(true)
}

#[cfg(test)]
#[path = "acl_tests.rs"]
mod acl_tests;
