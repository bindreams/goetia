//! FFI and small syscall helpers the `libc` crate lacks or makes awkward.

use std::ffi::{CStr, CString};
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

extern "C" {
    pub fn pthread_setugid_np(uid: libc::uid_t, gid: libc::gid_t) -> libc::c_int;
    /// Per-thread working directory; `pthread_fchdir_np(-1)` returns to the process-wide one.
    pub fn pthread_chdir_np(path: *const libc::c_char) -> libc::c_int;
    pub fn pthread_fchdir_np(fd: libc::c_int) -> libc::c_int;
    /// Undocumented with `flags == 0` (`sandbox.h` wants `SANDBOX_NAMED`): pass 2, M4 measures it.
    pub fn sandbox_init(profile: *const libc::c_char, flags: u64, errorbuf: *mut *mut libc::c_char) -> libc::c_int;
    pub fn sandbox_free_error(errorbuf: *mut libc::c_char);
}

/// `sys/kauth.h`: `(~(uid_t)0 - 100)`.
pub const KAUTH_UID_NONE: libc::uid_t = !0 - 100;
pub const KAUTH_GID_NONE: libc::gid_t = !0 - 100;

/// `sys/syscall.h`.
pub const SYS_INITGROUPS: libc::c_int = 243;

/// Extended `access()` bits, `sys/unistd.h`; `access1` shifts them >> 8 into KAUTH actions.
pub const R: i32 = 1 << 9;
pub const W: i32 = 1 << 10;
pub const X: i32 = 1 << 11;
pub const A: i32 = 1 << 13;

pub fn cpath(p: &Path) -> CString {
    CString::new(p.as_os_str().as_bytes()).expect("path without NUL")
}

pub fn cstr(s: &str) -> CString {
    CString::new(s).expect("string without NUL")
}

pub fn errno() -> i32 {
    io::Error::last_os_error().raw_os_error().unwrap_or(0)
}

/// `access(path, bits)` on the calling thread's credential: 0 or errno.
pub fn access(path: &Path, bits: i32) -> i32 {
    let c = cpath(path);
    if unsafe { libc::access(c.as_ptr(), bits) } == 0 {
        0
    } else {
        errno()
    }
}

/// `getgrouplist` with room for `cap` entries. Returns the list as filled.
pub fn getgrouplist(name: &str, gid: libc::gid_t, cap: usize) -> Vec<libc::gid_t> {
    let c = cstr(name);
    let mut groups = vec![0 as libc::c_int; cap];
    let mut n = cap as libc::c_int;
    unsafe { libc::getgrouplist(c.as_ptr(), gid as libc::c_int, groups.as_mut_ptr(), &mut n) };
    let n = (n.max(0) as usize).min(cap);
    groups[..n].iter().map(|g| *g as libc::gid_t).collect()
}

pub fn getgroups() -> Vec<libc::gid_t> {
    let mut g = vec![0 as libc::gid_t; 64];
    let n = unsafe { libc::getgroups(g.len() as libc::c_int, g.as_mut_ptr()) };
    if n < 0 {
        return vec![];
    }
    g.truncate(n as usize);
    g
}

pub struct Pw {
    pub name: String,
    pub uid: libc::uid_t,
    pub gid: libc::gid_t,
}

pub fn getpwnam(name: &str) -> Option<Pw> {
    let c = cstr(name);
    let p = unsafe { libc::getpwnam(c.as_ptr()) };
    if p.is_null() {
        return None;
    }
    let p = unsafe { &*p };
    Some(Pw {
        name: unsafe { CStr::from_ptr(p.pw_name) }.to_string_lossy().into_owned(),
        uid: p.pw_uid,
        gid: p.pw_gid,
    })
}

/// Decoded wait status.
pub fn decode_status(status: i32) -> String {
    if libc::WIFEXITED(status) {
        format!("exited:{}", libc::WEXITSTATUS(status))
    } else if libc::WIFSIGNALED(status) {
        format!("signaled:{}", libc::WTERMSIG(status))
    } else {
        format!("raw:{status:#x}")
    }
}

pub fn exited_code(status: i32) -> Option<i32> {
    libc::WIFEXITED(status).then(|| libc::WEXITSTATUS(status))
}
