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
    FILE_RENAME_POSIX_SEMANTICS, FILE_RENAME_REPLACE_IF_EXISTS, FileRenameInformation, FileRenameInformationEx,
    NtSetInformationFile,
};
use windows_sys::Win32::Foundation::{
    ERROR_SHARING_VIOLATION, HANDLE, STATUS_DELETE_PENDING, STATUS_INVALID_INFO_CLASS, STATUS_OBJECT_NAME_NOT_FOUND, STATUS_OBJECT_PATH_NOT_FOUND,
    STATUS_REPARSE_POINT_NOT_RESOLVED, STATUS_SHARING_VIOLATION,
};
use windows_sys::Win32::Storage::FileSystem::{
    DELETE, FILE_APPEND_DATA, FILE_LIST_DIRECTORY, FILE_READ_ATTRIBUTES, FILE_READ_DATA, FILE_SHARE_READ,
    FILE_TRAVERSE, FILE_WRITE_DATA, SYNCHRONIZE,
};
use windows_sys::Win32::System::IO::IO_STATUS_BLOCK;

use super::dir::{Dir, Listed, SHARE_ALL, SYNCHRONOUS_IO, delete_by_disposition, io_error, is_bare};

/// Makes the temp file `name` for a record's bytes. A leftover of an interrupted write is deleted first,
/// so the file is new and the account needs no right to overwrite one; a leftover that is held (a write in
/// flight) is a sharing violation. Others may read the file but not delete it while it is held, so a prune
/// cannot take it from under the write.
pub(super) fn create_temp(dir: &Dir, name: &OsStr) -> io::Result<File> {
    match delete(dir, name) {
        DeleteOutcome::Linked => return Err(dir.link_error()),
        left => left.into_result()?,
    }
    dir.open_in(
        name,
        TEMP_ACCESS,
        FILE_SHARE_READ,
        FILE_CREATE,
        FILE_NON_DIRECTORY_FILE | SYNCHRONOUS_IO,
    )
    .map_err(|status| dir.open_error(status))
}

/// Renames the file `file` holds to the name `to` in `dir`, replacing what is there. It is renamed through
/// the handle, relative to the directory's, so there is no moment at which it is closed and under its
/// old name: nothing can delete it between its last write and the rename. `file` must have been opened
/// with `DELETE`, and be in `dir`.
///
/// The replacement has POSIX semantics: a target that others have open (a reader of the record being
/// replaced) is replaced all the same, they go on reading the old bytes, and a new open gets the new file.
/// A holder that shares no delete refuses it with `STATUS_SHARING_VIOLATION` (measured; the plain class
/// answered `STATUS_ACCESS_DENIED` for any open target). A file system that does not know the class,
/// `STATUS_INVALID_INFO_CLASS`, gets the plain `FileRenameInformation`, which cannot replace an open target.
pub(super) fn rename_over(dir: &Dir, file: &File, to: &OsStr) -> io::Result<()> {
    debug_assert!(is_bare(to), "a name, not a path");
    let name: Vec<u16> = to.encode_wide().collect();
    let name_bytes = name.len() * size_of::<u16>();
    let header = std::mem::offset_of!(FILE_RENAME_INFORMATION, FileName);
    // The whole struct and then the name: a buffer cut at the name's end is refused or garbled for some
    // name lengths.
    let total = size_of::<FILE_RENAME_INFORMATION>() + name_bytes;
    let mut buffer = vec![0u64; total.div_ceil(size_of::<u64>())];
    let info = buffer.as_mut_ptr().cast::<FILE_RENAME_INFORMATION>();
    // SAFETY: `buffer` holds `total` zeroed bytes at the alignment of `FILE_RENAME_INFORMATION`: room for
    // the struct and for the name, which is copied in at the name's own offset.
    unsafe {
        (*info).RootDirectory = dir.file().as_raw_handle() as HANDLE;
        (*info).FileNameLength = name_bytes as u32;
        std::ptr::copy_nonoverlapping(name.as_ptr(), info.cast::<u8>().add(header).cast::<u16>(), name.len());
    }
    let set = |flags: u32, class| {
        // SAFETY: `info` is `total` bytes `buffer` holds; the flags are a plain write into its first field.
        unsafe { (*info).Anonymous.Flags = flags };
        let mut block = IO_STATUS_BLOCK::default();
        // SAFETY: `file` is a live handle opened with `DELETE`, `dir`'s handle is live; `info` is `total`
        // readable bytes.
        unsafe {
            NtSetInformationFile(
                file.as_raw_handle() as HANDLE,
                &mut block,
                info.cast(),
                total as u32,
                class,
            )
        }
    };
    let mut status = set(
        FILE_RENAME_REPLACE_IF_EXISTS | FILE_RENAME_POSIX_SEMANTICS,
        FileRenameInformationEx,
    );
    if status == STATUS_INVALID_INFO_CLASS {
        // The first byte of the flags is the `ReplaceIfExists` of the plain class.
        status = set(FILE_RENAME_REPLACE_IF_EXISTS, FileRenameInformation);
    }
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
    .map_err(|status| dir.open_error(status))
}

