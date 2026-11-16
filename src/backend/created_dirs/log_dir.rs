//! The check systemd's `append:` needs of a log's directory.
//!
//! systemd opens an `append:` log as root, before it drops to the daemon's account, and follows a
//! symlink there (measured on systemd 255). So whoever can write the log's directory can plant a link
//! at the log's name before the first start, and whoever can write an ancestor of it can rename the
//! directory away and make one of their own. The directory is therefore refused while an account other
//! than root can change it, as it is when install runs. A symlink on the way is judged by the
//! directory that holds it, which the walk passed too: its owner cannot change where it leads. Where
//! that directory is sticky and a group or everyone can write it (`/tmp`), the owner of an entry can
//! delete it and make another, so a link there is refused unless root owns it.
//!
//! The group of a directory root owns is trusted above the log's own directory, as systemd's
//! `LogsDirectory=` trusts it: Debian and Ubuntu ship `/var/log` as `root:syslog 0775`, and
//! `/var/log/<app>/app.log` is accepted under it. The log's own directory is not trusted so, since a
//! group member there can plant a link at the log's name. Only the group is trusted, not the accounts
//! an ACL names: where a directory has an access ACL its group bits are the ACL's mask, so one that
//! can be written is judged as writable by others.

use std::io;
use std::os::fd::{AsFd as _, AsRawFd as _, BorrowedFd};
use std::path::Path;

use nix::fcntl::{OFlag, openat};
use nix::sys::stat::{Mode, SFlag};
use nix::unistd::{Gid, Group};

use super::walk::{End, Entry, Step, entry};
use super::{CreatedDirs, Plan};
use crate::backend::account::describe_uid;
use crate::error::{Error, Result};

/// Refuse a log whose directory, or an ancestor, a user other than root can change, or whose existing
/// entry is a symlink or another user's file (see the module doc), before anything is made. Sticky and
/// the group of a root-owned directory excuse ancestors only. The error names the writer, and where the
/// log's own directory is refused but a directory made in it would be accepted, and `created` would make
/// one there (it is under one of its roots), suggests a directory of its own named for `app`, the
/// daemon's id.
///
/// Whether the log's directory is under a root is asked here only when the suggestion depends on it, of a
/// directory that exists (`plan` asks it of one that does not): a root that cannot be read fails the check.
pub(crate) fn refuse_writable_log_dir(logs: &Path, plan: &Plan, app: &str, created: &CreatedDirs) -> Result<()> {
    let Some(dir) = logs.parent().filter(|dir| !dir.as_os_str().is_empty()) else {
        return Ok(());
    };
    for step in &plan.found.trail {
        // The log lands in the directory the path resolves to, whatever step the walk reached it by:
        // `.` and `..`, or a link to either, bring it back to one it has passed.
        let leaf = !step.is_link()
            && matches!(&plan.found.end, End::Whole { stat, .. }
                if step.stat.st_dev == stat.st_dev && step.stat.st_ino == stat.st_ino);
        let found = writer(step, leaf).map_err(|e| Error::DirectoryRefused {
            path: dir.to_path_buf(),
            why: format!(
                "whether the group of {} may be trusted is not known, as its access ACL could not be read ({e}); \
                 use a log directory under directories only root can change, or drop `logs:` to log to the journal",
                step.at.display()
            ),
        })?;
        let Some(Writer { who, ancestor_passes }) = found else {
            continue;
        };
        let kind = if step.is_link() { "symlink" } else { "directory" };
        let advice = match logs.file_name() {
            Some(file) if leaf && ancestor_passes && created.under_a_root(&plan.found, dir)? => {
                format!(
                    "use a directory of its own, such as {}, or one that only root can change",
                    dir.join(app).join(file).display()
                )
            }
            _ => format!(
                "use a log directory that only root can change, under directories only root can change (a missing \
                 one is made under {}), or drop `logs:` to log to the journal",
                created.roots_text()
            ),
        };
        return Err(Error::DirectoryRefused {
            path: dir.to_path_buf(),
            why: format!(
                "the {kind} {} can be changed by {who}, who could {LED_ASTRAY}; {advice}",
                step.at.display()
            ),
        });
    }
    // A directory only root can change can still hold an account-owned entry, or a link the open follows.
    if let (End::Whole { dir: held, .. }, Some(name)) = (&plan.found.end, logs.file_name()) {
        let why = match entry(held.as_fd(), name).map_err(|e| Error::os_at("stat", logs, e))? {
            Some(Entry::Link(..)) => format!(
                "{} is a symlink, and root's open of the log (systemd opens it before it drops privileges) \
                follows it to wherever it leads; remove it, or use a log that is not a link",
                logs.display()
            ),
            Some(found) if found.uid() != 0 => format!(
                "{} is owned by {}, not root, and root's open of the log (systemd opens it before it drops \
                privileges) would append to it; use a log file that root owns",
                logs.display(),
                describe_uid(found.uid())
            ),
            _ => return Ok(()),
        };
        return Err(Error::DirectoryRefused {
            path: dir.to_path_buf(),
            why,
        });
    }
    Ok(())
}

