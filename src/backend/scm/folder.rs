//! The machine's well-known folders, as Windows names them. Their locations can be moved, so they
//! are asked for, never built from an environment variable a process may have changed.

use std::ffi::OsString;
use std::io;
use std::os::windows::ffi::OsStringExt as _;
use std::path::PathBuf;

use windows_sys::Win32::System::Com::CoTaskMemFree;
use windows_sys::Win32::UI::Shell::{FOLDERID_ProgramData, SHGetKnownFolderPath};
use windows_sys::core::GUID;

use super::wide::units_until_nul;
use crate::error::{Error, Result};

/// The machine's `%ProgramData%`: where goetia keeps what its services and tasks write (the shim's
/// log, the run records). The one answer to the question; nothing else reads the environment for it.
pub(crate) fn program_data() -> Result<PathBuf> {
    known_folder(&FOLDERID_ProgramData, "%ProgramData%")
}

/// The folder `id` names, for the machine. `name` stands for it in an error.
pub(crate) fn known_folder(id: &GUID, name: &str) -> Result<PathBuf> {
    let mut path: *mut u16 = std::ptr::null_mut();
    // SAFETY: the folder ID is borrowed for the call; `KF_FLAG_DEFAULT` and no token ask for the
    // machine's folder; `path` is a valid out-pointer.
    let hresult = unsafe { SHGetKnownFolderPath(id, 0, std::ptr::null_mut(), &mut path) };
    let found = if hresult >= 0 {
        // SAFETY: on success `path` is a NUL-terminated string that is live until freed below.
        Ok(PathBuf::from(OsString::from_wide(unsafe { units_until_nul(path) })))
    } else {
        Err(locate_error(name, hresult))
    };
    // SAFETY: `path` is null or what `SHGetKnownFolderPath` allocated, which the caller frees
    // with `CoTaskMemFree` whether or not the call succeeded.
    unsafe { CoTaskMemFree(path.cast()) };
    found
}

/// The error for `SHGetKnownFolderPath` failing with `hresult`. A Win32 error wrapped as an
/// `HRESULT` is read as that error.
fn locate_error(name: &str, hresult: i32) -> Error {
    const FACILITY_WIN32: u32 = 0x8007_0000;
    let code = hresult as u32;
    let source = if code & 0xFFFF_0000 == FACILITY_WIN32 {
        io::Error::from_raw_os_error((code & 0xFFFF) as i32)
    } else {
        io::Error::other(format!("HRESULT {code:#010x}"))
    };
    Error::os(format!("locate {name}"), source)
}

#[cfg(test)]
#[path = "folder_tests.rs"]
mod folder_tests;
