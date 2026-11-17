//! The permissions goetia gives the stock shim file and the folders it creates, and the check that
//! an existing file still has them.
//!
//! The file is owned by Administrators; SYSTEM and Administrators have full control, Everyone read
//! and execute; the DACL is protected, so nothing it would inherit is added. A folder has the same,
//! inherited by what is made in it. `Descriptor` holds them as SDDL, the one place the spec's
//! permissions are written down, and [`is_as_wanted`] compares what a file has with them.
//! `Descriptor` also makes the folders, never adopting one that exists (`Descriptor::create_each`).
#![cfg_attr(not(windows), allow(dead_code))]

use super::{Ace, ObjectSecurity};

/// Whether `actual` is `wanted`: the same owner, protected the same way, and the same entries in
/// any order. The path is a label and is not compared. A NULL DACL is never as wanted. A callback
/// entry's condition is not read, so two callback entries that differ only in it are the same;
/// the stock permissions have none.
pub(crate) fn is_as_wanted(actual: &ObjectSecurity, wanted: &ObjectSecurity) -> bool {
    let (Some(actual_dacl), Some(wanted_dacl)) = (&actual.dacl, &wanted.dacl) else {
        return false;
    };
    actual.owner == wanted.owner && actual.protected == wanted.protected && sorted(actual_dacl) == sorted(wanted_dacl)
}

fn sorted(aces: &[Ace]) -> Vec<&Ace> {
    let mut sorted: Vec<&Ace> = aces.iter().collect();
    sorted.sort();
    sorted
}

#[cfg(windows)]
pub(crate) use system::{Descriptor, missing_components};

#[cfg(windows)]
mod system {
    use std::ffi::c_void;
    use std::fs::{self, File};
    use std::io;
    use std::os::windows::io::{AsRawHandle as _, FromRawHandle as _};
    use std::path::{Path, PathBuf};

    use windows_sys::Win32::Foundation::{GENERIC_WRITE, HANDLE, INVALID_HANDLE_VALUE, LocalFree};
    use windows_sys::Win32::Security::Authorization::{
        ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
    };
    use windows_sys::Win32::Security::{
        ACL, DACL_SECURITY_INFORMATION, GetSecurityDescriptorDacl, GetSecurityDescriptorOwner,
        PROTECTED_DACL_SECURITY_INFORMATION, PSID, SECURITY_ATTRIBUTES, SetKernelObjectSecurity,
    };
    use windows_sys::Win32::Storage::FileSystem::{
        CREATE_NEW, CreateDirectoryW, CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_SHARE_READ,
    };

    use super::super::{ObjectSecurity, system::describe};
    use crate::backend::scm::sid::{Sid, try_sid_display};
    use crate::backend::scm::wide::wide_null;
    use crate::error::{Error, Result};

    /// Administrators own it; SYSTEM (`SY`) and Administrators (`BA`) have full control (`FA`),
    /// Everyone (`WD`) read and execute (`FR`, `FX`); `P` stops inheritance.
    const FILE: &str = "O:BAD:P(A;;FA;;;SY)(A;;FA;;;BA)(A;;FRFX;;;WD)";
    /// [`FILE`], with every entry inherited by files and folders made in it (`OI`, `CI`).
    const FOLDER: &str = "O:BAD:P(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)(A;OICI;FRFX;;;WD)";
    /// The install lock: Administrators own it, SYSTEM and Administrators have every right (`GA`)
    /// and nobody else any, so no other account can open it, hold it or release it. The owner is
    /// named because the creator's default owner is the user when policy says "Object creator"
    /// for administrators, and the lock's owner check would then refuse a lock another goetia made.
    /// Naming `BA` needs it enabled in the creator's token.
    const LOCK: &str = "O:BAD:P(A;;GA;;;SY)(A;;GA;;;BA)";

    /// What names only the owner: Administrators own it, and the DACL is left for what it is made in to
    /// inherit. Naming `BA` needs it enabled in the creator's token, like [`LOCK`].
    const INHERITING: &str = "O:BA";

    /// A security descriptor that a file or folder is created with.
    pub(crate) struct Descriptor {
        raw: *mut c_void,
    }

    impl Descriptor {
        /// The permissions of the stock shim file.
        pub(crate) fn for_file() -> Result<Self> {
            Self::from_sddl(FILE)
        }

        /// The permissions of a folder made to hold the stock shim.
        pub(crate) fn for_folder() -> Result<Self> {
            Self::from_sddl(FOLDER)
        }

