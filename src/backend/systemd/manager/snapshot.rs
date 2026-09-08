//! Every name a directory held **at one instant**, for the two questions this backend answers from
//! names alone: which ids `Systemd::list` has to account for, and whether a quarantine sibling holds
//! an id whose `<id>.service` came up vacant.
//!
//! # Why not `fs::read_dir`
//!
//! Enumeration is not observation. `fs::read_dir` is a *cursor*: glibc reads the directory a
//! bufferful at a time, and `readdir(3)` promises only the entries that were there for the whole
//! walk — POSIX leaves an entry added or removed after `opendir` unspecified, and on ext4 with
//! `dir_index` a `rename(2)` re-hashes the name, which can place it at a slot the cursor has already
//! passed. `super::write::replace_unit_verified` renames `<id>.service` to a quarantine sibling *in
//! this same directory*, so a cursor that crossed the quarantine slot before the rename and reaches
//! the fragment slot after it sees **neither name** — from one rename, not a cycle. Measured on this
//! host with `/etc/systemd/system` padded to 1521 entries: one `read_dir` per listing missed an
//! installed id in 479 of 2000 passes, and `Systemd::status` answered `Error::NotInstalled` — the
//! variant `cli::uninstall` renders as exit `0`, "nothing to do" — for a daemon that was installed
//! the whole time.
//!
//! One `getdents64(2)` has no cursor to lose. The VFS holds the directory's `i_rwsem` for the whole
//! call while `rename`/`link`/`unlink` take it exclusively, so what comes back is the directory as it
//! was at one instant, whatever else is happening to it. That is the same kind of answer a `stat` by
//! name gives, which is why [`names`] and a stat are the only two ways this backend asks what
//! occupies an id.
//!
//! The whole directory has to come back in that one call for the guarantee to hold, so the buffer is
//! grown until it does — the kernel stops filling only when the next entry does not fit or the
//! entries run out, so slack larger than any single record proves it was the second.

use std::ffi::OsString;
use std::fs::File;
use std::io;
use std::os::fd::AsRawFd as _;
use std::os::unix::ffi::OsStringExt as _;
use std::path::Path;

/// `struct linux_dirent64`'s fixed part: `d_ino` (8), `d_off` (8), `d_reclen` (2), `d_type` (1).
/// `d_name` follows, NUL-terminated, and `d_reclen` covers the whole record including its padding.
const HEADER: usize = 19;

/// The largest record `getdents64` can emit: [`HEADER`], a `NAME_MAX` name, its NUL, rounded up to
/// the 8-byte alignment the kernel pads records to.
const MAX_RECORD: usize = (HEADER + 255 + 1).next_multiple_of(8);

/// The first buffer tried. Comfortably more than `/etc/systemd/system` holds on a normal host, so
/// the growth path below is not the usual one.
const INITIAL_CAPACITY: usize = 64 * 1024;

/// Every name `dir` held at one instant, `.` and `..` excluded and in no particular order.
///
/// Errors are the caller's to read, unchanged from [`std::fs::read_dir`]'s: `NotFound` for a
/// directory that is not there (an established absence, not a failure to determine one), `EACCES`
/// for one this caller may not open, `ENOTDIR` for a path component that is not a directory.
pub(super) fn names(dir: &Path) -> io::Result<Vec<OsString>> {
    names_from(dir, INITIAL_CAPACITY)
}

/// [`names`] with the first buffer size spelled out, so the growth path is reachable from a test
/// without a directory large enough to overflow a 64 KiB one.
fn names_from(dir: &Path, initial_capacity: usize) -> io::Result<Vec<OsString>> {
    let mut capacity = initial_capacity.max(MAX_RECORD * 2);
    loop {
        // Re-opened per attempt rather than rewound: a fresh descriptor starts at offset 0 with no
        // seek to reason about, and a retry is rare enough that the open costs nothing.
        let dirfd = File::open(dir)?;
        let mut buf = vec![0u8; capacity];
        let filled = getdents64(&dirfd, &mut buf)?;
        if capacity - filled >= MAX_RECORD {
            return parse(&buf[..filled]);
        }
        // The buffer may have been what stopped the call, so what came back is a prefix of the
        // directory rather than the directory. Grow by what the directory turned out to need, not
        // by a chosen number of attempts.
        capacity *= 2;
    }
}

/// One `getdents64(2)`, returning the bytes it filled. `libc::syscall` rather than `libc::getdents64`
/// (glibc 2.30 and later only) so this does not depend on the C library's vintage.
fn getdents64(dirfd: &File, buf: &mut [u8]) -> io::Result<usize> {
    // SAFETY: `dirfd` is an open descriptor for the length of the call, and `buf` is a slice this
    // call has exclusive access to, valid for writes of exactly the length passed alongside it.
    let filled = unsafe {
        libc::syscall(
            libc::SYS_getdents64,
            dirfd.as_raw_fd(),
            buf.as_mut_ptr().cast::<libc::c_void>(),
            buf.len(),
        )
    };
    if filled < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(filled as usize)
}

/// The `linux_dirent64` records `getdents64` filled, as names. Every field is read out of the byte
/// slice rather than through a `#[repr(C)]` struct pointer: the records are packed and unaligned,
/// which a reference to a struct may not be.
fn parse(mut bytes: &[u8]) -> io::Result<Vec<OsString>> {
    let mut names = Vec::new();
    while !bytes.is_empty() {
        let Some(header) = bytes.get(..HEADER) else {
            return Err(malformed());
        };
        let reclen = usize::from(u16::from_ne_bytes([header[16], header[17]]));
        let Some(record) = bytes.get(..reclen) else {
            return Err(malformed());
        };
        if reclen <= HEADER {
            return Err(malformed());
        }
        let name = &record[HEADER..];
        let name = &name[..name.iter().position(|b| *b == 0).unwrap_or(name.len())];
        if name != b"." && name != b".." {
            names.push(OsString::from_vec(name.to_vec()));
        }
        bytes = &bytes[reclen..];
    }
    Ok(names)
}

fn malformed() -> io::Error {
    io::Error::other("getdents64 returned a record that does not fit the buffer it filled")
}

#[cfg(test)]
#[path = "snapshot_tests.rs"]
mod snapshot_tests;
