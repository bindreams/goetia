//! The parent directories `logs` and `cwd` need, which a unit fails at start without, made by
//! [`CreatedDirs`] and judged for the one thing systemd does as root with them.

use nix::unistd::Uid;

use crate::backend::created_dirs::{CreatedDirs, Owner, refuse_writable_log_dir};
use crate::error::{Error, Result};
use crate::spec::{AccountId, DaemonSpec, User};

/// Make the parent of `logs` and `cwd` where they are missing, hand `cwd` to a non-root account, and
/// refuse a log that root's `append:` open could be led astray in: systemd opens the file before it
/// drops privileges and follows a symlink, so a directory an account can write is a way for it to
/// make root append to any file. The log directory is therefore never handed over, and an existing
/// one the account can write is refused too. Runs while still elevated; the account is resolved
/// first, so an unknown one creates nothing.
pub(super) fn ensure_parent_dirs(spec: &DaemonSpec, created: &mut CreatedDirs) -> Result<()> {
    let common = spec.common();
    let owner = owner_of(&common.user)?;
    let logs_parent = common.logs.as_deref().and_then(|logs| logs.parent());
    if let Some(parent) = logs_parent
        && !parent.as_os_str().is_empty()
    {
        created.create(parent, None)?;
    }
    if let Some(cwd) = &common.cwd {
        created.create(cwd, owner)?;
    }
    created.hand_over()?;
    match &common.logs {
        Some(logs) => refuse_writable_log_dir(logs, common.id.as_str()),
        None => Ok(()),
    }
}

/// Who `cwd` is handed to so a non-root daemon can work in it, or `None` for `User::Root`: what this
/// already-elevated process makes is root's. A name is looked up in the passwd database and, failing
/// that, read as a number, as `chown` reads one. The group is the account's own where it has an entry.
pub(super) fn owner_of(user: &User) -> Result<Option<Owner>> {
    let lookup = |what: String, e: nix::Error| Error::AccountLookup {
        detail: what,
        source: Some(e.into()),
    };
    let found = match user {
        User::Root => return Ok(None),
        User::Id(AccountId::Uid(uid)) => nix::unistd::User::from_uid(Uid::from_raw(*uid))
            .map_err(|e| lookup(format!("look up uid {uid}"), e))?
            .map(|found| (found.uid, Some(found.gid)))
            .unwrap_or((Uid::from_raw(*uid), None)),
        User::Name(name) => {
            match nix::unistd::User::from_name(name).map_err(|e| lookup(format!("look up user `{name}`"), e))? {
                Some(found) => (found.uid, Some(found.gid)),
                None => (
                    name.parse().map(Uid::from_raw).map_err(|_| Error::AccountLookup {
                        detail: format!("no such user `{name}` on this host"),
                        source: None,
                    })?,
                    None,
                ),
            }
        }
        // `identity_for` rejects a SID user before `install`/`preview_install` ever reaches
        // `ensure_parent_dirs`, so a validated `spec.user` reaching this arm is impossible — not a
        // silently-skipped case.
        User::Id(AccountId::Sid(_)) => unreachable!("a SID user must already have been rejected by identity_for"),
    };
    Owner::new(found.0, found.1).map(Some)
}
