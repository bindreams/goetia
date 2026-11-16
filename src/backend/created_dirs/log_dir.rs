//! The check systemd's `append:` needs of a log's directory.
//!
//! systemd opens an `append:` log as root, before it drops to the daemon's account, and follows a
//! symlink there (measured on systemd 255). So whoever can write the log's directory can plant a link
//! at the log's name before the first start, and whoever can write an ancestor of it can rename the
//! directory away and make one of their own. The directory is therefore refused while an account other
//! than root can change it, as it is when install runs.

use std::path::Path;

use std::os::fd::AsFd as _;

use nix::sys::stat::SFlag;
use nix::unistd::{Gid, Group};

use super::Plan;
use super::walk::{End, Entry, Step, entry};
use crate::backend::account::describe_uid;
use crate::error::{Error, Result};

/// Refuse a log whose directory, or an ancestor, a user other than root can change, or whose existing
/// entry is a symlink or another user's file (see the module doc), before anything is made. Sticky
/// excuses ancestors only. The error names the writer.
pub(crate) fn refuse_writable_log_dir(logs: &Path, plan: &Plan) -> Result<()> {
    let Some(dir) = logs.parent().filter(|dir| !dir.as_os_str().is_empty()) else {
        return Ok(());
    };
    for step in &plan.found.trail {
        // The log lands in the directory the path resolves to, whatever step the walk reached it by:
        // `.` and `..`, or a link to either, bring it back to one it has passed.
        let leaf = !step.link
            && matches!(&plan.found.end, End::Whole { stat, .. }
                if step.stat.st_dev == stat.st_dev && step.stat.st_ino == stat.st_ino);
        let Some(who) = writer(step, leaf) else { continue };
        let kind = if step.link { "the symlink" } else { "the directory" };
        return Err(Error::DirectoryRefused {
            path: dir.to_path_buf(),
            why: format!(
                "{kind} {} can be changed by {who}, who could {LED_ASTRAY}",
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
                          symlink) write any file; use a log directory that only root can change, under \
                          directories only root can change";

/// Who besides root can change what `step` passed, if anyone: its owner, or the group or everyone
/// who can write a directory. The sticky bit excuses that unless `leaf`, the directory the log lands in.
fn writer(step: &Step, leaf: bool) -> Option<String> {
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
    } else if mode & libc::S_IWGRP != 0 {
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
