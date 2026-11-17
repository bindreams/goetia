//! The file operations under [`Runs`](super::Runs), each on an entry of a [`Dir`] by its bare name and
//! through the directory's handle: a record's temp file and its rename, the pin's open modes, a read, and
//! what a failed delete means ([`delete`]).

use std::ffi::OsStr;
use std::fs::File;
use std::io::{self, Read as _};
use std::os::windows::ffi::OsStrExt as _;
use std::os::windows::io::AsRawHandle as _;

use windows_sys::Wdk::Storage::FileSystem::{
    FILE_CREATE, FILE_DELETE_ON_CLOSE, FILE_NON_DIRECTORY_FILE, FILE_OPEN, FILE_RENAME_INFORMATION,
    FileRenameInformation, NtSetInformationFile,
};
use windows_sys::Win32::Foundation::{
    ERROR_SHARING_VIOLATION, HANDLE, STATUS_DELETE_PENDING, STATUS_OBJECT_NAME_NOT_FOUND, STATUS_OBJECT_PATH_NOT_FOUND,
    STATUS_REPARSE_POINT_NOT_RESOLVED, STATUS_SHARING_VIOLATION,
};
use windows_sys::Win32::Storage::FileSystem::{
    DELETE, FILE_APPEND_DATA, FILE_LIST_DIRECTORY, FILE_READ_ATTRIBUTES, FILE_READ_DATA, FILE_SHARE_READ,
    FILE_TRAVERSE, FILE_WRITE_DATA, SYNCHRONIZE,
};
use windows_sys::Win32::System::IO::IO_STATUS_BLOCK;

use super::dir::{Dir, Listed, SHARE_ALL, SYNCHRONOUS_IO, delete_by_disposition, io_error};

/// Makes the temp file `name` for a record's bytes, which must not exist: one an interrupted write left
/// is deleted first, and the file is made new, so that nothing of the old one (its attributes, say) is
/// kept and the account needs no right to overwrite it (KB ddcbd14). Others may read it but not delete it
/// while it is held, so a write in flight is not pruned from under it. A leftover that is held (a write
/// in flight) is a sharing violation.
pub(super) fn create_temp(dir: &Dir, name: &OsStr) -> io::Result<File> {
    delete(dir, name).into_result()?;
    dir.open_in(
        name,
        TEMP_ACCESS,
        FILE_SHARE_READ,
        FILE_CREATE,
        FILE_NON_DIRECTORY_FILE | SYNCHRONOUS_IO,
    )
    .map_err(io_error)
}

/// Renames the file `file` holds to the name `to` in `dir`, replacing what is there. It is renamed through
/// the handle, relative to the directory's, so there is no moment at which it is closed and under its
/// old name: nothing can delete it between its last write and the rename. `file` must have been opened
/// with `DELETE`, and be in `dir`.
pub(super) fn rename_over(dir: &Dir, file: &File, to: &OsStr) -> io::Result<()> {
    let name: Vec<u16> = to.encode_wide().collect();
    let name_bytes = name.len() * size_of::<u16>();
    let header = std::mem::offset_of!(FILE_RENAME_INFORMATION, FileName);
    // The whole struct and then the name, not only up to the name's end: a buffer cut at the end of the
    // name came out with garbage after the new name, or was refused, for one length of path in eight.
    let total = size_of::<FILE_RENAME_INFORMATION>() + name_bytes;
    // 8-byte alignment for `FILE_RENAME_INFORMATION`, whose name runs on past its declared end.
    let mut buffer = vec![0u64; total.div_ceil(size_of::<u64>())];
    let info = buffer.as_mut_ptr().cast::<FILE_RENAME_INFORMATION>();
    // SAFETY: `buffer` holds `total` zeroed bytes at the alignment of `FILE_RENAME_INFORMATION`: room for
    // the struct and for the name, which is copied in at the name's own offset.
    unsafe {
        (*info).Anonymous.ReplaceIfExists = true;
        (*info).RootDirectory = dir.file().as_raw_handle() as HANDLE;
        (*info).FileNameLength = name_bytes as u32;
        std::ptr::copy_nonoverlapping(name.as_ptr(), info.cast::<u8>().add(header).cast::<u16>(), name.len());
    }
    let mut block = IO_STATUS_BLOCK::default();
    // SAFETY: `file` is a live handle opened with `DELETE`, `dir`'s handle is live; `info` is `total`
    // readable bytes.
    let status = unsafe {
        NtSetInformationFile(
            file.as_raw_handle() as HANDLE,
            &mut block,
            info.cast(),
            total as u32,
            FileRenameInformation,
        )
    };
    if status < 0 {
        return Err(io_error(status));
    }
    Ok(())
}

