//! A task's records directory as one handle that never follows a link, and everything done to what is in
//! it through that handle.
//!
//! The account a task runs as holds `ADD_FILE` on its own empty directory, which is all it takes to make it
//! a mount point to any folder (KB 78c82b1, on windows-2022 and windows-2025); no permission closes that,
//! since an account that writes records needs the right. What the elevated side does to the directory is
//! therefore done through one handle, opened without following a link and refused when it shows one
//! ([`Dir::open`]); a removal alone takes a link and deletes it ([`Dir::open_even_if_link`]). The handle
//! is measured to stay on the directory itself when the account makes it a mount point while the handle is
//! held: its listing is empty and every open relative to it, a create too, fails with
//! `STATUS_REPARSE_POINT_NOT_RESOLVED`, so nothing reaches the target. Every child is opened relative to
//! the handle by its bare name ([`Dir::open_in`]), which does not follow a link either. There is no
//! operation on a child by its path.
//!
//! The handle is opened without `DELETE`: one with it stopped the account's rename of a record on
//! windows-2022 (KB 78c82b1). The directory is deleted through a second handle opened relative to its
//! parent ([`Dir::remove`]).

use std::ffi::{OsStr, OsString};
use std::fs::{File, OpenOptions};
use std::io;
use std::os::windows::ffi::{OsStrExt as _, OsStringExt as _};
use std::os::windows::fs::{MetadataExt as _, OpenOptionsExt as _};
use std::os::windows::io::{AsRawHandle as _, FromRawHandle as _};
use std::path::{Path, PathBuf};
use std::ptr::{null, null_mut};

use windows_sys::Wdk::Foundation::OBJECT_ATTRIBUTES;
use windows_sys::Wdk::Storage::FileSystem::{
    FILE_OPEN, FILE_OPEN_REPARSE_POINT, FILE_SYNCHRONOUS_IO_NONALERT, NtCreateFile,
};
use windows_sys::Win32::Foundation::{
    ERROR_NO_MORE_FILES, HANDLE, NTSTATUS, OBJ_CASE_INSENSITIVE, RtlNtStatusToDosError, STATUS_OBJECT_NAME_INVALID,
    UNICODE_STRING,
};
use windows_sys::Win32::Storage::FileSystem::{
    DELETE, FILE_ADD_FILE, FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_REPARSE_POINT, FILE_DISPOSITION_FLAG_DELETE,
    FILE_DISPOSITION_FLAG_IGNORE_READONLY_ATTRIBUTE, FILE_DISPOSITION_FLAG_POSIX_SEMANTICS, FILE_DISPOSITION_INFO_EX,
    FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_ID_BOTH_DIR_INFO, FILE_ID_INFO, FILE_LIST_DIRECTORY,
    FILE_READ_ATTRIBUTES, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, FILE_TRAVERSE, FileDispositionInfoEx,
    FileIdBothDirectoryInfo, FileIdBothDirectoryRestartInfo, FileIdInfo, GetFileInformationByHandleEx, READ_CONTROL,
    SYNCHRONIZE, SetFileInformationByHandle, WRITE_DAC,
};
use windows_sys::Win32::System::IO::IO_STATUS_BLOCK;

/// What every handle in a records directory shares: all of it. A holder that shared less would make
/// another party's open, the account's or the removal's, a sharing violation of its own.
pub(super) const SHARE_ALL: u32 = FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE;

/// What the handle on the directory is opened for: to list it, to make files in it and to open its
/// entries. Deleting an entry another account made needs no more of the handle: the open of the entry for
/// `DELETE` is judged against the directory's own permissions (`FILE_DELETE_CHILD`), not the handle's
/// access (KB 44fca8b). No `DELETE` on the directory itself: see the module's note.
const ACCESS: u32 = FILE_LIST_DIRECTORY | FILE_TRAVERSE | FILE_ADD_FILE | FILE_READ_ATTRIBUTES | SYNCHRONIZE;

/// The option of an [`open_in`](Dir::open_in) whose handle is read or written with `ReadFile` or
/// `WriteFile`, which need it a synchronous handle; the access must then include `SYNCHRONIZE`. A handle
/// for a delete, a rename or a query has neither: see [`Dir::open_in`].
pub(super) const SYNCHRONOUS_IO: u32 = FILE_SYNCHRONOUS_IO_NONALERT;

/// One entry of a directory's listing.
pub(super) struct Listed {
    pub name: OsString,
    /// A directory, or a link to one (a junction, a directory symlink): [`Dir::from_open`] tells which, on
    /// a handle opened without following a link.
    pub directory: bool,
}

/// A directory open for the elevated side's work on it, or for the account's own writes.
pub(super) struct Dir {
    file: File,
    path: PathBuf,
    link: bool,
}

impl Dir {
    /// Opens the directory `path` without following a link at it, and refuses one: a link where goetia made
    /// a directory is not goetia's ([`ErrorKind::NotADirectory`](io::ErrorKind::NotADirectory)). A directory
    /// that is not there is [`NotFound`](io::ErrorKind::NotFound).
    pub(super) fn open(path: &Path) -> io::Result<Dir> {
        Self::open_with(path, ACCESS)?.not_a_link()
    }