/// What a directory root's open of the log passes through must not let an account do.
const LED_ASTRAY: &str = "make root's open of the log (systemd opens it before it drops privileges, and follows a \
                          symlink) write any file";

/// Who can change what a step passed, and whether the same directory would be accepted as an ancestor.
struct Writer {
    who: String,
    ancestor_passes: bool,
}

/// Who besides root can change what `step` passed, if anyone: the owner of a directory, or the group or
/// everyone who can write it. Unless `leaf`, the directory the log lands in, the sticky bit excuses
/// everyone's writing and the group of a root-owned directory is trusted, unless it has an ACL, whose
/// accounts the group bits then stand for. A symlink has no one but its owner where its holder is sticky
/// and others can write it; otherwise whoever can write its holder is named at the holder's own step.
/// The ACL is read only here, of a root-owned directory whose group's trust is being decided; an error
/// other than "no ACL" is the caller's to refuse the log with.
fn writer(step: &Step, leaf: bool) -> io::Result<Option<Writer>> {
    let named = |who: String| {
        Some(Writer {
            who,
            ancestor_passes: false,
        })
    };
    if step.is_link() {
        return Ok(step.owner_who_can_replace().and_then(|uid| {
            named(format!(
                "its owner {}, who can replace it in the sticky directory that holds it",
                describe_uid(uid)
            ))
        }));
    }
    if step.stat.st_uid != 0 {
        return Ok(named(format!("its owner {}", describe_uid(step.stat.st_uid))));
    }
    let mode = step.stat.st_mode as libc::mode_t;
    let directory = SFlag::from_bits_truncate(step.stat.st_mode) & SFlag::S_IFMT == SFlag::S_IFDIR;
    let sticky = mode & libc::S_ISVTX != 0;
    if !directory || (sticky && !leaf) {
        return Ok(None);
    }
    // The sticky bit excuses an ancestor, not the log's own directory.
    let found = |who: String, ancestor_passes: bool| Some(Writer { who, ancestor_passes });
    if mode & libc::S_IWOTH != 0 {
        return Ok(found("every user, which can write it".to_string(), sticky));
    }
    if mode & libc::S_IWGRP == 0 {
        return Ok(None);
    }
    let acl = match &step.dir {
        Some(dir) => has_access_acl(dir.as_fd())?,
        None => false,
    };
    if acl {
        return Ok(found(
            "the accounts its ACL names, which can write it".to_string(),
            sticky,
        ));
    }
    Ok(leaf.then(|| Writer {
        who: match Group::from_gid(Gid::from_raw(step.stat.st_gid)) {
            Ok(Some(group)) => format!(
                "the members of group `{}` (gid {}), which can write it",
                group.name, step.stat.st_gid
            ),
            _ => format!("the members of gid {}, which can write it", step.stat.st_gid),
        },
        ancestor_passes: true,
    }))
}

/// Whether the directory `dir` has an access ACL: an extended attribute that exists only where the ACL
/// has an entry beyond the mode's own. `fgetxattr` refuses an `O_PATH` descriptor, so the directory is
/// opened again through it, which is the same directory, not a name looked up again. No ACL, and a file
/// system that has none, are `false`; any other error is the file system's to say.
fn has_access_acl(dir: BorrowedFd<'_>) -> io::Result<bool> {
    let again = openat(
        dir,
        ".",
        OFlag::O_RDONLY | OFlag::O_DIRECTORY | OFlag::O_CLOEXEC,
        Mode::empty(),
    )?;
    // SAFETY: `again` is open and the name is NUL-terminated for the call; a null buffer of size 0 asks
    // only whether the attribute is there.
    let size = unsafe {
        libc::fgetxattr(
            again.as_raw_fd(),
            c"system.posix_acl_access".as_ptr(),
            std::ptr::null_mut(),
            0,
        )
    };
    if size >= 0 {
        return Ok(true);
    }
    let e = io::Error::last_os_error();
    match e.raw_os_error() {
        Some(libc::ENODATA | libc::EOPNOTSUPP) => Ok(false),
        _ => Err(e),
    }
}

#[cfg(test)]
#[path = "log_dir_tests.rs"]
mod log_dir_tests;
