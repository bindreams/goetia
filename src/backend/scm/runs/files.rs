//! The file operations under [`Runs`](super::Runs) that `std::fs` does not offer: a file that is
//! renamed while it is held, and the pin's open modes.

use std::fs::{File, OpenOptions};
use std::io;
use std::os::windows::ffi::OsStrExt as _;
use std::os::windows::fs::OpenOptionsExt as _;
use std::os::windows::io::AsRawHandle as _;
use std::path::Path;

use windows_sys::Win32::Foundation::{ERROR_SHARING_VIOLATION, GENERIC_WRITE, HANDLE};
use windows_sys::Win32::Storage::FileSystem::{
    DELETE, FILE_FLAG_DELETE_ON_CLOSE, FILE_RENAME_INFO, FILE_SHARE_READ, FileRenameInfo, SetFileInformationByHandle,
};

/// Creates `path`, or empties what an interrupted write left there, for a record's bytes. Others
/// may read it but not delete it while it is held, so a write in flight is not pruned from under it.
pub(super) fn create_temp(path: &Path) -> io::Result<File> {
    OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .access_mode(GENERIC_WRITE | DELETE)
        .share_mode(FILE_SHARE_READ)
        .open(path)
}

/// Renames the file `file` holds to `to`, replacing what is there. It is renamed through the handle,
/// so there is no moment at which it is closed and under its old name: nothing can delete it between
/// its last write and the rename. `file` must have been opened with `DELETE`.
pub(super) fn rename_over(file: &File, to: &Path) -> io::Result<()> {
    let name: Vec<u16> = std::path::absolute(to)?.as_os_str().encode_wide().collect();
    let name_bytes = name.len() * size_of::<u16>();
    let header = std::mem::offset_of!(FILE_RENAME_INFO, FileName);
    // The whole struct and then the name, not only up to the name's end: a buffer cut at the end of the
    // name came out with garbage after the new name, or was refused, for one length of path in eight.
    let total = size_of::<FILE_RENAME_INFO>() + name_bytes;
    // 8-byte alignment for `FILE_RENAME_INFO`, whose name runs on past its declared end.
    let mut buffer = vec![0u64; total.div_ceil(size_of::<u64>())];
    let info = buffer.as_mut_ptr().cast::<FILE_RENAME_INFO>();
    // SAFETY: `buffer` holds `total` zeroed bytes at the alignment of `FILE_RENAME_INFO`: room for the
    // struct and for the name, which is copied in at the name's own offset.
    unsafe {
        (*info).Anonymous.ReplaceIfExists = true;
        (*info).FileNameLength = name_bytes as u32;
        std::ptr::copy_nonoverlapping(name.as_ptr(), info.cast::<u8>().add(header).cast::<u16>(), name.len());
    }
    // SAFETY: `file` is a live handle opened with `DELETE`; `info` is `total` readable bytes.
    let ok = unsafe {
        SetFileInformationByHandle(
            file.as_raw_handle() as HANDLE,
            FileRenameInfo,
            info.cast(),
            total as u32,
        )
    };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Creates the pin file `path`, which must not exist. It is deleted when its last handle closes,
/// whatever ends the holder. Nobody else can delete it while it is held: the holder shares no delete, so
/// any delete of it, by any account the directory lets delete entries, is a sharing violation
/// ([`is_held`]).
pub(super) fn create_pin(path: &Path) -> io::Result<File> {
    pin_options().create_new(true).open(path)
}

/// How a pin is opened: no sharing at all, and deleted on close.
pub(super) fn pin_options() -> OpenOptions {
    let mut options = OpenOptions::new();
    options
        .write(true)
        .access_mode(PIN_ACCESS)
        .share_mode(0)
        .custom_flags(FILE_FLAG_DELETE_ON_CLOSE);
    options
}

/// What a pin's holder has of it.
pub(super) const PIN_ACCESS: u32 = GENERIC_WRITE | DELETE;

/// Whether some handle holds the pin file `path`, and the pin is live: it tries to delete it. A pin's
/// holder shares no delete, so a sharing violation says a handle holds it; a delete that succeeds found
/// a pin nothing held (one left by a power cut), which is now gone; a file that is gone is not held.
/// Any other failure is returned: it says nothing about a handle.
///
/// This works for every account the directory lets delete entries, which is what a delete needs: the
/// pin's own permissions, which name the account that made it, are not read, so a live pin of another
/// account (after a change of account) is told from a stale one by the sharing and by nothing else.
pub(super) fn is_held(path: &Path) -> io::Result<bool> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(false),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(e) if is_sharing_violation(&e) => Ok(true),
        Err(e) => Err(e),
    }
}

/// Whether `error` says another handle has the file open without sharing what was asked.
pub(super) fn is_sharing_violation(error: &io::Error) -> bool {
    error.raw_os_error() == Some(ERROR_SHARING_VIOLATION as i32)
}

#[cfg(test)]
#[path = "files_tests.rs"]
mod files_tests;