    /// [`Dir::open`] for a handle that will also write the directory's permissions.
    pub(super) fn open_for_dacl(path: &Path) -> io::Result<Dir> {
        Self::open_with(path, ACCESS | WRITE_DAC | READ_CONTROL)?.not_a_link()
    }

    /// Opens `path`, which may be a link: the handle is on the link itself ([`Dir::is_link`]), never on what
    /// it points to. For the removal, which deletes a link, and for asking whether there is anything.
    pub(super) fn open_even_if_link(path: &Path) -> io::Result<Dir> {
        Self::open_with(path, ACCESS)
    }

    fn open_with(path: &Path, access: u32) -> io::Result<Dir> {
        let file = OpenOptions::new()
            .access_mode(access)
            .share_mode(SHARE_ALL)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
            .open(path)?;
        Self::from_open(file, path.to_path_buf())
    }

    /// `file`, which was opened without following a link, as the directory at `path`.
    pub(super) fn from_open(file: File, path: PathBuf) -> io::Result<Dir> {
        let link = file.metadata()?.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0;
        Ok(Dir { file, path, link })
    }

    fn not_a_link(self) -> io::Result<Dir> {
        if self.link {
            return Err(io::Error::new(
                io::ErrorKind::NotADirectory,
                format!(
                    "{} is a link, which goetia did not make; uninstalling the task removes it",
                    self.path.display()
                ),
            ));
        }
        Ok(self)
    }

    /// Whether the directory is a link (a mount point or a symlink) and not a directory of its own.
    pub(super) fn is_link(&self) -> bool {
        self.link
    }

    pub(super) fn path(&self) -> &Path {
        &self.path
    }

    /// The handle, for the one call that takes it: the write of the directory's permissions, which needs a
    /// directory opened by [`Dir::open_for_dacl`].
    pub(super) fn file(&self) -> &File {
        &self.file
    }

    /// What the directory lists, through its own handle, in no order. Names are the file system's, as
    /// UTF-16 and not necessarily Unicode.
    pub(super) fn list(&self) -> io::Result<Vec<Listed>> {
        let mut listed = Vec::new();
        // 8-byte aligned, as `FILE_ID_BOTH_DIR_INFO` is.
        let mut buffer = vec![0u64; 8192];
        let mut class = FileIdBothDirectoryRestartInfo;
        loop {
            // SAFETY: `buffer` is writable for the length given; the handle is live and was opened to list.
            let ok = unsafe {
                GetFileInformationByHandleEx(
                    self.file.as_raw_handle() as HANDLE,
                    class,
                    buffer.as_mut_ptr().cast(),
                    (buffer.len() * size_of::<u64>()) as u32,
                )
            };
            class = FileIdBothDirectoryInfo;
            if ok == 0 {
                let e = io::Error::last_os_error();
                return if e.raw_os_error() == Some(ERROR_NO_MORE_FILES as i32) {
                    Ok(listed)
                } else {
                    Err(e)
                };
            }
            let base = buffer.as_ptr().cast::<u8>();
            let mut offset = 0usize;
            loop {
                // SAFETY: the call filled `buffer` with entries chained by `NextEntryOffset`, each at an
                // 8-byte offset from the start; the name is `FileNameLength` bytes at `FileName`.
                let (info, name) = unsafe {
                    let info = &*base.add(offset).cast::<FILE_ID_BOTH_DIR_INFO>();
                    let units = std::slice::from_raw_parts(info.FileName.as_ptr(), info.FileNameLength as usize / 2);
                    (info, OsString::from_wide(units))
                };
                if name != "." && name != ".." {
                    listed.push(Listed {
                        name,
                        directory: info.FileAttributes & FILE_ATTRIBUTE_DIRECTORY != 0,
                    });
                }
                if info.NextEntryOffset == 0 {
                    break;
                }
                offset += info.NextEntryOffset as usize;
            }
        }
    }

