//! The file operations under [`Runs`](super::Runs) that `std::fs` does not offer: a file that is
//! renamed while it is held, and the pin's open modes.

use std::fs::{File, OpenOptions};
use std::io;
use std::os::windows::ffi::OsStrExt as _;
use std::os::windows::fs::OpenOptionsExt as _;
use std::os::windows::io::AsRawHandle as _;
use std::path::Path;

use windows_sys::Win32::Foundation::{GENERIC_READ, GENERIC_WRITE, HANDLE};
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
    let total = header + name_bytes;
    // 8-byte alignment for `FILE_RENAME_INFO`, whose name runs on past its declared end.
    let mut buffer = vec![0u64; total.div_ceil(size_of::<u64>())];
    let info = buffer.as_mut_ptr().cast::<FILE_RENAME_INFO>();
    // SAFETY: `buffer` holds `total` zeroed bytes at the alignment of `FILE_RENAME_INFO`: room for its
    // header and the name after it, which is copied in at the name's own offset.
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
/// it was opened with. Any failure to open it, sharing violation or a pending delete among them,
/// counts as held; a file that is gone is not.
pub(super) fn is_held(path: &Path) -> bool {
    match OpenOptions::new()
        .read(true)
        .access_mode(GENERIC_READ | DELETE)
        .share_mode(0)
        .custom_flags(FILE_FLAG_DELETE_ON_CLOSE)
        .open(path)
    {
        Ok(_stale) => false,
        Err(e) => e.kind() != io::ErrorKind::NotFound,
    }
}
