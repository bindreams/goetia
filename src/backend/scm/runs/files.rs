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
    DELETE, FILE_FLAG_DELETE_ON_CLOSE, FILE_RENAME_INFO, FILE_SHARE_READ, FileRenameInfo, SYNCHRONIZE,
    SetFileInformationByHandle,
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
/// whatever ends the holder, and others may delete it, which [`Runs::remove_all`](super::Runs::remove_all) does.
pub(super) fn create_pin(path: &Path) -> io::Result<File> {
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .access_mode(GENERIC_WRITE | DELETE)
        .custom_flags(FILE_FLAG_DELETE_ON_CLOSE)
        .open(path)
}

/// Whether some handle holds the pin file `path`, and the pin is live. An exclusive open succeeds
/// only on a file that nothing holds (a pin left by a power cut), and that file goes with the handle
/// it was opened with. A sharing violation says a handle holds it; a file that is gone is not held.
/// Any other failure is returned: it says nothing about a handle. Access denied is the one to expect: a
/// pin a previous principal left has no entry for this one, and a file whose delete is pending
/// (another handle is open on it, its holder closed it) fails an open with access denied too, so the
/// two cannot be told apart by the open.
///
/// It asks for no more than the delete needs (`DELETE` and `SYNCHRONIZE`, which `CreateFile` always
/// adds), no read.
pub(super) fn is_held(path: &Path) -> io::Result<bool> {
    match OpenOptions::new()
        .access_mode(DELETE | SYNCHRONIZE)
        .share_mode(0)
        .custom_flags(FILE_FLAG_DELETE_ON_CLOSE)
        .open(path)
    {
        Ok(_stale) => Ok(false),
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