    /// Opens or creates the entry `name` of the directory, relative to its handle. **It never follows a
    /// link** (`FILE_OPEN_REPARSE_POINT`). `access` is what the handle is for, `share` what it lets others
    /// do, `disposition` is one of `FILE_OPEN`, `FILE_CREATE` and `FILE_OPEN_IF`, and `options` is more
    /// `NtCreateFile` options (`FILE_NON_DIRECTORY_FILE`, say).
    ///
    /// Nothing is added to `access`: `SYNCHRONIZE` is a right the entry's own permissions must give, and
    /// those of an entry another account made do not, so an open that asked for it was refused where one
    /// for `DELETE` alone is allowed by the directory's `FILE_DELETE_CHILD` (KB 44fca8b). A handle that is
    /// read or written asks for `SYNCHRONIZE` and passes [`SYNCHRONOUS_IO`].
    /// The error is the `NTSTATUS`, which tells a pending delete from a refusal where the Win32 code does not.
    pub(super) fn open_in(
        &self,
        name: &OsStr,
        access: u32,
        share: u32,
        disposition: u32,
        options: u32,
    ) -> Result<File, NTSTATUS> {
        debug_assert!(
            name.encode_wide().all(|unit| unit != u16::from(b'\\')),
            "a name, not a path"
        );
        let mut units: Vec<u16> = name.encode_wide().collect();
        let bytes = u16::try_from(units.len() * size_of::<u16>()).map_err(|_| STATUS_OBJECT_NAME_INVALID)?;
        if units.contains(&0) {
            return Err(STATUS_OBJECT_NAME_INVALID);
        }
        let name = UNICODE_STRING {
            Length: bytes,
            MaximumLength: bytes,
            Buffer: units.as_mut_ptr(),
        };
        let attributes = OBJECT_ATTRIBUTES {
            Length: size_of::<OBJECT_ATTRIBUTES>() as u32,
            RootDirectory: self.file.as_raw_handle() as HANDLE,
            ObjectName: &name,
            Attributes: OBJ_CASE_INSENSITIVE,
            SecurityDescriptor: null(),
            SecurityQualityOfService: null(),
        };
        let mut handle: HANDLE = null_mut();
        let mut block = IO_STATUS_BLOCK::default();
        // SAFETY: `attributes`, the name it points at and the handle it is relative to outlive the call;
        // `handle` and `block` are valid out-pointers.
        let status = unsafe {
            NtCreateFile(
                &mut handle,
                access,
                &attributes,
                &mut block,
                null(),
                0,
                share,
                disposition,
                options | FILE_OPEN_REPARSE_POINT,
                null(),
                0,
            )
        };
        if status < 0 {
            return Err(status);
        }
        // SAFETY: `handle` was just opened and nothing else owns it.
        Ok(unsafe { File::from_raw_handle(handle.cast()) })
    }

    /// Deletes the directory, which must be empty, or a link, which is deleted itself. It is done through a
    /// second handle, opened relative to the parent for `DELETE` and matched to this one by its file ID, so
    /// that this handle needs no `DELETE`. A directory another handle holds without sharing delete is a
    /// sharing violation, and one that is not empty is [`DirectoryNotEmpty`](io::ErrorKind::DirectoryNotEmpty).
    pub(super) fn remove(&self) -> io::Result<()> {
        let (Some(parent), Some(name)) = (self.path.parent(), self.path.file_name()) else {
            return Err(io::Error::other(format!(
                "{} has no parent to delete it from",
                self.path.display()
            )));
        };
        let parent = Dir::open(parent)?;
        let second = parent
            .open_in(name, DELETE | FILE_READ_ATTRIBUTES, SHARE_ALL, FILE_OPEN, 0)
            .map_err(io_error)?;
        if file_id(&second)? != file_id(&self.file)? {
            return Err(io::Error::other(format!(
                "{} was replaced while goetia had it open",
                self.path.display()
            )));
        }
        delete_by_disposition(&second)
    }
}

/// The error an `NTSTATUS` is, as the Win32 call that would have returned it reports it.
pub(super) fn io_error(status: NTSTATUS) -> io::Error {
    // SAFETY: a pure conversion of a number.
    let code = unsafe { RtlNtStatusToDosError(status) };
    io::Error::from_raw_os_error(code as i32)
}

/// Deletes the file or directory `file` is open on, with POSIX semantics: its name goes at once, whoever
/// else has it open and shares delete (KB 1315c05, 2b01933), and a READONLY file goes too. `file` must have
/// been opened with `DELETE`.
pub(super) fn delete_by_disposition(file: &File) -> io::Result<()> {
    let info = FILE_DISPOSITION_INFO_EX {
        Flags: FILE_DISPOSITION_FLAG_DELETE
            | FILE_DISPOSITION_FLAG_POSIX_SEMANTICS
            | FILE_DISPOSITION_FLAG_IGNORE_READONLY_ATTRIBUTE,
    };
    // SAFETY: `file` is a live handle; `info` is `size_of::<FILE_DISPOSITION_INFO_EX>()` readable bytes.
    let ok = unsafe {
        SetFileInformationByHandle(
            file.as_raw_handle() as HANDLE,
            FileDispositionInfoEx,
            (&raw const info).cast(),
            size_of::<FILE_DISPOSITION_INFO_EX>() as u32,
        )
    };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(test)]
#[path = "dir_tests.rs"]
mod dir_tests;

/// The volume and file ID of what `file` is open on, which no rename or replacement of its name changes.
fn file_id(file: &File) -> io::Result<(u64, [u8; 16])> {
    let mut info = FILE_ID_INFO::default();
    // SAFETY: `info` is `size_of::<FILE_ID_INFO>()` writable bytes; the handle is live.
    let ok = unsafe {
        GetFileInformationByHandleEx(
            file.as_raw_handle() as HANDLE,
            FileIdInfo,
            (&raw mut info).cast(),
            size_of::<FILE_ID_INFO>() as u32,
        )
    };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok((info.VolumeSerialNumber, info.FileId.Identifier))
}
