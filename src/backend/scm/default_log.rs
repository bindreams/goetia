//! A daemon's default log, `%ProgramData%\Goetia\logs\<id>.log`: the file an elevated install makes
//! ([`ensure`]) and the shim only opens ([`open_for_append`]).
//!
//! The folder is protected ([`Descriptor::for_logs`]), so no other account can make a file in it or plant
//! a link there. The file is the account's to append to and nothing else ([`Descriptor::for_default_log`]),
//! so the shim opens it with exactly those rights. A reparse point at the file is refused on the handle that
//! would write through it, on both sides: an account that held a handle while it still had rights could set
//! one after the install checked. Only a link is refused ([`is_link`]): a file with any other reparse tag is
//! data, as for the run records.

use std::fs::{self, File, OpenOptions};
use std::io;
use std::os::windows::fs::OpenOptionsExt as _;
use std::os::windows::io::AsRawHandle as _;
use std::path::{Path, PathBuf};

use windows_sys::Wdk::Storage::FileSystem::FILE_OPEN;
use windows_sys::Win32::Foundation::{ERROR_ACCESS_DENIED, HANDLE, STATUS_DELETE_PENDING};
use windows_sys::Win32::Storage::FileSystem::{
    FILE_FLAG_OPEN_REPARSE_POINT, FILE_READ_ATTRIBUTES, FILE_STANDARD_INFO, FileStandardInfo,
    GetFileInformationByHandleEx,
};

use super::folder::logs_folder;
use super::link::is_link;
use super::runs::dir::{Dir, SHARE_ALL};
use super::shim::acl::{DEFAULT_LOG_ACCESS, Descriptor, ObjectId, index_at};
use super::sid::Sid;
use crate::error::{Error, Result};
use crate::spec::Id;

pub(crate) fn path_in(logs: &Path, id: &str) -> PathBuf {
    logs.join(format!("{id}.log"))
}

/// What an install does with a log that is already there.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Existing {
    /// Sets its owner and permissions to the ones computed.
    Set,
    /// Leaves it as it is. A file that is missing is made all the same.
    Keep,
}

/// What [`ensure`] did to the file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum EnsureOutcome {
    /// There was none, and it is made.
    Made,
    /// Its owner and permissions are set.
    Set,
    /// It is as it was ([`Existing::Keep`]).
    Kept,
}

/// Where an install of a service stands against the change of its account.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Step {
    /// Before the change lands: the log names the accounts the service may run as, old and new.
    Widen,
    /// Once it has, or where there is none: the log names the account the service now runs as.
    Narrow,
}

/// The accounts a log names, and what is done with the file there, at `step` for the new account `new` and the
/// accounts `kept` (those the service ran as before the change). `starting` is whether SCM reports the service
/// `START_PENDING`: its shim may have been launched as a kept account and has not yet opened the log, which it
/// does before it registers the control handler, so the first status it reports says it has. Any other state means
/// that run's shim holds the log, or that there is none, and a start after the change runs as `new`. The file is
/// therefore not rewritten while a start is in progress, and one that has to be made names the kept accounts too.
pub(crate) fn naming(step: Step, starting: bool, kept: &[Sid], new: &Sid) -> (Vec<Sid>, Existing) {
    let with_kept = || [kept, std::slice::from_ref(new)].concat();
    match (step, starting) {
        (Step::Widen, _) => (with_kept(), Existing::Set),
        (Step::Narrow, false) => (vec![new.clone()], Existing::Set),
        (Step::Narrow, true) => (with_kept(), Existing::Keep),
    }
}

