//! The check a log's directory needs where root opens the log: systemd's `append:`, and a launchd job
//! that runs as root. A job that runs as another user is opened as that user, and needs no check.
//!
//! systemd opens an `append:` log as root, before it drops to the daemon's account, and follows a
//! symlink there (measured on systemd 255); launchd opens `StandardOutPath` as the job's user, and
//! follows one the same way. So whoever can write the log's directory can plant a link at the log's
//! name before the first start, and whoever can write an ancestor of it can rename the directory away
//! and make one of their own. The directory is therefore refused while an account other than root can
//! change it, as it is when install runs. A symlink on the way is judged by the directory that holds
//! it, which the walk passed too: its owner cannot change where it leads. Where that directory is
//! sticky and a group or everyone can write it (`/tmp`), the owner of an entry can delete it and make
//! another, so a link there is refused unless root owns it.
//!
//! The group of a directory root owns is trusted above the log's own directory, as systemd's
//! `LogsDirectory=` trusts it: Debian and Ubuntu ship `/var/log` as `root:syslog 0775`, and
//! `/var/log/<app>/app.log` is accepted under it; macOS ships `/Library/Logs` as `root:admin 0775`.
//! The log's own directory is not trusted so, since a group member there can plant a link at the log's
//! name.
//!
//! goetia does not evaluate ACLs. A directory on the path that carries one, of any kind and whatever it
//! grants (see [`acl`]), is refused: an ACL can let an account write a directory whose mode says nobody
//! can, and one that grants nobody anything is refused too. The owner and the writers the mode names are
//! judged first, so that a directory refused for them is not refused for its ACL, which stripping would
//! not mend.

use std::os::fd::AsFd as _;
use std::path::Path;

use nix::sys::stat::SFlag;
use nix::unistd::{Gid, Group};

mod acl;

use super::walk::{End, Entry, Step, entry};
use super::{CreatedDirs, Plan, roots_text};
use crate::backend::account::describe_uid;
use crate::error::{Error, Result};

