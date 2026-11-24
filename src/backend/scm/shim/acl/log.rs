//! The check of a daemon's own `logs:` path, which the shim, running as the daemon's account, opens for appending
//! and follows no link to: `install` judges it, so that no account that can change the path makes the daemon's
//! account create or append to a file it names.
//!
//! The log's folder is the one the log's name is looked up in once links are followed (a junction on the way leads
//! to it): that object is read as itself and judged strictly ([`ObjectKind::LogFolder`]), and every folder on the way
//! as any folder is ([`verdict`]). The file is judged where it is: a file that is there by its own permissions,
//! refused outright when it is a link or has more than one name (a hard link planted while the folder could be
//! added to would survive the folder's remedy); a file the shim would make by what it would inherit
//! ([`inherited_file`]). A folder that is missing is refused: the shim makes none.

use std::fs::{self, OpenOptions};
use std::io;
use std::os::windows::fs::OpenOptionsExt as _;
use std::path::{Path, PathBuf};

use windows_sys::Win32::Storage::FileSystem::{
    FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_READ_ATTRIBUTES, READ_CONTROL,
};

use super::*;
use crate::backend::scm::link::{is_link, standard_info};
use crate::backend::scm::sid::sid_from_parts;
use crate::error::log_folder_remedy;
use crate::spec::Id;

/// `OBJECT_INHERIT_ACE`: the entry is passed on to the files made in the folder it is on.
const OBJECT_INHERIT_ACE: u8 = 0x01;

impl FolderCheck {
    /// `Ok` under [`Disable`](CheckPermissions::Disable), without reading anything. Otherwise judges `log`, the
    /// `logs:` path of the daemon `id`, which runs as `daemon`: what the module documentation says. A refusal for
    /// the permissions of a folder on the way, of the log's own folder or of the log is
    /// [`Error::FolderUnsafe`] for a daemon's log; one for what the path is or where (a folder that does not exist,
    /// a link, a file with more than one name, a folder where a file is wanted or the other way round) is
    /// [`Error::Invalid`]. Nothing is made.
    pub(crate) fn check_log(&self, id: &Id, log: &Path, daemon: &Sid) -> Result<()> {
        if self.mode == CheckPermissions::Disable {
            return Ok(());
        }
        let refuse = |message: String| Error::Invalid {
            daemon: id.to_string(),
            message,
            source: None,
        };
        let absolute = std::path::absolute(log).map_err(|e| Error::os_at("make absolute", log, e))?;
        let (Some(folder), Some(name)) = (absolute.parent(), absolute.file_name()) else {
            return Err(refuse(format!("`logs: {}` does not name a file", log.display())));
        };
        let daemon_name = account_name(daemon);
        let daemon_sid = sid_display(daemon);
        let remedy = |folder: &Path| log_folder_remedy(folder, &daemon_name, &daemon_sid);

        let final_folder = match fs::canonicalize(folder) {
            Ok(final_folder) => final_folder,
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                return Err(refuse(format!(
                    "`logs: {}` is in a folder that does not exist ({}), and the shim does not make it; {}",
                    log.display(),
                    first_missing(folder)?.display(),
                    remedy(folder)
                )));
            }
            Err(e) => return Err(Error::os_at("resolve", folder, e)),
        };
        let shown_folder = display_path(&final_folder);
        let mut own = read_path(&final_folder)?;
        if own.kind != ObjectKind::Folder {
            return Err(refuse(format!(
                "`logs: {}` is in {}, which is not a folder",
                log.display(),
                shown_folder.display()
            )));
        }
        let made = inherited_file(&own, daemon, shown_folder.join(name));
        own.kind = ObjectKind::LogFolder;
        let mut chain = system::read_chain(folder)?;
        chain.insert(0, own);

        match open_as_itself(&absolute)? {
            None => chain.push(made),
            Some(file) => {
                let info = standard_info(&file).map_err(|e| Error::os_at("read", &absolute, e))?;
                if is_link(&file).map_err(|e| Error::os_at("read the type of", &absolute, e))? {
                    return Err(refuse(format!(
                        "`logs: {}` is a link, which the daemon's log does not follow; remove it, or name the file the \
                         log is to be in",
                        log.display()
                    )));
                }
                if info.Directory {
                    return Err(refuse(format!(
                        "`logs: {}` is a folder; name the file the log is to be in",
                        log.display()
                    )));
                }
                if info.NumberOfLinks > 1 {
                    return Err(refuse(format!(
                        "`logs: {}` has more than one name (a hard link), so appending to it may change a file that is \
                         somewhere else; delete the extra names, or name another file",
                        log.display()
                    )));
                }
                chain.push(read_object(&file, display_path(&absolute))?);
            }
        }

        verdict(&chain, Some(daemon)).map_err(|refusal| {
            refusal.into_error_as(FolderRole::DaemonLog {
                folder: shown_folder,
                daemon: daemon_name.clone(),
                daemon_sid: daemon_sid.clone(),
            })
        })
    }
}

/// The first folder of `folder`'s path that does not exist: the one to make first. `folder` is not there.
fn first_missing(folder: &Path) -> Result<&Path> {
    let mut missing = folder;
    for ancestor in folder.ancestors() {
        match fs::canonicalize(ancestor) {
            Ok(_) => break,
            Err(e) if e.kind() == io::ErrorKind::NotFound => missing = ancestor,
            Err(e) => return Err(Error::os_at("resolve", ancestor, e)),
        }
    }
    Ok(missing)
}

/// The file at `path`, opened as itself (a link is not followed) for its permissions and attributes alone, or
/// `None` when nothing is there.
fn open_as_itself(path: &Path) -> Result<Option<fs::File>> {
    match OpenOptions::new()
        .access_mode(READ_CONTROL | FILE_READ_ATTRIBUTES)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)
    {
        Ok(file) => Ok(Some(file)),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(Error::os_at("read the permissions of", path, e)),
    }
}

/// The file the shim makes at `path` in `folder`, as its permissions will be: the daemon's account owns it, and it
/// holds each entry of the folder that is passed on to files, whether or not the entry applies to the folder itself
/// (an entry that is inherit-only on the folder is the one to look for). `CREATOR OWNER` and `OWNER RIGHTS` are
/// the file's owner, the daemon's account. `CREATOR GROUP` is the primary group of that account, which is not read
/// and is not trusted: an entry for it that lets it tamper with the file is refused.
fn inherited_file(folder: &ObjectSecurity, daemon: &Sid, path: PathBuf) -> ObjectSecurity {
    let owners = [sid_from_parts(3, &[0]), sid_from_parts(3, &[4])];
    let dacl = folder
        .dacl
        .iter()
        .flatten()
        .filter(|ace| ace.flags & OBJECT_INHERIT_ACE != 0)
        .map(|ace| Ace {
            trustee: if owners.contains(&ace.trustee) {
                daemon.clone()
            } else {
                ace.trustee.clone()
            },
            flags: 0,
            ..ace.clone()
        })
        .collect();
    ObjectSecurity {
        path,
        owner: daemon.clone(),
        protected: false,
        kind: ObjectKind::File,
        dacl: Some(dacl),
    }
}

#[cfg(test)]
#[path = "log_tests.rs"]
mod log_tests;