/// Creates the pin file `name` in `dir`, which must not exist. It is deleted when its last handle closes,
/// whatever ends the holder. Nobody else can delete it while it is held: the holder shares no delete, so
/// any delete of it, by any account the directory lets delete entries, is a sharing violation
/// ([`DeleteOutcome::Held`]).
pub(super) fn create_pin(dir: &Dir, name: &OsStr) -> io::Result<File> {
    dir.open_in(
        name,
        PIN_ACCESS,
        0,
        FILE_CREATE,
        FILE_NON_DIRECTORY_FILE | FILE_DELETE_ON_CLOSE | SYNCHRONOUS_IO,
    )
    .map_err(io_error)
}

/// What the handle of a pin is made with. A creating open is granted all it asks for, whatever the
/// directory's entry for the account says, so it asks for what is used and no more: `DELETE` is the
/// delete-on-close, `SYNCHRONIZE` the synchronous handle. A pin is never written.
pub(super) const PIN_ACCESS: u32 = DELETE | SYNCHRONIZE;

/// What the handle of a temp file is made with, by the same rule: write and append are the record's bytes,
/// `DELETE` the rename ([`rename_over`]), `SYNCHRONIZE` the synchronous handle. Not `FILE_GENERIC_WRITE`,
/// which adds the right to set attributes and extended attributes.
const TEMP_ACCESS: u32 = FILE_WRITE_DATA | FILE_APPEND_DATA | DELETE | SYNCHRONIZE;

/// The bytes of the file `name` in `dir`, or `None` if there is no such file. A link is opened, not
/// followed, and has no bytes of its own to read.
pub(super) fn read(dir: &Dir, name: &OsStr) -> io::Result<Option<Vec<u8>>> {
    let mut file = match dir.open_in(
        name,
        FILE_READ_DATA | FILE_READ_ATTRIBUTES | SYNCHRONIZE,
        SHARE_ALL,
        FILE_OPEN,
        FILE_NON_DIRECTORY_FILE | SYNCHRONOUS_IO,
    ) {
        Ok(file) => file,
        Err(STATUS_OBJECT_NAME_NOT_FOUND) => return Ok(None),
        Err(status) => return Err(io_error(status)),
    };
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)?;
    Ok(Some(bytes))
}

/// How [`delete`] of one file went.
pub(super) enum DeleteOutcome {
    /// The file is gone, was already, or is on its way out: its delete is pending, and it goes when the
    /// last handle on it closes.
    Deleted,
    /// Another handle holds the file, and it stays until that handle goes.
    Held,
    /// Windows refused the delete for a reason that is no handle: a permission, say.
    Failed(io::Error),
    /// The directory `dir` is a link now, though it was not when it was opened: its account made it a mount
    /// point while the handle was held. Every open relative to it fails with
    /// `STATUS_REPARSE_POINT_NOT_RESOLVED` and reaches nothing of the target (KB 78c82b1); what is left
    /// to do is to delete the link ([`Dir::remove`]), which no listing taken before it holds anything of.
    Linked,
}

impl DeleteOutcome {
    /// The outcome as a result, for a caller that needs the file gone and has nothing to wait for: held,
    /// and a directory that became a link, are errors.
    pub(super) fn into_result(self) -> io::Result<()> {
        match self {
            DeleteOutcome::Deleted => Ok(()),
            DeleteOutcome::Held => Err(io::Error::from_raw_os_error(ERROR_SHARING_VIOLATION as i32)),
            DeleteOutcome::Failed(e) => Err(e),
            DeleteOutcome::Linked => Err(io::Error::new(
                io::ErrorKind::NotADirectory,
                "the directory became a link while goetia had it open",
            )),
        }
    }
}