/// Refuse a log whose directory, or an ancestor, a user other than root can change or that carries an ACL, or
/// whose existing entry is a symlink or another user's file (see the module doc), before anything is
/// made. Sticky and the group of a root-owned directory excuse ancestors only. The error names the
/// writer, and where the log's own directory is refused but a directory made in it would be accepted, and
/// `created` would make one there (it is under one of its roots), suggests a directory of its own named
/// for `app`, the daemon's id.
///
/// Whether the log's directory is under a root is asked here only when the suggestion depends on it, of a
/// directory that exists (`plan` asks it of one that does not). A root that cannot be read leaves the
/// refusal with the advice that does not depend on the roots.
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
        // The writer is judged first, so that a directory an account owns or can write is refused for that,
        // and nobody is told to strip an ACL that is not the problem.
        let Some(Writer { who, ancestor_passes }) = writer(step, leaf) else {
            refuse_acl(step, dir)?;
            continue;
        };
        let kind = if step.is_link() { "symlink" } else { "directory" };
        let roots = plan.found.under_root(&created.roots);
        let advice = match logs.file_name() {
            Some(file) if leaf && ancestor_passes && roots.holds => {
                format!(
                    "use a directory of its own, such as {}, or one that only root can change",
                    dir.join(app).join(file).display()
                )
            }
            _ => format!(
                "use a log directory that only root can change, under directories only root can change (a missing \
                 one is made under {}), or {WITHOUT_THE_LOG}",
                roots_text(&roots)
            ),
        };
        return Err(Error::DirectoryRefused {
            path: dir.to_path_buf(),
            why: format!(
                "the {kind} {} can be changed by {who}, who could make root's open of the log ({ROOT_OPENS}) write any file; {advice}",
                step.at.display()
            ),
        });
    }
    // A directory only root can change can still hold an account-owned entry, or a link the open follows.
    if let (End::Whole { dir: held, .. }, Some(name)) = (&plan.found.end, logs.file_name()) {
        let why = match entry(held.as_fd(), name).map_err(|e| Error::os_at("stat", logs, e))? {
            Some(Entry::Link(..)) => format!(
                "{} is a symlink, and root's open of the log ({ROOT_OPENS}) \
                follows it to wherever it leads; remove it, or use a log that is not a link",
                logs.display()
            ),
            Some(found) if found.uid() != 0 => format!(
                "{} is owned by {}, not root, and root's open of the log ({ROOT_OPENS}) \
                would append to it; use a log file that root owns",
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

/// Refuse `step` if it carries an ACL, or if whether it does could not be read.
fn refuse_acl(step: &Step, dir: &Path) -> Result<()> {
    let why = match &step.dir {
        None => return Ok(()),
        Some(held) => match acl::has_acl(held.as_fd()) {
            Ok(false) => return Ok(()),
            Ok(true) => format!(
                "{} carries an ACL, which goetia does not evaluate; a log root opens must have none on its path",
                step.at.display()
            ),
            Err(e) => format!(
                "whether {} carries an ACL, which goetia does not evaluate, is not known as it could not be read ({e})",
                step.at.display()
            ),
        },
    };
    Err(Error::DirectoryRefused {
        path: dir.to_path_buf(),
        why: format!("{why}; use a log directory with no ACL on its path, or {WITHOUT_THE_LOG}"),
    })
}

/// Who opens the log as root, in the words of a refusal.
#[cfg(target_os = "linux")]
const ROOT_OPENS: &str = "systemd opens it before it drops privileges, and follows a symlink";
#[cfg(target_os = "macos")]
const ROOT_OPENS: &str = "launchd opens it as the job's user, root, and follows a symlink";

/// What a daemon can do instead of a refused log.
#[cfg(target_os = "linux")]
const WITHOUT_THE_LOG: &str = "drop `logs:` to log to the journal";
#[cfg(target_os = "macos")]
const WITHOUT_THE_LOG: &str = "drop `logs:`";

/// Who can change what a step passed, and whether the same directory would be accepted as an ancestor.
struct Writer {
    who: String,
    ancestor_passes: bool,
}

/// Who besides root can change what `step` passed, if anyone: the owner of a directory, or the group or
/// everyone who can write it. Unless `leaf`, the directory the log lands in, the sticky bit excuses
/// everyone's writing and the group of a root-owned directory is trusted. A symlink has no one but its
/// owner where its holder is sticky and others can write it; otherwise whoever can write its holder is
/// named at the holder's own step. The mode alone is judged; the caller refuses an ACL after.
fn writer(step: &Step, leaf: bool) -> Option<Writer> {
    let named = |who: String| {
        Some(Writer {
            who,
            ancestor_passes: false,
        })
    };
    if step.is_link() {
        return step.owner_who_can_replace().and_then(|uid| {
            named(format!(
                "its owner {}, who can replace it in the sticky directory that holds it",
                describe_uid(uid)
            ))
        });
    }
    if step.stat.st_uid != 0 {
        return named(format!("its owner {}", describe_uid(step.stat.st_uid)));
    }
    let mode = step.stat.st_mode as libc::mode_t;
    let directory = SFlag::from_bits_truncate(step.stat.st_mode) & SFlag::S_IFMT == SFlag::S_IFDIR;
    let sticky = mode & libc::S_ISVTX != 0;
    if !directory || (sticky && !leaf) {
        return None;
    }
    // The sticky bit excuses an ancestor, not the log's own directory.
    let found = |who: String, ancestor_passes: bool| Some(Writer { who, ancestor_passes });
    if mode & libc::S_IWOTH != 0 {
        return found("every user, which can write it".to_string(), sticky);
    }
    if mode & libc::S_IWGRP == 0 {
        return None;
    }
    leaf.then(|| Writer {
        who: match Group::from_gid(Gid::from_raw(step.stat.st_gid)) {
            Ok(Some(group)) => format!(
                "the members of group `{}` (gid {}), which can write it",
                group.name, step.stat.st_gid
            ),
            _ => format!("the members of gid {}, which can write it", step.stat.st_gid),
        },
        ancestor_passes: true,
    })
}

#[cfg(test)]
#[path = "log_dir_tests.rs"]
mod log_dir_tests;
