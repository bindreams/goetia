//! The parent directories `logs` and `cwd` need, which a unit fails at start without, made by
//! [`CreatedDirs`] and judged for the one thing systemd does as root with them.

use crate::backend::account::{Unlisted, resolve_account};
use crate::backend::created_dirs::{CreatedDirs, refuse_writable_log_dir};
use crate::error::Result;
use crate::spec::{DaemonSpec, User};

/// Make the parent of `logs` and `cwd` where they are missing, refuse a log that root's `append:` open
/// could be led astray in, and then hand `cwd` to a non-root account. systemd opens the file before
/// it drops privileges and follows a symlink, so a directory an account can write is a way for it to
/// make root append to any file. The log directory is therefore never handed over, a log in or under
/// `cwd` is refused (the check reads `cwd` as the account's, which it is to be), and an existing
/// directory the account can write is refused too. The check comes before anything is handed over.
/// Runs while still elevated; the account is resolved first, so an unknown one creates nothing. A uid
/// with no passwd entry is the number: systemd runs a unit as one, and its `cwd` is handed to it with
/// the group left as it is.
pub(super) fn ensure_parent_dirs(spec: &DaemonSpec, created: &mut CreatedDirs) -> Result<()> {
    let common = spec.common();
    let owner = match &common.user {
        User::Root => None,
        user => Some(resolve_account(user, Unlisted::AsNumber)?.owner()?),
    };
    let logs_parent = common.logs.as_deref().and_then(|logs| logs.parent());
    if let Some(parent) = logs_parent
        && !parent.as_os_str().is_empty()
    {
        created.create(parent, None)?;
    }
    if let Some(cwd) = &common.cwd {
        created.create(cwd, owner)?;
    }
    if let Some(logs) = &common.logs {
        refuse_writable_log_dir(logs, &created.to_hand_over())?;
    }
    created.hand_over()
}
