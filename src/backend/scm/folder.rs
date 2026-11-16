//! The machine's well-known folders, as Windows names them. Their locations can be moved, so they
//! are asked for, never built from an environment variable a process may have changed.

use std::ffi::OsString;
use std::io;
use std::os::windows::ffi::OsStringExt as _;
use std::path::{Path, PathBuf};

use windows_sys::Win32::System::Com::CoTaskMemFree;
use windows_sys::Win32::UI::Shell::{FOLDERID_ProgramData, SHGetKnownFolderPath};
use windows_sys::core::GUID;

use super::shim::acl::{Descriptor, missing_components, require_acls_under};
use super::wide::units_until_nul;
use crate::error::{Error, Result};

/// The machine's `%ProgramData%`: where goetia keeps what its services and tasks write (the shim's
/// log, the run records). The one answer to the question; nothing else reads the environment for it.
pub(crate) fn program_data() -> Result<PathBuf> {
    known_folder(&FOLDERID_ProgramData, "%ProgramData%")
}

/// `<program_data>\Goetia`: the folder that holds goetia's own folders.
pub(crate) fn goetia_folder(program_data: &Path) -> PathBuf {
    program_data.join("Goetia")
}

/// `<program_data>\Goetia\logs`: where the shim writes a daemon's default log.
pub(crate) fn logs_folder(program_data: &Path) -> PathBuf {
    goetia_folder(program_data).join("logs")
}

/// Makes `Goetia` and `Goetia\logs` under `program_data` ([`Descriptor::for_goetia`]: owned by
/// Administrators, inheriting what `%ProgramData%` gives). Only an elevated install makes them: a shim
/// that creates them runs as the daemon's account, which would own them, and a standard user can make
/// either first, as `%ProgramData%` lets any user create a folder.
///
/// Each folder is judged by `judge` before it is made and after, so that what a folder inherits from
/// its ancestors is judged once it is there, and what it is left as is judged whichever step found
/// it: one that is there and does not pass, or that appears after it was found missing, is refused,
/// never repaired or adopted. Call it from every elevated write that registers a shim.
pub(crate) fn make_goetia_folders(program_data: &Path, judge: impl Fn(&Path) -> Result<()>) -> Result<()> {
    for folder in [goetia_folder(program_data), logs_folder(program_data)] {
        judge(&folder)?;
        require_acls_under(&folder)?;
        let missing = missing_components(&folder)?;
        if !missing.is_empty() {
            Descriptor::for_goetia()?.create_each(&missing)?;
        }
        judge(&folder)?;
    }
    Ok(())
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
