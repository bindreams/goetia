//! The check systemd's `append:` needs of a log's directory.
//!
//! systemd opens an `append:` log as root, before it drops to the daemon's account, and follows a
//! symlink there (measured on systemd 255). So whoever can write the log's directory, or an ancestor
//! of it, can plant a link at the log's name and make root append to any file. The directory is
//! therefore refused while an account other than root can change it, however it came to be so.

use std::path::Path;

use nix::sys::stat::SFlag;

use super::walk::{End, Step, walk};
use crate::error::{Error, Result};

/// Refuse a log at `logs` whose directory, or any directory or symlink on the way to it, a user other
/// than root can change: owned by another, or writable by a group or by everyone without the sticky bit
/// that keeps entries from being renamed by others. Names the writer.
pub(crate) fn refuse_writable_log_dir(logs: &Path) -> Result<()> {
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
    match walked
        .trail
        .iter()
        .find_map(|step| writer(step).map(|writer| (step, writer)))
    {
        None => Ok(()),
        Some((step, who)) => Err(Error::DirectoryRefused {
            path: dir.to_path_buf(),
            why: format!(
                "{} {} can be changed by {who}, who could make root's open of the log (systemd opens it                  before it drops privileges, and follows a symlink) write any file; use a log directory                  that only root can change, under directories only root can change",
                if step.link { "the symlink" } else { "the directory" },
                step.at.display(),
            ),
        }),
    }
}

/// Who besides root can change what `step` passed, if anyone: its owner, or the group or everyone
/// who can write a directory whose entries others may rename.
fn writer(step: &Step) -> Option<String> {
    if step.stat.st_uid != 0 {
        return Some(
            match nix::unistd::User::from_uid(nix::unistd::Uid::from_raw(step.stat.st_uid)) {
                Ok(Some(user)) => format!("its owner `{}` (uid {})", user.name, step.stat.st_uid),
                _ => format!("its owner, uid {}", step.stat.st_uid),
            },
        );
    }
    let mode = step.stat.st_mode as libc::mode_t;
    let directory = SFlag::from_bits_truncate(step.stat.st_mode) & SFlag::S_IFMT == SFlag::S_IFDIR;
    if !directory || mode & libc::S_ISVTX != 0 {
        return None;
    }
    if mode & libc::S_IWOTH != 0 {
        Some("every user, which can write it".to_string())
    } else if mode & libc::S_IWGRP != 0 {
        Some(
            match nix::unistd::Group::from_gid(nix::unistd::Gid::from_raw(step.stat.st_gid)) {
                Ok(Some(group)) => format!(
                    "the members of group `{}` (gid {}), which can write it",
                    group.name, step.stat.st_gid
                ),
                _ => format!("the members of gid {}, which can write it", step.stat.st_gid),
            },
        )
    } else {
        None
    }
}

#[cfg(test)]
#[path = "log_dir_tests.rs"]
mod log_dir_tests;
