//! The check systemd's `append:` needs of a log's directory.
//!
//! systemd opens an `append:` log as root, before it drops to the daemon's account, and follows a
//! symlink there (measured on systemd 255). So whoever can write the log's directory can plant a link
//! at the log's name before the first start, and whoever can write an ancestor of it can rename the
//! directory away and make one of their own. The directory is therefore refused while an account other
//! than root can change it, however it came to be so.

use std::path::Path;

use nix::sys::stat::SFlag;
use nix::unistd::{Gid, Group, Uid, User};

use super::PendingHandOver;
use super::walk::{End, Step, walk};
use crate::error::{Error, Result};

/// Refuse a log at `logs` whose directory, or any directory or symlink on the way to it, a user other
/// than root can change: owned by another, or writable by a group or by everyone. An ancestor that
/// everyone can write is accepted when it is sticky, which keeps others from renaming root's entries;
/// the log's own directory is not, since the sticky bit does not keep them from making an entry in it.
/// `handing` are the directories about to be handed to an account, which are its own from then on: a
/// log in one of them, or under it, is refused. Names the writer.
pub(crate) fn refuse_writable_log_dir(logs: &Path, handing: &[PendingHandOver]) -> Result<()> {
    let Some(dir) = logs.parent().filter(|dir| !dir.as_os_str().is_empty()) else {
        return Ok(());
    };
    let walked = walk(dir, &[])?;
    if !matches!(walked.end, End::Whole(_)) {
        return Err(Error::os_at(
            "stat",
            dir,
            std::io::Error::from(std::io::ErrorKind::NotFound),
        ));
    }
    let last = walked.trail.len() - 1;
    for (n, step) in walked.trail.iter().enumerate() {
        let kind = if step.link { "the symlink" } else { "the directory" };
        let handed = handing
            .iter()
            .find(|pending| !step.link && pending.dev == step.stat.st_dev && pending.ino == step.stat.st_ino);
        let reason = match (handed, writer(step, n == last)) {
            (Some(pending), _) => format!(
                "{kind} {} will belong to {}, to whom this install hands {} once it has made every directory, \
                 who could",
                step.at.display(),
                account_text(pending.owner.uid.as_raw()),
                pending.path.display(),
            ),
            (None, Some(who)) => format!("{kind} {} can be changed by {who}, who could", step.at.display()),
            (None, None) => continue,
        };
        return Err(Error::DirectoryRefused {
            path: dir.to_path_buf(),
            why: format!(
                "{reason} make root's open of the log (systemd opens it before it drops privileges, and follows \
                 a symlink) write any file; use a log directory that only root can change, under directories \
                 only root can change"
            ),
        });
    }
    Ok(())
}

/// Who besides root can change what `step` passed, if anyone: its owner, or the group or everyone
/// who can write a directory. The sticky bit excuses that unless `leaf`, the log's own directory.
fn writer(step: &Step, leaf: bool) -> Option<String> {
    if step.stat.st_uid != 0 {
        return Some(format!("its owner {}", account_text(step.stat.st_uid)));
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

/// An account as an error names it: its name where it has an entry, and always its uid.
fn account_text(uid: u32) -> String {
    match User::from_uid(Uid::from_raw(uid)) {
        Ok(Some(user)) => format!("`{}` (uid {uid})", user.name),
        _ => format!("uid {uid}"),
    }
}

#[cfg(test)]
#[path = "log_dir_tests.rs"]
mod log_dir_tests;
