//! Whether a directory carries an ACL, which goetia does not evaluate: a log root opens must have none
//! on its path. The directory is read through the descriptor the walk holds, never opened again by name
//! or for reading, so that what is judged is the directory the walk passed, and a root that may search a
//! directory and not read it (an NFS export that squashes root, a FUSE file system) still reads its ACL.

use std::io;
use std::os::fd::{AsRawFd as _, BorrowedFd};

/// The extended attributes a file system keeps an ACL in, by the names the Linux kernel gives them. Each
/// exists only where the file system keeps an ACL that is more than the mode:
/// - `system.posix_acl_access`: a POSIX ACL with an entry beyond the mode's own (`set_posix_acl` in
///   `fs/posix_acl.c` keeps none for one the mode says), `XATTR_NAME_POSIX_ACL_ACCESS` in
///   `include/uapi/linux/xattr.h`;
/// - `system.posix_acl_default`: the default ACL of a directory, which a directory made in it inherits;
///   `XATTR_NAME_POSIX_ACL_DEFAULT` in the same header. It is an ACL whatever the access ACL is;
/// - `system.nfs4_acl`: the NFS client's, `XATTR_NAME_NFSV4_ACL` in `fs/nfs/nfs4proc.c`;
/// - `system.nfs4_acl_xdr`: the NFSv4 ACL of ZFS, `NFS41ACL_XATTR` in `module/os/linux/zfs/zpl_xattr.c` of
///   TrueNAS's OpenZFS fork (`truenas/zfs`); upstream OpenZFS does not have it;
/// - `system.cifs_acl`: the CIFS client's, `CIFS_XATTR_CIFS_ACL` in `fs/smb/client/xattr.c`; the
///   client's `system.smb3_acl` is the same handler under another name.
///
/// A directory with any of them is refused whatever the ACL grants.
#[cfg(target_os = "linux")]
const ACL_ATTRIBUTES: &[&std::ffi::CStr] = &[
    c"system.posix_acl_access",
    c"system.posix_acl_default",
    c"system.nfs4_acl",
    c"system.nfs4_acl_xdr",
    c"system.cifs_acl",
];

/// Whether `dir` carries an ACL: any of [`ACL_ATTRIBUTES`] is on it. No ACL, and a file system that has
/// none, are `false`; any other error is the file system's to say.
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
    /// `entry_p` is an `acl_entry_t`, a pointer; `-1` with `EINVAL` says there is no such entry.
    fn acl_get_entry(acl: *mut libc::c_void, entry_id: libc::c_int, entry_p: *mut *mut libc::c_void) -> libc::c_int;
    fn acl_free(obj: *mut libc::c_void) -> libc::c_int;
}

/// `ACL_FIRST_ENTRY` of `<sys/acl.h>`.
#[cfg(target_os = "macos")]
const ACL_FIRST_ENTRY: libc::c_int = 0;

/// Whether `dir` carries an extended ACL with an entry, read through the descriptor. An ACL with no
/// entries is none, as is a file system that has no ACLs; any other error is the file system's to say.
#[cfg(target_os = "macos")]
pub(super) fn has_acl(dir: BorrowedFd<'_>) -> io::Result<bool> {
    // SAFETY: `dir` is open for the call.
    let acl = unsafe { acl_get_fd_np(dir.as_raw_fd(), ACL_TYPE_EXTENDED) };
    if acl.is_null() {
        return no_acl_or_error(io::Error::last_os_error());
    }
    let mut entry = std::ptr::null_mut();
    // SAFETY: `acl` came from `acl_get_fd_np` and is live; `entry` is written only on success.
    let found = unsafe { acl_get_entry(acl, ACL_FIRST_ENTRY, &mut entry) };
    let error = io::Error::last_os_error();
    // SAFETY: `acl` came from `acl_get_fd_np` and is freed once.
    unsafe { acl_free(acl) };
    if found == 0 {
        return Ok(true);
    }
    // Libc says there is no first entry as `EINVAL`.
    match error.raw_os_error() {
        Some(libc::EINVAL) => Ok(false),
        _ => Err(error),
    }
}

/// What `acl_get_fd_np`'s error says: `ENOENT`, no ACL on the file, and `ENOTSUP` or `EOPNOTSUPP` (distinct on
/// macOS), a file system that has none, are `false`; any other is the error.
#[cfg(target_os = "macos")]
fn no_acl_or_error(e: io::Error) -> io::Result<bool> {
    match e.raw_os_error() {
        Some(libc::ENOENT | libc::ENOTSUP | libc::EOPNOTSUPP) => Ok(false),
        _ => Err(e),
    }
}

#[cfg(test)]
#[path = "acl_tests.rs"]
mod acl_tests;
