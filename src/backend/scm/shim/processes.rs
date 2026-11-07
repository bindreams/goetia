//! The images of the processes that are running, which is what tells a shim file in use from one
//! left behind. SCM's own record cannot: it marks a service `STOPPED`, with no pid, before its host
//! has exited, and a host updated to a new file keeps running the old one until it restarts.
//!
//! Every process is opened through [`open_process_with`], which enables `SeDebugPrivilege` as `kill`
//! does, and its image is read through the handle it was opened with, so a pid reused meanwhile
//! cannot mislead. A process that is still running and whose image cannot be established, because
//! the open fails for any reason other than the process not existing (a right removed by policy, a
//! protected process) or the read does, is known only by the file name the process list gives, and
//! counts as running every file of that name. A process that has exited is skipped, though its handle
//! is still held by someone.
use std::ffi::OsString;
use std::io;
use std::mem::size_of;
use std::os::windows::ffi::OsStringExt as _;
use std::os::windows::io::{AsRawHandle as _, FromRawHandle as _, OwnedHandle};
use std::path::{Path, PathBuf};

use windows_sys::Win32::Foundation::{ERROR_INSUFFICIENT_BUFFER, ERROR_NO_MORE_FILES, INVALID_HANDLE_VALUE};
use windows_sys::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW, TH32CS_SNAPPROCESS,
};
use windows_sys::Win32::System::Threading::{
    PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE, QueryFullProcessImageNameW,
};

use super::embedded::shim_file;
use super::sweep::{name_key, normalise, service_processes, unowned_processes};
use crate::backend::scm::kill::system::{has_exited, open_process_with};
use crate::error::{Error, Result};

/// What a process is opened with: to read its image and to wait for its exit.
const ACCESS: u32 = PROCESS_SYNCHRONIZE | PROCESS_QUERY_LIMITED_INFORMATION;

/// What one process of the list came to.
enum Seen {
    /// Running, held, with the pid it was opened by and the image it was started from.
    Image(OwnedHandle, u32, PathBuf),
    /// Running, but its image could not be read: the file name the process list gives.
    Unnamed(OsString),
}

/// The image of every running process, [`normalise`]d: a process that could not be read is its file
/// name alone, and only when that is a shim file's (see [`shim_file`]), since only those matter to
/// the sweep. A process that exits while the list is read is skipped.
pub(crate) fn running_images() -> Result<Vec<PathBuf>> {
    let mut images = Vec::new();
    each(|seen| match seen {
        Seen::Image(_, _, image) => images.push(normalise(&image)),
        Seen::Unnamed(name) => {
            if name.to_str().and_then(shim_file).is_some() {
                images.push(PathBuf::from(name));
            }
        }
    })?;
    Ok(images)
}

/// A handle, opened with `SYNCHRONIZE`, on every running process that runs `host` and that no live
/// service reports as its own (see [`unowned_processes`]): the processes an uninstall of the
/// service that ran `host` waits for. The processes are listed before the services are read, and
/// each is held from the list on, so the pid a service reports cannot have passed to another
/// process meanwhile. The image is read through the handle, so what is held runs `host`, never a
/// stranger that took a pid. A process whose image cannot be read is not here: it cannot be waited
/// for as one that runs `host`, but [`running_images`] still counts it.
pub(crate) fn unowned_on(host: &Path) -> Result<Vec<OwnedHandle>> {
    let host = normalise(host);
    let mut held = Vec::new();
    each(|seen| {
        if let Seen::Image(handle, pid, image) = seen
            && name_key(&image) == name_key(&host)
        {
            held.push((pid, normalise(&image), handle));
        }
    })?;
    let services = service_processes()?;
    let images: Vec<(u32, PathBuf)> = held.iter().map(|(pid, image, _)| (*pid, image.clone())).collect();
    let unowned = unowned_processes(&host, &images, &services);
    Ok(held
        .into_iter()
        .filter(|(pid, ..)| unowned.contains(pid))
        .map(|(.., handle)| handle)
        .collect())
}