/// Makes the default log of `id` under `program_data` empty, or, as `existing` says, sets the owner and the
/// permissions of the one there, to those of [`Descriptor::for_default_log`] for `accounts`: the accounts the
/// daemon's shim runs as, or may run as until a change lands. The permissions are computed from `accounts` and
/// never read back from the file. The file is opened once, without following a link, and its owner and
/// permissions are set on that handle (a file this open made has them already); a link is refused and nothing is
/// set on it. What is in the file is kept. The folder is
/// [`make_goetia_folders`](super::folder::make_goetia_folders)'s, made before: where it is not there this
/// is an error.
pub(crate) fn ensure(program_data: &Path, id: &Id, accounts: &[Sid], existing: Existing) -> Result<EnsureOutcome> {
    let path = path_in(&logs_folder(program_data), id.as_str());
    let mut named: Vec<Sid> = Vec::new();
    for account in accounts {
        if !named.contains(account) {
            named.push(account.clone());
        }
    }
    let permissions = Descriptor::for_default_log(&named)?;
    // Only an elevated token may assign Administrators as the owner, as the open that makes the file does;
    // an unelevated one, which no install is, sets the permissions alone.
    let owner = cosca::elevation::is_elevated();
    let (file, made) = permissions
        .open_always_for_dacl(&path, owner)
        .map_err(|e| make_error(&path, e))?;
    if is_link(&file).map_err(|e| Error::os_at("read the type of", &path, e))? {
        return Err(Error::os("use the default log", link_error(&path)));
    }
    if made {
        return Ok(EnsureOutcome::Made);
    }
    if existing == Existing::Keep {
        return Ok(EnsureOutcome::Kept);
    }
    let set = if owner {
        permissions.set_owner_and_dacl_of(&file)
    } else {
        permissions.set_dacl_of(&file)
    };
    set.map_err(|e| Error::os_at("set the permissions of", &path, e))?;
    Ok(EnsureOutcome::Set)
}

/// What an install says of a default log it made, if a run of the daemon was up: the log it made is not the file
/// the run writes. The delete of a file frees its name at once on current NTFS (POSIX semantics) even while a
/// handle is open on it, so the install made a new file, and the daemon's output goes on into the deleted one until
/// the daemon is started again. `None` for any other case.
pub(crate) fn made_while_running(
    program_data: &Path,
    id: &Id,
    outcome: EnsureOutcome,
    running: bool,
) -> Option<String> {
    let path = path_in(&logs_folder(program_data), id.as_str());
    (outcome == EnsureOutcome::Made && running).then(|| {
        format!(
            "`{id}` was running when its default log {} was made again: its output goes to the deleted log until it \
             is restarted; restart it",
            path.display()
        )
    })
}

/// Prints [`made_while_running`] when `outcome` is [`EnsureOutcome::Made`] and `running` (asked only then) says
/// the daemon is up. A `running` that cannot say is told as running, with why.
pub(crate) fn tell_if_made_while_running(
    program_data: &Path,
    id: &Id,
    outcome: EnsureOutcome,
    running: impl FnOnce() -> Result<bool>,
) {
    if outcome != EnsureOutcome::Made {
        return;
    }
    let (running, unread) = match running() {
        Ok(running) => (running, String::new()),
        Err(e) => (
            true,
            format!("could not tell whether it runs ({}); if it does, ", e.report()),
        ),
    };
    if let Some(said) = made_while_running(program_data, id, outcome, running) {
        eprintln!("warning: {unread}{said}");
    }
}

/// A deleted log whose handle a daemon still holds keeps its name where the delete is not POSIX (the daemon's
/// output handle is on the file), and an open answers access denied; the error says to stop the daemon and install
/// again. A directory at the name is told as one. The status of an open of the name tells either from a permission
/// that refuses the install: the Win32 error is access denied for all three.
fn make_error(path: &Path, e: io::Error) -> Error {
    if e.raw_os_error() == Some(ERROR_ACCESS_DENIED as i32) {
        match what_is_at(path) {
            AtName::Directory => {
                return Error::os(
                    "use the default log",
                    io::Error::other(format!(
                        "{} is a directory, which goetia did not make; remove it and install the daemon again",
                        path.display()
                    )),
                );
            }
            AtName::DeletePending => {
                return Error::os(
                    "use the default log",
                    io::Error::other(format!(
                        "{} is being deleted while a run of the daemon still writes it; stop the daemon, then install it again",
                        path.display()
                    )),
                );
            }
            AtName::Other => {}
        }
    }
    Error::os_at("make", path, e)
}

/// What a name that refused an open with access denied is.
enum AtName {
    Directory,
    DeletePending,
    Other,
}

/// What is at `path`, from an open of its attributes relative to its folder: the status says `DELETE_PENDING`
/// where the Win32 error is access denied as for a permission that refuses the open. A folder that cannot be
/// opened, or any other status, is no answer.
fn what_is_at(path: &Path) -> AtName {
    let (Some(folder), Some(name)) = (path.parent(), path.file_name()) else {
        return AtName::Other;
    };
    let Ok(folder) = Dir::open(folder) else {
        return AtName::Other;
    };
    match folder.open_in(name, FILE_READ_ATTRIBUTES, SHARE_ALL, FILE_OPEN, 0) {
        Err(STATUS_DELETE_PENDING) => AtName::DeletePending,
        Ok(file) if file.metadata().is_ok_and(|metadata| metadata.is_dir()) => AtName::Directory,
        Ok(_) | Err(_) => AtName::Other,
    }
}