/// Deletes the file `name` of `dir` and says what became of it. This is the one place a failed delete is read
/// ([`Runs::remove_all`](super::Runs::remove_all), a prune and a pin's liveness all go through it). The file
/// is opened for `DELETE` relative to `dir`, without following a link. It asks for `DELETE` and nothing
/// else, not even `SYNCHRONIZE`: the directory's `FILE_DELETE_CHILD` lets an account delete an entry whose
/// own permissions it has no entry on (a file of the account the task had before), but covers no other
/// right, and an open for `DELETE | SYNCHRONIZE` was refused as access denied, a live pin of another
/// account included (KB 44fca8b). The status of the open says:
///
/// - A sharing violation says a handle holds the file without sharing delete, whoever it is: a pin's
///   holder shares none, nor does a shim writing its temp file. Held.
/// - A file that is not there, or whose delete is pending, is deleted. A file whose delete is pending
///   answers access denied to a Win32 open and delete, as a permission that refuses it does (error 5 for
///   both); only the status under it, `STATUS_DELETE_PENDING` against `STATUS_ACCESS_DENIED`, tells them
///   apart, and the open reads it.
/// - `STATUS_REPARSE_POINT_NOT_RESOLVED` says `dir` itself became a link: [`DeleteOutcome::Linked`], no
///   failure.
/// - Anything else, a directory by that name included, is a failure.
///
/// Which account made the file is not read: a live pin of another account (after a change of account) is
/// told from a stale one by the sharing and by nothing else.
pub(super) fn delete(dir: &Dir, name: &OsStr) -> DeleteOutcome {
    let file = match dir.open_in(name, DELETE, SHARE_ALL, FILE_OPEN, FILE_NON_DIRECTORY_FILE) {
        Ok(file) => file,
        Err(STATUS_OBJECT_NAME_NOT_FOUND | STATUS_OBJECT_PATH_NOT_FOUND | STATUS_DELETE_PENDING) => {
            return DeleteOutcome::Deleted;
        }
        Err(STATUS_SHARING_VIOLATION) => return DeleteOutcome::Held,
        Err(STATUS_REPARSE_POINT_NOT_RESOLVED) => return DeleteOutcome::Linked,
        Err(status) => return DeleteOutcome::Failed(io_error(status)),
    };
    match delete_by_disposition(&file) {
        Ok(()) => DeleteOutcome::Deleted,
        Err(e) if is_sharing_violation(&e) => DeleteOutcome::Held,
        Err(e) => DeleteOutcome::Failed(e),
    }
}

/// Whether `error` says another handle has the file or directory open without sharing what was asked.
pub(super) fn is_sharing_violation(error: &io::Error) -> bool {
    error.raw_os_error() == Some(ERROR_SHARING_VIOLATION as i32)
}

/// Removes the directory `name` of `dir` with everything in it, each entry through a handle opened relative
/// to its parent: a link in the tree, a mount point or a symlink, is deleted and never followed. A handle
/// anyone holds on something in the tree without sharing delete fails the removal, with the sharing
/// violation. One that is not there is already removed. [`DeleteOutcome::Linked`] says `dir` is a link now;
/// a directory in the tree that became one is deleted as a link. [`DeleteOutcome::Held`] is never the
/// answer: what is held in the tree is a failure.
pub(super) fn remove_tree(dir: &Dir, name: &OsStr) -> DeleteOutcome {
    let child = match dir.open_in(
        name,
        DELETE | FILE_LIST_DIRECTORY | FILE_TRAVERSE | FILE_READ_ATTRIBUTES | SYNCHRONIZE,
        SHARE_ALL,
        FILE_OPEN,
        SYNCHRONOUS_IO,
    ) {
        Ok(file) => file,
        Err(STATUS_OBJECT_NAME_NOT_FOUND | STATUS_OBJECT_PATH_NOT_FOUND) => return DeleteOutcome::Deleted,
        Err(STATUS_REPARSE_POINT_NOT_RESOLVED) => return DeleteOutcome::Linked,
        Err(status) => return DeleteOutcome::Failed(io_error(status)),
    };
    let child = match Dir::from_open(child, dir.path().join(name)) {
        Ok(child) => child,
        Err(e) => return DeleteOutcome::Failed(e),
    };
    if !child.is_link() {
        match child.list() {
            Ok(entries) => {
                if let Err(e) = remove_entries(&child, &entries) {
                    return DeleteOutcome::Failed(e);
                }
            }
            Err(e) => return DeleteOutcome::Failed(e),
        }
    }
    match delete_by_disposition(child.file()) {
        Ok(()) => DeleteOutcome::Deleted,
        Err(e) => DeleteOutcome::Failed(e),
    }
}

/// Deletes the `entries` a listing of `dir` gave, each with its tree. `dir` that became a link since the
/// listing has nothing of them to delete: the rest is skipped, and the caller deletes the link.
fn remove_entries(dir: &Dir, entries: &[Listed]) -> io::Result<()> {
    for entry in entries {
        let outcome = if entry.directory {
            remove_tree(dir, &entry.name)
        } else {
            delete(dir, &entry.name)
        };
        match outcome {
            DeleteOutcome::Linked => break,
            done => done.into_result()?,
        }
    }
    Ok(())
}

#[cfg(test)]
#[path = "files_tests.rs"]
mod files_tests;