        /// The permissions of the install lock.
        pub(crate) fn for_lock() -> Result<Self> {
            Self::from_sddl(LOCK)
        }

        /// The permissions of the folder that holds goetia's own folders under `%ProgramData%`:
        /// Administrators own it and it inherits what `%ProgramData%` gives, as the services' logs
        /// folder under it always has.
        pub(crate) fn for_goetia() -> Result<Self> {
            Self::from_sddl(INHERITING)
        }

        /// The permissions of a task's run-record directory: Administrators own it; SYSTEM and
        /// Administrators have full control of it and of what is made in it; `principal`, the account the
        /// task runs as, may read, write, run and delete the files made in it (`0x1300af`: not change
        /// permissions or take ownership, and not set attributes or extended attributes, `0x110`; an
        /// entry only files inherit) and has, on the directory itself, the same without adding a
        /// subdirectory (`0x4`) and without deleting the directory (`DELETE`, `0x10000`), and with
        /// deleting entries (`0x40`), so that an account that replaces it can prune what it wrote: it
        /// holds no entry on the files the other made (`0x1200eb`). Without the right to set attributes
        /// it cannot make the directory READONLY, which would keep an elevated removal from deleting it.
        /// Only SYSTEM and Administrators can delete, or delete-pend, the directory. It still has
        /// `ADD_FILE`, which is all it takes to make the directory a mount point (KB 78c82b1): no mask
        /// closes that, and the elevated side never follows one (`scm::runs`). A SID
        /// that is not whole is an error.
        pub(crate) fn for_runs(principal: &Sid) -> Result<Self> {
            let principal = try_sid_display(principal).ok_or_else(|| {
                Error::os(
                    "build the permissions of a run-record directory",
                    io::Error::other("the account is not a whole SID"),
                )
            })?;
            Self::from_sddl(&format!(
                "O:BAD:P(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)(A;OIIO;0x1300af;;;{principal})(A;;0x1200eb;;;{principal})"
            ))
        }

        /// A descriptor written as SDDL.
        pub(crate) fn from_sddl(sddl: &str) -> Result<Self> {
            let what = || format!("build the permissions from the SDDL `{}`", sddl.escape_debug());
            let wide = wide_null(sddl).map_err(|e| Error::os(what(), e))?;
            let mut raw: *mut c_void = std::ptr::null_mut();
            // SAFETY: `wide` is NUL-terminated; `raw` is a valid out-pointer, and the size is not asked for.
            let ok = unsafe {
                ConvertStringSecurityDescriptorToSecurityDescriptorW(
                    wide.as_ptr(),
                    SDDL_REVISION_1,
                    &mut raw,
                    std::ptr::null_mut(),
                )
            };
            if ok == 0 {
                return Err(Error::os(what(), io::Error::last_os_error()));
            }
            Ok(Self { raw })
        }

        /// The owner, the entries and the protection this descriptor gives what is created with it.
        pub(crate) fn wanted(&self) -> Result<ObjectSecurity> {
            let mut owner: PSID = std::ptr::null_mut();
            let mut dacl: *mut ACL = std::ptr::null_mut();
            let (mut present, mut defaulted) = (0, 0);
            // SAFETY: `self.raw` is live until `drop`; the out-pointers are valid lvalues.
            let (got_owner, got_dacl) = unsafe {
                (
                    GetSecurityDescriptorOwner(self.raw, &mut owner, &mut defaulted),
                    GetSecurityDescriptorDacl(self.raw, &mut present, &mut dacl, &mut defaulted),
                )
            };
            if got_owner == 0 || got_dacl == 0 {
                return Err(Error::os(
                    "read the permissions of a descriptor",
                    io::Error::last_os_error(),
                ));
            }
            // SAFETY: `owner` and `dacl` point into `self.raw`, which is live for the call; the DACL
            // is null only when the descriptor has none, which `describe` reports as a NULL DACL.
            unsafe { describe(self.raw, owner, dacl, PathBuf::new(), false) }
        }

