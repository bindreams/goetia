//! Creating and owning the parent directories `logs` and `cwd` need, which a unit fails at start without. Only path
//! components a call actually creates are touched; an already-existing directory's mode and
//! ownership are left alone.

use std::fs;

use nix::unistd::Uid;

use crate::backend::created_dirs::{CreatedDirs, Owner};
use crate::error::{Error, Result};
use crate::spec::{AccountId, DaemonSpec, User};

/// Create (and, for a non-root account, hand over) the parent of `logs` and the `cwd` directory
/// itself. Runs while still elevated.
pub(super) fn ensure_parent_dirs(spec: &DaemonSpec, created: &mut CreatedDirs) -> Result<()> {
    if let Some(logs) = &spec.logs
        && let Some(parent) = logs.parent()
        && !parent.as_os_str().is_empty()
    {
        ensure_dir(parent, &spec.user, created)?;
    }
    if let Some(cwd) = &spec.cwd {
        ensure_dir(cwd, &spec.user, created)?;
    }
    Ok(())
}

/// Create `dir` (and any missing parents), `chmod`ing and `chown`ing only the path components this
/// call actually creates. `spec.cwd`/`logs` are user-supplied absolute paths with no restriction to a
/// Goetia-owned root, so reassigning mode/ownership unconditionally — including when `dir` or an
/// ancestor already existed — would let a manifest silently widen or reassign an arbitrary existing
/// system directory (`/var/log`, `/etc`, another user's home). A directory that already existed (or
/// that a concurrent, racing install created first) is left exactly as it was; only a non-directory
/// at that path is an error.
///
/// The account is resolved before anything is created, so an unknown one creates nothing.
pub(super) fn ensure_dir(dir: &std::path::Path, user: &User, created: &mut CreatedDirs) -> Result<()> {
    created.create(dir, owner_of(user)?)?;
    let meta = fs::metadata(dir).map_err(|e| Error::os_at("stat", dir, e))?;
    if !meta.is_dir() {
        return Err(Error::NotADirectory {
            path: dir.to_path_buf(),
        });
    }
    Ok(())
}

/// Who to hand a created directory to so a non-root daemon can use it, or `None` for `User::Root`
/// (directories created by this — already-elevated — process are already root-owned). The owner
/// only, not the group: the daemon runs *as* this account, so owner-bit access suffices. A name is
/// looked up in the passwd database and, failing that, read as a number, as `chown` reads one.
fn owner_of(user: &User) -> Result<Option<Owner>> {
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
    Ok(Some(Owner { uid, gid: None }))
}
