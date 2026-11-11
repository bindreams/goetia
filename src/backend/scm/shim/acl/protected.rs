//! The permissions goetia gives the stock shim file and the folders it creates, and the check that
//! an existing file still has them.
//!
//! The file is owned by Administrators; SYSTEM and Administrators have full control, Everyone read
//! and execute; the DACL is protected, so nothing it would inherit is added. A folder has the same,
//! inherited by what is made in it. `Descriptor` holds them as SDDL, the one place the spec's
//! permissions are written down, and [`is_as_wanted`] compares what a file has with them.
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
pub(crate) use system::Descriptor;

#[cfg(windows)]
mod system {
    use std::ffi::c_void;
    use std::fs::File;
    use std::io;
    use std::os::windows::io::FromRawHandle as _;
    use std::path::{Path, PathBuf};

    use windows_sys::Win32::Foundation::{GENERIC_WRITE, INVALID_HANDLE_VALUE, LocalFree};
    use windows_sys::Win32::Security::Authorization::{
        ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
    };
    use windows_sys::Win32::Security::{
        ACL, GetSecurityDescriptorDacl, GetSecurityDescriptorOwner, PSID, SECURITY_ATTRIBUTES,
    };
    use windows_sys::Win32::Storage::FileSystem::{
        CREATE_NEW, CreateDirectoryW, CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_SHARE_READ,
    };

    use super::super::{ObjectSecurity, system::describe};
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

        /// A descriptor written as SDDL.
        pub(crate) fn from_sddl(sddl: &str) -> Result<Self> {
            let wide = wide_null(sddl);
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
                return Err(Error::os(
                    "build the permissions of the shim",
                    io::Error::last_os_error(),
                ));
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
                    "read the permissions of the shim",
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
            let wide = wide(path)?;
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
            let wide = wide(path)?;
            let attributes = self.attributes();
            // SAFETY: `wide` is NUL-terminated and `attributes` points at `self.raw`, which is live
            // for the call.
            if unsafe { CreateDirectoryW(wide.as_ptr(), &attributes) } == 0 {
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

    impl Drop for Descriptor {
        fn drop(&mut self) {
            // SAFETY: `raw` was allocated by `ConvertStringSecurityDescriptorToSecurityDescriptorW`,
            // and is freed once.
            unsafe {
                LocalFree(self.raw);
            }
        }
    }

    /// `path` as NUL-terminated UTF-16; a NUL inside it would end the name early.
    fn wide(path: &Path) -> io::Result<Vec<u16>> {
        let wide = wide_null(path);
        if wide[..wide.len() - 1].contains(&0) {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "the path holds a NUL"));
        }
        Ok(wide)
    }
}

#[cfg(test)]
#[path = "protected_tests.rs"]
mod protected_tests;
