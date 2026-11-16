//! The parent directories `logs` and `cwd` need, which a unit fails at start without, made by
//! [`CreatedDirs`] and judged for the one thing systemd does as root with them.

use crate::backend::account::{Unlisted, resolve_account};
use crate::backend::created_dirs::{CreatedDirs, Owner, refuse_writable_log_dir};
use crate::error::Result;
use crate::spec::{DaemonSpec, User};

/// Make the parent of `logs` and `cwd` where they are missing, refuse a log that root's `append:` open
/// could be led astray in, and then hand `cwd` to a non-root account. systemd opens the file before
/// it drops privileges and follows a symlink, so a directory an account can write is a way for it to
/// make root append to any file. The log directory is therefore never handed over, a log in or under
/// `cwd` is refused (the check reads `cwd` as the account's, which it is to be), and an existing
/// directory the account can write is refused too. The check comes before anything is handed over.
/// Runs while still elevated. The account is looked up only when `cwd` is made to be handed to it: an
/// install that hands nothing over does not need it to exist yet (`sysusers` or a package may make it
/// later), and an unknown one then fails before `cwd` is made. A uid with no passwd entry is the
/// number: systemd runs a unit as one, and its `cwd` is handed to it with the group left as it is.
pub(super) fn ensure_parent_dirs(spec: &DaemonSpec, created: &mut CreatedDirs) -> Result<()> {
    let common = spec.common();
    let logs_parent = common.logs.as_deref().and_then(|logs| logs.parent());
    if let Some(parent) = logs_parent
        && !parent.as_os_str().is_empty()
    {
        created.create(parent, None)?;
    }
    if let Some(cwd) = &common.cwd {
        created.create_for(cwd, || owner_of(&common.user))?;
    }
    if let Some(logs) = &common.logs
        && let Err(refused) = refuse_writable_log_dir(logs, &created.to_hand_over())
    {
        // The directories stay for the guard to remove or keep, and a keep must not hand over the one
        // the check refused: the log is in it.
        created.withhold_hand_over();
        return Err(refused);
    }
    created.hand_over()
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
