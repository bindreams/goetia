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

use windows_service::service::ServiceState;
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
use super::sid::{Sid, distinct};
use crate::error::{Error, Result};
use crate::spec::Id;

pub(crate) fn path_in(logs: &Path, id: &str) -> PathBuf {
    logs.join(format!("{id}.log"))
}

/// What an install does with a log that is already there.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Existing {
    /// Sets its owner and permissions to the ones computed for these accounts.
    Set(Vec<Sid>),
    /// Leaves it as it is.
    Keep,
}

/// The accounts a log names, and what is done with a log that is already there, which a missing one is not: that
/// is made, for [`Naming::made`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Naming {
    /// The accounts a file this install makes names.
    pub made: Vec<Sid>,
    /// What is done with a file that is there.
    pub existing: Existing,
}

impl Naming {
    /// A file that is made, and one that is there, names `accounts`.
    pub(crate) fn alike(accounts: &[Sid]) -> Self {
        Self {
            made: accounts.to_vec(),
            existing: Existing::Set(accounts.to_vec()),
        }
    }
}

/// What an install did to the file, and how far the service's run was while it did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct LogChange {
    pub outcome: EnsureOutcome,
    pub run: Run,
}

impl LogChange {
    /// Whether the file was made while a run of the service was live: the one fact that the [`Step::Leave`] of the
    /// install's next step follows.
    pub(crate) fn under_live_run(&self) -> bool {
        self.outcome == EnsureOutcome::Made && self.run.is_live()
    }

    /// The step of the same install that follows the change of account: [`Step::Leave`] for a file this step made
    /// under a live run, which narrowing would take from the shim that may reopen it, else [`Step::Narrow`].
    pub(crate) fn step_after(&self) -> Step {
        if self.under_live_run() {
            Step::Leave
        } else {
            Step::Narrow
        }
    }
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
    /// Once it has, after a step of the same install that made the log under a live run (starting or up): that
    /// log stays as it is, naming the kept accounts too, until the next install narrows it.
    Leave,
}

/// How far a service's run is, from the state SCM reports for it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Run {
    /// None. A start after the install runs as the account it names.
    Stopped,
    /// `START_PENDING`: the shim opens the log, as the account it was launched as, and spawns the daemon before it
    /// reports `RUNNING`, so it may or may not hold the log yet.
    Starting,
    /// Past its start: the shim holds the log it opened, and reopens by path, as its own account, one that was
    /// made in its place. That account is a kept one: a run started before an earlier change of account is not
    /// named.
    Up,
}

impl Run {
    /// Whether a shim of the run may hold the log, or reopen it by path: a start in progress and any run past it.
    pub(crate) fn is_live(self) -> bool {
        self != Self::Stopped
    }

    pub(crate) fn of(state: ServiceState) -> Self {
        match state {
            ServiceState::Stopped => Self::Stopped,
            ServiceState::StartPending => Self::Starting,
            ServiceState::Running
            | ServiceState::StopPending
            | ServiceState::ContinuePending
            | ServiceState::PausePending
            | ServiceState::Paused => Self::Up,
        }
    }
}

/// What a log names, and what is done with the one there, at `step` for the new account `new` and the accounts
/// `kept` (the account this install changes the service from), while the service's run is `run`. A file is not
/// rewritten while a start is in progress, and one that has to be made names the kept accounts too. A file made under
/// an up run names them as well, since that run's shim, which holds the file that was there, reopens the path as its
/// own account if it was deleted; the one there is narrowed, as that shim holds it. A run older than `kept`, started
/// before an earlier change of account, is not named: until it restarts it writes its own lines to the Event Log only.
pub(crate) fn naming(step: Step, run: Run, kept: &[Sid], new: &Sid) -> Naming {
    let with_kept = || [kept, std::slice::from_ref(new)].concat();
    match (step, run) {
        (Step::Widen, _) => Naming::alike(&with_kept()),
        (Step::Narrow, Run::Stopped) => Naming::alike(std::slice::from_ref(new)),
        (Step::Narrow, Run::Starting) | (Step::Leave, _) => Naming {
            made: with_kept(),
            existing: Existing::Keep,
        },
        (Step::Narrow, Run::Up) => Naming {
            made: with_kept(),
            existing: Existing::Set(vec![new.clone()]),
        },
    }
}

/// [`ensure`] for the [`naming`] of `step`, `kept` and `new` while the run is `run`.
pub(crate) fn make(program_data: &Path, id: &Id, step: Step, run: Run, kept: &[Sid], new: &Sid) -> Result<LogChange> {
    let outcome = ensure(program_data, id, &naming(step, run, kept, new))?;
    Ok(LogChange { outcome, run })
}

