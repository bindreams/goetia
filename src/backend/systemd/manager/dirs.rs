//! The parent directories `logs` and `cwd` need, which a unit fails at start without, made by
//! [`CreatedDirs`] and judged for the one thing systemd does as root with them.

use crate::backend::account::{Unlisted, resolve_account};
use std::path::Path;

use crate::backend::created_dirs::{CreatedDirs, Owner, log_in_handed_dir, refuse_writable_log_dir};
use crate::error::Result;
use crate::spec::{DaemonSpec, User};

/// Make the parent of `logs` and `cwd` where they are missing, refuse a log that root's `append:` open
/// could be led astray in, and hand `cwd` to a non-root account, all before this returns: the unit can
/// run as soon as the write that follows starts, and finds every directory as it is to be. systemd opens
/// the file before it drops privileges and follows a symlink, so a directory an account can write is a
/// way for it to make root append to any file. The log directory is therefore never handed over, a log
/// in or under `cwd` is refused (it is to be the account's), and so is an existing directory the
/// account can write, and an existing log that is a link or the account's.
///
/// Everything is judged first, on what [`CreatedDirs::plan`] read, and nothing is made until it all
/// passes. Runs while still elevated. The account is looked up only when `cwd` is to be made and handed
/// to it: an install that hands nothing over does not need it to exist yet (`sysusers` or a package may
/// make it later), and an unknown one then fails before anything is made. A uid with no passwd entry is
/// the number: systemd runs a unit as one, and its `cwd` is to be handed to it with the group left as it
/// is.
pub(super) fn ensure_parent_dirs(spec: &DaemonSpec, created: &mut CreatedDirs) -> Result<()> {
    let common = spec.common();
    let logs_parent = common
        .logs
        .as_deref()
        .and_then(Path::parent)
        .filter(|parent| !parent.as_os_str().is_empty());
    let log = logs_parent.map(|parent| created.plan(parent)).transpose()?;
    let cwd = common.cwd.as_deref().map(|cwd| created.plan(cwd)).transpose()?;

    if let (Some(logs), Some(log)) = (&common.logs, &log) {
        refuse_writable_log_dir(logs, log)?;
    }
    let owner = match &cwd {
        Some(cwd) if cwd.makes() => owner_of(&common.user)?,
        _ => None,
    };
    if let (Some(owner), Some(cwd_dir), Some(cwd), Some(log), Some(logs)) =
        (owner, &common.cwd, &cwd, &log, &common.logs)
        && log.is_at_or_under(cwd)
    {
        return Err(log_in_handed_dir(logs, cwd_dir, owner));
    }

    created.make(
        log.map(|log| (log, None))
            .into_iter()
            .chain(cwd.map(|cwd| (cwd, owner)))
            .collect(),
    )
}

/// Whom `cwd` is handed to for `user`. Root, by any spelling (uid 0 is root whatever names it), is
/// handed nothing: the directory is already root's.
fn owner_of(user: &User) -> Result<Option<Owner>> {
    if matches!(user, User::Root) {
        return Ok(None);
    }
    let account = resolve_account(user, Unlisted::AsNumber)?;
    if account.uid.is_root() {
        return Ok(None);
    }
    account.owner().map(Some)
}

#[cfg(test)]
#[path = "dirs_tests.rs"]
mod dirs_tests;