        /// Creates `path`, which must not exist, for writing, with this descriptor. The file is
        /// readable by others while it is open, and cannot be renamed or deleted until it is closed.
        pub(crate) fn create_file(&self, path: &Path) -> io::Result<File> {
            let wide = wide_null(path)?;
            let attributes = self.attributes();
            // SAFETY: `wide` is NUL-terminated and `attributes` points at `self.raw`, which is live
            // for the call; no template file.
            let handle = unsafe {
                CreateFileW(
                    wide.as_ptr(),
                    GENERIC_WRITE,
                    FILE_SHARE_READ,
                    &attributes,
                    CREATE_NEW,
                    FILE_ATTRIBUTE_NORMAL,
                    std::ptr::null_mut(),
                )
            };
            if handle == INVALID_HANDLE_VALUE {
                return Err(io::Error::last_os_error());
            }
            // SAFETY: `handle` is a valid file handle that nothing else owns.
            Ok(unsafe { File::from_raw_handle(handle) })
        }

        /// Creates the folder `path`, whose parent exists, with this descriptor.
        pub(crate) fn create_dir(&self, path: &Path) -> io::Result<()> {
            let wide = wide_null(path)?;
            let attributes = self.attributes();
            // SAFETY: `wide` is NUL-terminated and `attributes` points at `self.raw`, which is live
            // for the call.
            if unsafe { CreateDirectoryW(wide.as_ptr(), &attributes) } == 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        }

        /// Creates each of `paths` in order, as folders with this descriptor, each with a call that
        /// fails when it exists, so that goetia never adopts a folder it did not make: see [`created`].
        pub(crate) fn create_each(&self, paths: &[PathBuf]) -> Result<()> {
            for path in paths {
                self.create_dir(path).map_err(|e| created(path, e))?;
            }
            Ok(())
        }

        /// Makes this descriptor's DACL, protected, the DACL of the existing folder `file` is open on, and
        /// changes nothing else: not the owner, and not what is in the folder. `file` must have been opened
        /// with `WRITE_DAC`, and not following a link. It is `SetKernelObjectSecurity` on the handle:
        /// `SetNamedSecurityInfo` and `SetSecurityInfo` apply the new DACL to everything that inherits it,
        /// which reaches through a hard link in the folder to its target wherever that is; the kernel's
        /// call does not.
        pub(crate) fn set_dacl_of(&self, file: &File) -> io::Result<()> {
            // SAFETY: `file` is open with `WRITE_DAC`, and `self.raw` is a live descriptor.
            let set = unsafe {
                SetKernelObjectSecurity(
                    file.as_raw_handle() as HANDLE,
                    DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
                    self.raw,
                )
            };
            if set == 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        }

        /// The attributes an object is created with to have this descriptor. Valid while `self` is.
        pub(crate) fn attributes(&self) -> SECURITY_ATTRIBUTES {
            SECURITY_ATTRIBUTES {
                nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
                lpSecurityDescriptor: self.raw,
                bInheritHandle: 0,
            }
        }
    }

    /// The folders of `dir` that do not exist, outermost first: those below its first existing
    /// ancestor.
    pub(crate) fn missing_components(dir: &Path) -> Result<Vec<PathBuf>> {
        let mut missing = Vec::new();
        for ancestor in dir.ancestors() {
            match fs::symlink_metadata(ancestor) {
                Ok(_) => break,
                Err(e) if e.kind() == io::ErrorKind::NotFound => missing.push(ancestor.to_path_buf()),
                Err(e) => return Err(Error::os_at("read the type of", ancestor, e)),
            }
        }
        missing.reverse();
        Ok(missing)
    }

    /// The error for `error`, from creating `path`, which was found missing. A folder that exists
    /// now was made by someone else since, and goetia cannot tell a hostile one from another's (a
    /// non-administrator can add folders under `C:\` and `%ProgramData%`, and could leave a link): it
    /// is refused.
    pub(super) fn created(path: &Path, error: io::Error) -> Error {
        if error.kind() != io::ErrorKind::AlreadyExists {
            return Error::os_at("create", path, error);
        }
        Error::Step {
            what: format!(
                "refusing `{}`: it appeared after goetia found it missing, so goetia did not make it and cannot \
                 vouch for it",
                path.display()
            ),
            source: Box::new(Error::AlreadyExists {
                path: path.to_path_buf(),
            }),
        }
    }

    impl Drop for Descriptor {
        fn drop(&mut self) {
            // SAFETY: `raw` was allocated by `ConvertStringSecurityDescriptorToSecurityDescriptorW`,
            // and is freed once.
            unsafe {
                LocalFree(self.raw);
            }
        }
    }
}

#[cfg(test)]
#[path = "protected_tests.rs"]
mod protected_tests;