/// Makes the default log of `id` under `program_data` empty, or, as `naming` says, sets the owner and the
/// permissions of the one there, to those of [`Descriptor::for_default_log`]: for the accounts the daemon's shim
/// runs as, or may run as until a change lands. The permissions are computed from the accounts and
/// never read back from the file. The file is opened once, without following a link, and its owner and
/// permissions are set on that handle (a file this open made has them already); a link is refused and nothing is
/// set on it. What is in the file is kept. The folder is
/// [`make_goetia_folders`](super::folder::make_goetia_folders)'s, made before: where it is not there this
/// is an error.
pub(crate) fn ensure(program_data: &Path, id: &Id, naming: &Naming) -> Result<EnsureOutcome> {
    let path = path_in(&logs_folder(program_data), id.as_str());
    let made_for = Descriptor::for_default_log(&distinct(&naming.made))?;
    // Only an elevated token may assign Administrators as the owner, as the open that makes the file does;
    // an unelevated one, which no install is, sets the permissions alone.
    let owner = cosca::elevation::is_elevated();
    let (file, made) = made_for
        .open_always_for_dacl(&path, owner)
        .map_err(|e| make_error(&path, e))?;
    if is_link(&file).map_err(|e| Error::os_at("read the type of", &path, e))? {
        return Err(Error::os("use the default log", link_error(&path)));
    }
    if made {
        return Ok(EnsureOutcome::Made);
    }
    let Existing::Set(accounts) = &naming.existing else {
        return Ok(EnsureOutcome::Kept);
    };
    let permissions = Descriptor::for_default_log(&distinct(accounts))?;
    let set = if owner {
        permissions.set_owner_and_dacl_of(&file)
    } else {
        permissions.set_dacl_of(&file)
    };
    set.map_err(|e| Error::os_at("set the permissions of", &path, e))?;
    Ok(EnsureOutcome::Set)
}

/// A deleted log whose handle something still holds (a daemon's output handle, or a program reading it) keeps its
/// name where the delete is not POSIX, and an open answers access denied; the error says to end what holds it and
/// install again. A directory at the name, and a link, are told as what they are. The status of an open of the
/// name tells each of the three from a permission that refuses the install: the Win32 error is access denied for
/// all four.
fn make_error(path: &Path, e: io::Error) -> Error {
    if e.raw_os_error() == Some(ERROR_ACCESS_DENIED as i32)
        && let Some(refusal) = refusal_at(path, what_is_at(path))
    {
        return refusal;
    }
    Error::os_at("make", path, e)
}

/// What [`ensure`] refuses of what `at` says is at `path`, if it refuses it.
fn refusal_at(path: &Path, at: AtName) -> Option<Error> {
    let said = match at {
        AtName::Directory => format!(
            "{} is a directory, which goetia did not make; remove it and install the daemon again",
            path.display()
        ),
        AtName::Link => return Some(Error::os("use the default log", link_error(path))),
        AtName::DeletePending => format!(
            "{} is being deleted while something still has it open (a run of the daemon, or a program reading the \
             log); end that, then install the daemon again",
            path.display()
        ),
        AtName::Other => return None,
    };
    Some(Error::os("use the default log", io::Error::other(said)))
}

/// What [`ensure`] would refuse of the default log of `id` under `program_data` as it stands, found without
/// changing anything: a directory, a link or a pending delete at its name, told as `ensure` tells them. A file
/// that is not there, or one `ensure` would only set, passes; so does what cannot be foreseen (a write of the
/// permissions that is denied at that moment).
pub(crate) fn check(program_data: &Path, id: &Id) -> Result<()> {
    let path = path_in(&logs_folder(program_data), id.as_str());
    match refusal_at(&path, what_is_at(&path)) {
        Some(refusal) => Err(refusal),
        None => Ok(()),
    }
}

/// What a name that refused an open with access denied is.
enum AtName {
    Link,
    Directory,
    DeletePending,
    Other,
}

/// What is at `path`, from an open of its attributes relative to its folder, which does not follow a link
/// ([`Dir::open_in`]): the status says `DELETE_PENDING` where the Win32 error is access denied as for a permission
/// that refuses the open. A link to a directory is refused by the install's open as a directory is. A folder that
/// cannot be opened, or any other status, is no answer.
fn what_is_at(path: &Path) -> AtName {
    let (Some(folder), Some(name)) = (path.parent(), path.file_name()) else {
        return AtName::Other;
    };
    let Ok(folder) = Dir::open(folder) else {
        return AtName::Other;
    };
    match folder.open_in(name, FILE_READ_ATTRIBUTES, SHARE_ALL, FILE_OPEN, 0) {
        Err(STATUS_DELETE_PENDING) => AtName::DeletePending,
        Ok(file) if is_link(&file).unwrap_or(false) => AtName::Link,
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