/// Visits every process the list names that was still running when it was opened.
fn each(mut visit: impl FnMut(Seen)) -> Result<()> {
    let listed = list().map_err(|e| Error::os("list the running processes", e))?;
    for (pid, name) in listed {
        if let Some(seen) = open(pid, name) {
            visit(seen);
        }
    }
    Ok(())
}

/// `pid` opened and its image read, or `None` for a process that is gone. A process that cannot be
/// opened or read is [`Seen::Unnamed`], whatever the reason (a right that policy removed, a system
/// process): what it runs is unknown, never "nothing".
fn open(pid: u32, name: OsString) -> Option<Seen> {
    let handle = match open_process_with(pid, ACCESS) {
        Ok(Some(handle)) => handle,
        Ok(None) => return None,
        Err(_) => return Some(Seen::Unnamed(name)),
    };
    match image_of(&handle) {
        Ok(image) => Some(Seen::Image(handle, pid, image)),
        // An exited process keeps its handles but has no image to read. One that has not exited and
        // cannot be read is unknown.
        Err(_) => match has_exited(&handle) {
            Ok(true) => None,
            Ok(false) | Err(_) => Some(Seen::Unnamed(name)),
        },
    }
}

/// The path of the executable the process behind `process` was started from.
fn image_of(process: &OwnedHandle) -> io::Result<PathBuf> {
    let mut buf = vec![0u16; 1024];
    loop {
        let mut len = buf.len() as u32;
        // SAFETY: `process` is a live handle with `PROCESS_QUERY_LIMITED_INFORMATION`; `buf` is
        // `len` writable units, and `len` is a valid in-out parameter.
        let ok = unsafe {
            QueryFullProcessImageNameW(process.as_raw_handle(), PROCESS_NAME_WIN32, buf.as_mut_ptr(), &mut len)
        };
        if ok != 0 {
            return Ok(PathBuf::from(OsString::from_wide(&buf[..len as usize])));
        }
        let e = io::Error::last_os_error();
        if e.raw_os_error() != Some(ERROR_INSUFFICIENT_BUFFER as i32) {
            return Err(e);
        }
        buf.resize(buf.len() * 2, 0);
    }
}

/// The pid and executable file name of every process, as one snapshot of the process list sees it.
fn list() -> io::Result<Vec<(u32, OsString)>> {
    // SAFETY: no pointers; an invalid handle is the failure.
    let raw = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) };
    if raw == INVALID_HANDLE_VALUE {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: a valid handle the call above returned and nothing else owns.
    let snapshot = unsafe { OwnedHandle::from_raw_handle(raw) };

    // SAFETY: an all-zero `PROCESSENTRY32W` is valid; `dwSize` is set as the calls require.
    let mut entry: PROCESSENTRY32W = unsafe { std::mem::zeroed() };
    entry.dwSize = size_of::<PROCESSENTRY32W>() as u32;
    let mut listed = Vec::new();
    // SAFETY: `snapshot` is live and `entry` is a valid out-parameter of the size it declares.
    let mut more = unsafe { Process32FirstW(snapshot.as_raw_handle(), &mut entry) };
    loop {
        if more == 0 {
            let e = io::Error::last_os_error();
            return match e.raw_os_error() {
                Some(code) if code == ERROR_NO_MORE_FILES as i32 => Ok(listed),
                _ => Err(e),
            };
        }
        let len = entry
            .szExeFile
            .iter()
            .position(|&unit| unit == 0)
            .unwrap_or(entry.szExeFile.len());
        listed.push((entry.th32ProcessID, OsString::from_wide(&entry.szExeFile[..len])));
        // SAFETY: as above.
        more = unsafe { Process32NextW(snapshot.as_raw_handle(), &mut entry) };
    }
}

#[cfg(test)]
#[path = "processes_tests.rs"]
mod processes_tests;
