//! A daemon's default log, `%ProgramData%\Goetia\logs\<id>.log`: the file an elevated install makes
//! ([`ensure`]) and the shim only opens ([`open_for_append`]).
//!
//! The folder is protected ([`Descriptor::for_logs`]), so no other account can make a file in it or plant
//! a link there. The file is the account's to append to and nothing else ([`Descriptor::for_default_log`]),
//! so the shim opens it with exactly those rights. A reparse point at the file is refused on the handle that
//! would write through it, on both sides: an account that held a handle while it still had rights could set
//! one after the install checked.

use std::fs::{File, OpenOptions};
use std::io;
use std::os::windows::fs::OpenOptionsExt as _;
use std::path::{Path, PathBuf};

use windows_sys::Win32::Storage::FileSystem::FILE_FLAG_OPEN_REPARSE_POINT;

use super::folder::logs_folder;
use super::link::reparse_tag;
use super::shim::acl::{DEFAULT_LOG_ACCESS, Descriptor};
use super::sid::Sid;
use crate::error::{Error, Result};
use crate::spec::Id;

/// The default log of daemon `id` in the folder `logs`.
pub(crate) fn path_in(logs: &Path, id: &str) -> PathBuf {
    logs.join(format!("{id}.log"))
}

/// Makes the default log of `id` under `program_data` empty, or sets the permissions of the one there, to
/// those of [`Descriptor::for_default_log`] for `account`, the account the daemon's shim runs as. The file
/// is opened once, without following a link, and its permissions are set on that handle; a reparse point
/// is refused and nothing is set on it. What is in the file is kept, so an install over a daemon that has
/// logged, as another account too, leaves its log. The folder is [`make_goetia_folders`](super::folder::make_goetia_folders)'s,
/// made before: where it is not there this is an error.
pub(crate) fn ensure(program_data: &Path, id: &Id, account: &Sid) -> Result<()> {
    let path = path_in(&logs_folder(program_data), id.as_str());
    let permissions = Descriptor::for_default_log(account)?;
    let file = permissions
        .open_always_for_dacl(&path)
        .map_err(|e| Error::os_at("make", &path, e))?;
    let tag = reparse_tag(&file).map_err(|e| Error::os_at("read the type of", &path, e))?;
    if tag.is_some() {
        return Err(Error::os_at("use", &path, io::Error::other(format!("it is {LINK}"))));
    }
    permissions
        .set_dacl_of(&file)
        .map_err(|e| Error::os_at("set the permissions of", &path, e))
}

/// Opens the default log at `path`, which an install made, for append and nothing else: the rights an
/// account is granted on it ([`DEFAULT_LOG_ACCESS`]), `OPEN_EXISTING`, and not following a link. A file
/// that is not there is [`NotFound`](io::ErrorKind::NotFound): the shim makes none. A reparse point,
/// seen on the handle itself, is refused.
pub(crate) fn open_for_append(path: &Path) -> io::Result<File> {
    let file = OpenOptions::new()
        .access_mode(DEFAULT_LOG_ACCESS)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)?;
    if reparse_tag(&file)?.is_some() {
        return Err(link_error(path));
    }
    Ok(file)
}

/// What a default log that is a reparse point is: not goetia's.
const LINK: &str = "a link, which goetia did not make; delete it and install the daemon again";

fn link_error(path: &Path) -> io::Error {
    io::Error::other(format!("{} is {LINK}", path.display()))
}

#[cfg(test)]
#[path = "default_log_tests.rs"]
mod default_log_tests;
