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
//! group member there can plant a link at the log's name.

use std::path::Path;

use std::os::fd::AsFd as _;

use nix::sys::stat::SFlag;
use nix::unistd::{Gid, Group};

use super::Plan;
use super::walk::{End, Entry, Step, entry};
use crate::backend::account::describe_uid;
use crate::error::{Error, Result};

/// Refuse a log whose directory, or an ancestor, a user other than root can change, or whose existing
/// entry is a symlink or another user's file (see the module doc), before anything is made. Sticky and
/// the group of a root-owned directory excuse ancestors only. The error names the writer, and where the
/// log's own directory is refused but would be an acceptable ancestor, suggests a directory of its own
/// named for `app`, the daemon's id.
pub(crate) fn refuse_writable_log_dir(logs: &Path, plan: &Plan, app: &str) -> Result<()> {
    let Some(dir) = logs.parent().filter(|dir| !dir.as_os_str().is_empty()) else {
        return Ok(());
    };
    for step in &plan.found.trail {
        // The log lands in the directory the path resolves to, whatever step the walk reached it by:
        // `.` and `..`, or a link to either, bring it back to one it has passed.
        let leaf = !step.is_link()
            && matches!(&plan.found.end, End::Whole { stat, .. }
                if step.stat.st_dev == stat.st_dev && step.stat.st_ino == stat.st_ino);
        let Some(who) = writer(step, leaf) else { continue };
        let kind = if step.is_link() { "symlink" } else { "directory" };
        let advice = match logs.file_name() {
            Some(file) if leaf && own_directory_would_stand(step) => format!(
                "use a directory of its own, such as {}, or one that only root can change",
                dir.join(app).join(file).display()
            ),
            _ => "use a log directory that only root can change, under directories only root can change".to_string(),
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

/// Whether a directory made inside `leaf`, which is refused as the log's own, would be an acceptable
/// ancestor: `leaf` is a directory root owns and everyone cannot write it, unless it is sticky.
fn own_directory_would_stand(leaf: &Step) -> bool {
    let mode = leaf.stat.st_mode as libc::mode_t;
    !leaf.is_link() && leaf.stat.st_uid == 0 && (mode & libc::S_ISVTX != 0 || mode & libc::S_IWOTH == 0)
}

/// Who besides root can change what `step` passed, if anyone: the owner of a directory, or the group or
/// everyone who can write it. Unless `leaf`, the directory the log lands in, the sticky bit excuses
/// everyone's writing and the group of a root-owned directory is trusted. A symlink has no one but its owner where its holder is sticky and others can write it; otherwise
/// whoever can write its holder is named at the holder's own step.
fn writer(step: &Step, leaf: bool) -> Option<String> {
    if step.is_link() {
        return step.owner_who_can_replace().map(|uid| {
            format!(
                "its owner {}, who can replace it in the sticky directory that holds it",
                describe_uid(uid)
            )
        });
    }
    if step.stat.st_uid != 0 {
        return Some(format!("its owner {}", describe_uid(step.stat.st_uid)));
    }
    let mode = step.stat.st_mode as libc::mode_t;
    let directory = SFlag::from_bits_truncate(step.stat.st_mode) & SFlag::S_IFMT == SFlag::S_IFDIR;
    if !directory || (mode & libc::S_ISVTX != 0 && !leaf) {
        return None;
    }
    if mode & libc::S_IWOTH != 0 {
        Some("every user, which can write it".to_string())
    } else if leaf && mode & libc::S_IWGRP != 0 {
        Some(match Group::from_gid(Gid::from_raw(step.stat.st_gid)) {
            Ok(Some(group)) => format!(
                "the members of group `{}` (gid {}), which can write it",
                group.name, step.stat.st_gid
            ),
            _ => format!("the members of gid {}, which can write it", step.stat.st_gid),
        })
    } else {
        None
    }
}

#[cfg(test)]
#[path = "log_dir_tests.rs"]
mod log_dir_tests;