/// What the handle of a pin is made with: what a creating open asks for is what it is granted
/// (`Descriptor::for_runs`), so it asks for what is used. `DELETE` is the delete-on-close, `SYNCHRONIZE` the
/// synchronous handle; a pin is never written.
pub(super) const PIN_ACCESS: u32 = DELETE | SYNCHRONIZE;

/// What the handle of a temp file is made with, by the same rule: write and append are the record's bytes,
/// `DELETE` the rename ([`rename_over`]), `SYNCHRONIZE` the synchronous handle. Not `FILE_GENERIC_WRITE`.
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
        Err(status) => return Err(dir.open_error(status)),
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
    /// The directory is a link now: its account made it a mount point after it was opened (see the
    /// [module's note](super::dir)). What is left to do is to delete the link ([`Dir::remove`]); no listing
    /// taken before holds anything of it.
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
/// is opened relative to `dir` for `DELETE` and nothing else, not even `SYNCHRONIZE`: the directory's
/// `FILE_DELETE_CHILD` lets an account delete an entry whose own permissions it has no entry on (a file of
/// the account the task had before), but covers no other right, and an open for `DELETE | SYNCHRONIZE` was
/// refused as access denied, a live pin of another account included (measured). The status of the open says:
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

/// Removes the directory `name` of `dir` with its tree, whatever `name` is. A link in the tree, or one that
/// a directory becomes meanwhile, is deleted and never followed. What is held in the tree without sharing
/// delete fails the removal with the sharing violation ([`DeleteOutcome::Held`] is never the answer), and
/// one that is not there is removed already. [`DeleteOutcome::Linked`] says `dir` is a link now.
pub(super) fn remove_tree(dir: &Dir, name: &OsStr) -> DeleteOutcome {
    match open_child(dir, name) {
        Ok(child) => remove_opened(&child),
        Err(outcome) => outcome,
    }
}

/// Opens the directory `name` of `dir` to list it and delete it, or says what became of it instead.
fn open_child(dir: &Dir, name: &OsStr) -> Result<Dir, DeleteOutcome> {
    let file = match dir.open_in(
        name,
        DELETE | FILE_LIST_DIRECTORY | FILE_TRAVERSE | FILE_READ_ATTRIBUTES | SYNCHRONIZE,
        SHARE_ALL,
        FILE_OPEN,
        SYNCHRONOUS_IO,
    ) {
        Ok(file) => file,
        Err(STATUS_OBJECT_NAME_NOT_FOUND | STATUS_OBJECT_PATH_NOT_FOUND) => return Err(DeleteOutcome::Deleted),
        Err(STATUS_REPARSE_POINT_NOT_RESOLVED) => return Err(DeleteOutcome::Linked),
        Err(status) => return Err(DeleteOutcome::Failed(io_error(status))),
    };
    Dir::from_open(file, dir.path().join(name)).map_err(DeleteOutcome::Failed)
}

/// Deletes the tree of `child`, an open directory, or `child` alone if it is a link. One that became a link
/// since it was opened has an empty listing, and its handle is on the link: the link is deleted.
fn remove_opened(child: &Dir) -> DeleteOutcome {
    let emptied = if child.is_link() {
        Ok(())
    } else {
        child.list().and_then(|entries| remove_entries(child, &entries))
    };
    match emptied.and_then(|()| delete_by_disposition(child.file())) {
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
