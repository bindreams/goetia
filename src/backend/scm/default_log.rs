//! A daemon's default log, `%ProgramData%\Goetia\logs\<id>.log`: the file an elevated install makes
//! ([`ensure`]) and the shim only opens ([`open_for_append`]).

use std::fs::File;
use std::io;
use std::path::{Path, PathBuf};

use super::sid::Sid;
use crate::error::Result;
use crate::spec::Id;

/// The default log of daemon `id` in the folder `logs`.
pub(crate) fn path_in(logs: &Path, id: &str) -> PathBuf {
    logs.join(format!("{id}.log"))
}

/// Makes the default log of `id` under `program_data`, or sets the permissions of the one there, to
/// those of [`Descriptor::for_default_log`](super::shim::acl::Descriptor::for_default_log) for `account`.
pub(crate) fn ensure(program_data: &Path, id: &Id, account: &Sid) -> Result<()> {
    let _ = (program_data, id, account);
    todo!()
}

/// Opens the default log at `path` for append and nothing else.
pub(crate) fn open_for_append(path: &Path) -> io::Result<File> {
    let _ = path;
    todo!()
}

#[cfg(test)]
#[path = "default_log_tests.rs"]
mod default_log_tests;