/// Refuses a daemon's `logs:` that lies in goetia's logs folder under `program_data`, where the default logs
/// are: the folder is protected, so no service account can make a file in it, and a SYSTEM service named
/// there would append to another daemon's default log. The folder is the boundary, and it is compared as the
/// object it is ([`lies_in`]), not by its spelling: no file-name rule of Windows (case, 8.3 names, trailing
/// dots, a share's name for the drive) is applied to the path. The folder is made before this is called.
pub(crate) fn refuse_logs_in_folder(program_data: &Path, id: &Id, logs: Option<&Path>) -> Result<()> {
    let Some(logs) = logs else {
        return Ok(());
    };
    let folder = logs_folder(program_data);
    let identity = index_at(&folder).map_err(|e| Error::os_at("read", &folder, e))?;
    let absolute = std::path::absolute(logs).map_err(|e| Error::os_at("make absolute", logs, e))?;
    if lies_in(&absolute, identity)? {
        return Err(Error::Invalid {
            daemon: id.to_string(),
            message: format!(
                "`logs: {}` is inside goetia's own log folder {}; remove `logs:` to use the default log, or name a file elsewhere",
                logs.display(),
                folder.display()
            ),
            source: None,
        });
    }
    Ok(())
}

/// Whether the deepest part of `path` that exists, or a folder above it, is the folder `folder` identifies
/// (the volume and file index of the logs folder). The rest of the path, which no file system can say anything
/// of yet, is not looked at: whatever it names is made inside the part that exists. Only a part that is not
/// there ([`NotFound`](io::ErrorKind::NotFound)) is walked past; any other failure to resolve one is an error. Each folder is read from
/// the canonical form of that part (links followed), so a spelling that shares no text with the folder's is
/// found by what it is.
fn lies_in(path: &Path, folder: ObjectId) -> Result<bool> {
    let mut existing = None;
    for part in path.ancestors() {
        match fs::canonicalize(part) {
            Ok(canonical) => {
                existing = Some(canonical);
                break;
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(Error::os_at("resolve", part, e)),
        }
    }
    let Some(existing) = existing else {
        return Ok(false);
    };
    for part in existing.ancestors() {
        if index_at(part).map_err(|e| Error::os_at("read", part, e))? == folder {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Whether the name `file` was opened by has been deleted while the handle has been open, so the path no
/// longer leads to this file and a line written through the handle is not in the file a reader of the path
/// sees. The recovery in the README deletes the log and installs again while a shim may run, which makes a new
/// file at the same path. A file that was only moved is not deleted: the handle follows it. `file` must have
/// been opened for `FILE_READ_ATTRIBUTES`, as [`open_for_append`] does.
pub(crate) fn is_deleted(file: &File) -> io::Result<bool> {
    let mut info = FILE_STANDARD_INFO::default();
    // SAFETY: `info` is `size_of::<FILE_STANDARD_INFO>()` writable bytes; the handle is live.
    let ok = unsafe {
        GetFileInformationByHandleEx(
            file.as_raw_handle() as HANDLE,
            FileStandardInfo,
            (&raw mut info).cast(),
            size_of::<FILE_STANDARD_INFO>() as u32,
        )
    };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(info.DeletePending || info.NumberOfLinks == 0)
}

/// Opens the default log at `path`, which an install made, for append and nothing else: the rights an
/// account is granted on it ([`DEFAULT_LOG_ACCESS`]), `OPEN_EXISTING`, and not following a link. A file
/// that is not there is [`NotFound`](io::ErrorKind::NotFound): the shim makes none. A link, seen on the
/// handle itself, is refused.
pub(crate) fn open_for_append(path: &Path) -> io::Result<File> {
    let file = OpenOptions::new()
        .access_mode(DEFAULT_LOG_ACCESS)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)?;
    if is_link(&file)? {
        return Err(link_error(path));
    }
    Ok(file)
}

/// The refusal of a default log that is a link, which goetia did not make: the install's and the shim's.
fn link_error(path: &Path) -> io::Error {
    io::Error::other(format!(
        "{} is a link, which goetia did not make; delete it and install the daemon again",
        path.display()
    ))
}

#[cfg(test)]
#[path = "default_log_tests.rs"]
mod default_log_tests;
