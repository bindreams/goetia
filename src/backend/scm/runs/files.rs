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
use windows_sys::Wdk::Storage::FileSystem::{FILE_NON_DIRECTORY_FILE, FILE_OPEN_REPARSE_POINT, NtOpenFile};
use windows_sys::Win32::Foundation::{
    ERROR_ACCESS_DENIED, ERROR_SHARING_VIOLATION, GENERIC_WRITE, HANDLE, NTSTATUS, OBJ_CASE_INSENSITIVE,
    STATUS_DELETE_PENDING, STATUS_OBJECT_NAME_NOT_FOUND, STATUS_OBJECT_PATH_NOT_FOUND, UNICODE_STRING,
};
use windows_sys::Win32::Storage::FileSystem::{
    DELETE, FILE_FLAG_DELETE_ON_CLOSE, FILE_RENAME_INFO, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
    FileRenameInfo, SYNCHRONIZE, SetFileInformationByHandle,
};
use windows_sys::Win32::System::IO::IO_STATUS_BLOCK;

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
/// A delete-pending file answers access denied to an open and a delete, as a permission that refuses the
/// delete does: the Win32 error is 5 for both. Only the status under it differs, `STATUS_DELETE_PENDING`
/// against `STATUS_ACCESS_DENIED`, and that is what [`delete_is_pending`] reads.
///
/// Which account made the file is not read: a live pin of another account (after a change of account) is
/// told from a stale one by the sharing and by nothing else.
pub(super) fn delete(path: &Path) -> DeleteOutcome {
    judged(std::fs::remove_file(path), path)
}

/// What `result`, of a delete of `path` (a file, or a directory or tree that is removed with it), means:
/// see [`delete`].
pub(super) fn judged(result: io::Result<()>, path: &Path) -> DeleteOutcome {
    match result {
        Ok(()) => DeleteOutcome::Deleted,
        Err(e) if e.kind() == io::ErrorKind::NotFound => DeleteOutcome::Deleted,
        Err(e) if is_sharing_violation(&e) => DeleteOutcome::Held,
        Err(e) if e.raw_os_error() == Some(ERROR_ACCESS_DENIED as i32) && delete_is_pending(path) => {
            DeleteOutcome::Deleted
        }
        Err(e) => DeleteOutcome::Failed(e),
    }
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

/// Whether `error` says another handle has the file open without sharing what was asked.
fn is_sharing_violation(error: &io::Error) -> bool {
    error.raw_os_error() == Some(ERROR_SHARING_VIOLATION as i32)
}

/// Whether the delete of `path` that failed with access denied found the file with its delete pending,
/// or found it gone since: the kernel's own answer to an open for `DELETE`. A file the permissions
/// refuse, or one that is there and opens, is not.
fn delete_is_pending(path: &Path) -> bool {
    matches!(
        open_for_delete_status(path),
        Some(STATUS_DELETE_PENDING | STATUS_OBJECT_NAME_NOT_FOUND | STATUS_OBJECT_PATH_NOT_FOUND)
    )
}

/// The `NTSTATUS` of opening `path` for `DELETE` without following a link, or `None` if the name cannot
/// be put to `ntdll`. The Win32 call that `std` makes folds the status into an error code, which is
/// what [`delete`] cannot read the cause from.
fn open_for_delete_status(path: &Path) -> Option<NTSTATUS> {
    let mut units = nt_path(path).ok()?;
    let bytes = u16::try_from(units.len() * 2).ok()?;
    let name = UNICODE_STRING {
        Length: bytes,
        MaximumLength: bytes,
        Buffer: units.as_mut_ptr(),
    };
    let attributes = OBJECT_ATTRIBUTES {
        Length: size_of::<OBJECT_ATTRIBUTES>() as u32,
        RootDirectory: null_mut(),
        ObjectName: &name,
        Attributes: OBJ_CASE_INSENSITIVE,
        SecurityDescriptor: null(),
        SecurityQualityOfService: null(),
    };
    let mut handle: HANDLE = null_mut();
    let mut status_block = IO_STATUS_BLOCK::default();
    // SAFETY: `attributes` and the name it points at outlive the call; `handle` and `status_block` are valid
    // out-pointers. A handle that is returned is closed below.
    let status = unsafe {
        NtOpenFile(
            &mut handle,
            DELETE | SYNCHRONIZE,
            &attributes,
            &mut status_block,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            FILE_NON_DIRECTORY_FILE | FILE_OPEN_REPARSE_POINT,
        )
    };
    if status >= 0 {
        // SAFETY: `handle` was just opened and is not used again.
        unsafe { NtClose(handle) };
    }
    Some(status)
}

/// `path` as the name `ntdll` takes: absolute, under `\??\`, as UTF-16 with no terminator.
fn nt_path(path: &Path) -> io::Result<Vec<u16>> {
    let absolute: Vec<u16> = std::path::absolute(path)?.as_os_str().encode_wide().collect();
    let backslash = u16::from(b'\\');
    let verbatim = [backslash, backslash, u16::from(b'?'), backslash];
    let mut nt: Vec<u16> = r"\??\".encode_utf16().collect();
    if let Some(rest) = absolute.strip_prefix(&verbatim[..]) {
        nt.extend_from_slice(rest);
    } else if let Some(rest) = absolute.strip_prefix(&[backslash, backslash][..]) {
        nt.extend(r"UNC\".encode_utf16());
        nt.extend_from_slice(rest);
    } else {
        nt.extend_from_slice(&absolute);
    }
    Ok(nt)
}

#[cfg(test)]
#[path = "files_tests.rs"]
mod files_tests;
