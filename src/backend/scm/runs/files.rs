//! The file operations under [`Runs`](super::Runs) that `std::fs` does not offer: a file that is
//! renamed while it is held, the pin's open modes, and what a failed delete means ([`delete`]).

use std::fs::{File, OpenOptions};
use std::io;
use std::os::windows::ffi::OsStrExt as _;
use std::os::windows::fs::OpenOptionsExt as _;
use std::os::windows::io::AsRawHandle as _;
use std::path::Path;
use std::ptr::{null, null_mut};

use windows_sys::Wdk::Foundation::{NtClose, OBJECT_ATTRIBUTES};
use windows_sys::Wdk::Storage::FileSystem::{FILE_OPEN_REPARSE_POINT, NtOpenFile};
use windows_sys::Win32::Foundation::{
    ERROR_ACCESS_DENIED, ERROR_SHARING_VIOLATION, GENERIC_WRITE, HANDLE, NTSTATUS, OBJ_CASE_INSENSITIVE,
    STATUS_DELETE_PENDING, STATUS_OBJECT_NAME_NOT_FOUND, STATUS_OBJECT_PATH_NOT_FOUND, UNICODE_STRING,
};
use windows_sys::Win32::Storage::FileSystem::{
    DELETE, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_DELETE_ON_CLOSE, FILE_READ_ATTRIBUTES, FILE_RENAME_INFO,
    FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, FileRenameInfo, SYNCHRONIZE, SetFileInformationByHandle,
};
use windows_sys::Win32::System::IO::IO_STATUS_BLOCK;

use crate::backend::scm::wide::wide_null;

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

/// How [`delete`] of one file went.
pub(super) enum DeleteOutcome {
    /// The file is gone, was already, or is on its way out: its delete is pending, and it goes when the
    /// last handle on it closes.
    Deleted,
    /// Another handle holds the file, and it stays until that handle goes.
    Held,
    /// Windows refused the delete for a reason that is no handle: a permission, say.
    Failed(io::Error),
}

/// Deletes the file `path` and says what became of it. This is the one place a failed delete is read
/// ([`Runs::remove_all`](super::Runs::remove_all), a prune and a pin's liveness all go through it):
///
/// - A sharing violation says a handle holds the file without sharing delete, whoever it is: a pin's
///   holder shares none, nor does a shim writing its temp file. Held.
/// - A file that is not there, or whose delete is pending, is deleted.
/// - Anything else is a failure.
///
/// A delete-pending file or directory answers access denied to an open and a delete, as a permission that
/// refuses the delete does: the Win32 error is 5 for both. Only the status under it differs, `STATUS_DELETE_PENDING`
/// against `STATUS_ACCESS_DENIED`, and that is what [`delete_is_pending`] reads.
///
/// Which account made the file is not read: a live pin of another account (after a change of account) is
/// told from a stale one by the sharing and by nothing else.
pub(super) fn delete(path: &Path) -> DeleteOutcome {
    match std::fs::remove_file(path) {
        Ok(()) => DeleteOutcome::Deleted,
        Err(e) if e.kind() == io::ErrorKind::NotFound => DeleteOutcome::Deleted,
        Err(e) if is_sharing_violation(&e) => DeleteOutcome::Held,
        Err(e) if is_access_denied(&e) && delete_is_pending(path) => DeleteOutcome::Deleted,
        Err(e) => DeleteOutcome::Failed(e),
    }
}

/// Whether `error` is Win32's access denied, the answer to a file or directory that is delete-pending as
/// much as to one a permission protects ([`delete_is_pending`] tells them apart).
pub(super) fn is_access_denied(error: &io::Error) -> bool {
    error.raw_os_error() == Some(ERROR_ACCESS_DENIED as i32)
}

/// Whether some handle holds the pin file `path`: [`delete`] says held. A pin nothing holds (one left by a
/// power cut) is deleted by the asking and is not held. A delete that failed is returned: it says nothing
/// about a handle.
pub(super) fn is_held(path: &Path) -> io::Result<bool> {
    match delete(path) {
        DeleteOutcome::Deleted => Ok(false),
        DeleteOutcome::Held => Ok(true),
        DeleteOutcome::Failed(e) => Err(e),
    }
}

/// Whether `error` says another handle has the file or directory open without sharing what was asked.
pub(super) fn is_sharing_violation(error: &io::Error) -> bool {
    error.raw_os_error() == Some(ERROR_SHARING_VIOLATION as i32)
}

/// Whether the delete of `path` (a file or a directory) that failed with access denied found it with its
/// delete pending, or found it gone since: the kernel's own answer to an open for `DELETE`. One the
/// permissions refuse, or one that is there and opens, is not.
pub(super) fn delete_is_pending(path: &Path) -> bool {
    matches!(
        open_for_delete_status(path),
        Some(STATUS_DELETE_PENDING | STATUS_OBJECT_NAME_NOT_FOUND | STATUS_OBJECT_PATH_NOT_FOUND)
    )
}

/// The `NTSTATUS` of opening `path` for `DELETE` without following a link, or `None` if that cannot be
/// asked. The Win32 call that `std` makes folds the status into an error code, which is what [`delete`]
/// cannot read the cause from.
///
/// The open is made relative to a handle on the parent directory, by the bare file name, so no whole path
/// is ever converted: a path past `MAX_PATH` (goetia has no long-path manifest) or in any prefix form is
/// opened by `std` as it opens everything. A parent that is not there is a path that is gone. A name with
/// a NUL in it is refused, not cut.
fn open_for_delete_status(path: &Path) -> Option<NTSTATUS> {
    let path = std::path::absolute(path).ok()?;
    let mut units = wide_null(path.file_name()?).ok()?;
    units.pop();
    let bytes = u16::try_from(units.len() * size_of::<u16>()).ok()?;
    let parent = match OpenOptions::new()
        .access_mode(FILE_READ_ATTRIBUTES | SYNCHRONIZE)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
        .open(path.parent()?)
    {
        Ok(parent) => parent,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Some(STATUS_OBJECT_PATH_NOT_FOUND),
        Err(_) => return None,
    };
    let name = UNICODE_STRING {
        Length: bytes,
        MaximumLength: bytes,
        Buffer: units.as_mut_ptr(),
    };
    let attributes = OBJECT_ATTRIBUTES {
        Length: size_of::<OBJECT_ATTRIBUTES>() as u32,
        RootDirectory: parent.as_raw_handle() as HANDLE,
        ObjectName: &name,
        Attributes: OBJ_CASE_INSENSITIVE,
        SecurityDescriptor: null(),
        SecurityQualityOfService: null(),
    };
    let mut handle: HANDLE = null_mut();
    let mut status_block = IO_STATUS_BLOCK::default();
    // SAFETY: `attributes`, the name it points at and `parent`, the handle it is relative to, outlive the
    // call; `handle` and `status_block` are valid out-pointers. A handle that is returned is closed below.
    let status = unsafe {
        NtOpenFile(
            &mut handle,
            DELETE | SYNCHRONIZE,
            &attributes,
            &mut status_block,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            FILE_OPEN_REPARSE_POINT,
        )
    };
    if status >= 0 {
        // SAFETY: `handle` was just opened and is not used again.
        unsafe { NtClose(handle) };
    }
    Some(status)
}

#[cfg(test)]
#[path = "files_tests.rs"]
mod files_tests;
