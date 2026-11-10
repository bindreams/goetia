//! Creating and owning the parent directories `logs` and `cwd` need, which a unit fails at start without.

use nix::unistd::Uid;

use crate::backend::created_dirs::{CreatedDirs, Owner};
use crate::error::{Error, Result};
use crate::spec::{AccountId, DaemonSpec, User};

/// Create the parent of `logs` and the `cwd` directory itself, and hand what was created to a
/// non-root account. Runs while still elevated. The account is resolved first, so an unknown one
/// creates nothing.
pub(super) fn ensure_parent_dirs(spec: &DaemonSpec, created: &mut CreatedDirs) -> Result<()> {
    let owner = owner_of(&spec.user)?;
    if let Some(logs) = &spec.logs
        && let Some(parent) = logs.parent()
        && !parent.as_os_str().is_empty()
    {
        created.create(parent, owner)?;
    }
    if let Some(cwd) = &spec.cwd {
        created.create(cwd, owner)?;
    }
    created.hand_over()
}

/// Who to hand a created directory to so a non-root daemon can use it, or `None` for `User::Root`
/// (directories created by this — already-elevated — process are already root-owned). The owner
/// only, not the group: the daemon runs *as* this account, so owner-bit access suffices. A name is
/// looked up in the passwd database and, failing that, read as a number, as `chown` reads one.
pub(super) fn owner_of(user: &User) -> Result<Option<Owner>> {
    let uid = match user {
        User::Root => return Ok(None),
        User::Id(AccountId::Uid(uid)) => Uid::from_raw(*uid),
        User::Name(name) => {
            let found = nix::unistd::User::from_name(name).map_err(|e| Error::AccountLookup {
                detail: format!("look up user `{name}`"),
                source: Some(e.into()),
            })?;
            match found {
                Some(found) => found.uid,
                None => name.parse().map(Uid::from_raw).map_err(|_| Error::AccountLookup {
                    detail: format!("no such user `{name}` on this host"),
                    source: None,
                })?,
            }
        }
        // `identity_for` rejects a SID user before `install`/`preview_install` ever reaches
        // `ensure_parent_dirs`, so a validated `spec.user` reaching this arm is impossible — not a
        // silently-skipped case.
        User::Id(AccountId::Sid(_)) => unreachable!("a SID user must already have been rejected by identity_for"),
    };
    Owner::new(uid, None).map(Some)
}
