//! Creating and owning the parent directories `logs` and `cwd` need — obligation 5. Only path
//! components a call actually creates are touched; an already-existing directory's mode and
//! ownership are left alone.

use std::fs;
use std::io;
use std::os::unix::fs::{DirBuilderExt as _, PermissionsExt as _};
use std::path::Path;
use std::process::Command;

use crate::error::{Error, Result};
use crate::spec::{AccountId, DaemonSpec, User};

use super::io_err;

/// Create (and, for a non-root account, `chown`) the parent of `logs` and the `cwd` directory itself
/// — obligation 5. Runs while still elevated.
pub(super) fn ensure_parent_dirs(spec: &DaemonSpec) -> Result<()> {
    if let Some(logs) = &spec.logs {
        if let Some(parent) = logs.parent() {
            if !parent.as_os_str().is_empty() {
                ensure_dir(parent, &spec.user)?;
            }
        }
    }
    if let Some(cwd) = &spec.cwd {
        ensure_dir(cwd, &spec.user)?;
    }
    Ok(())
}

/// Create `dir` (and any missing parents), `chmod`ing and `chown`ing only the path components this
/// call actually creates. `spec.cwd`/`logs` are user-supplied absolute paths with no restriction to a
/// Goetia-owned root, so reassigning mode/ownership unconditionally — including when `dir` or an
/// ancestor already existed — would let a manifest silently widen or reassign an arbitrary existing
/// system directory (`/var/log`, `/etc`, another user's home). A directory that already existed (or
/// that a concurrent, racing install created first — see the loop below) is left exactly as it was;
/// only a non-directory at that path is an error.
pub(super) fn ensure_dir(dir: &Path, user: &User) -> Result<()> {
    let mut missing = Vec::new();
    let mut cursor = dir;
    loop {
        match fs::symlink_metadata(cursor) {
            Ok(_) => break,
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                missing.push(cursor);
                match cursor.parent() {
                    Some(parent) if !parent.as_os_str().is_empty() => cursor = parent,
                    _ => break,
                }
            }
            Err(e) => return Err(io_err("stat", cursor, e)),
        }
    }

    let owner = chown_target(user);
    // Whether *this call* is the one that actually created `dir` itself (as opposed to every
    // ancestor above it, or `dir` already existing, or a concurrent racing install creating it
    // first — see the `AlreadyExists` arm below). Only then do the post-loop checks get skipped:
    // anything else means this call did not create `dir`, which is exactly the condition the
    // do-not-touch-an-existing-directory rule above is about.
    let mut created_dir = false;
    // Outermost-first, so each `create_dir` always has an existing parent.
    for path in missing.into_iter().rev() {
        // The mode is set atomically with creation (`DirBuilder`, not a separate `set_permissions`
        // after a default `mkdir`) so the directory is never briefly `0777`-minus-umask — under a
        // permissive umask (e.g. `000`, common in some CI/root contexts) a plain `create_dir` +
        // later `chmod` would leave it world-writable for the duration in between, letting any local
        // user plant a file or symlink in what is about to become a daemon's `cwd`/log directory.
        // `mkdir`'s mode argument is itself still umask-masked, so the explicit `chmod` after
        // creation remains necessary to guarantee exactly 0755 regardless of umask — the ordering
        // change here only ever narrows the window, from "up to 0777" down to "at most 0755".
        match fs::DirBuilder::new().mode(0o755).create(path) {
            Ok(()) => {
                fs::set_permissions(path, fs::Permissions::from_mode(0o755)).map_err(|e| io_err("chmod", path, e))?;
                if let Some(owner) = &owner {
                    chown(path, owner)?;
                }
                if path == dir {
                    created_dir = true;
                }
            }
            // Two installs sharing a parent directory (e.g. two daemons both under `logs:
            // /var/log/goetia/*.log`) can both see this component missing and both race to create
            // it. The loser must not fail outright — nor may it chmod/chown a component it did not
            // itself create, per this function's own do-not-touch-existing-dirs rule above.
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(io_err("create directory", path, e)),
        }
    }

    if !created_dir {
        let meta = fs::metadata(dir).map_err(|e| io_err("stat", dir, e))?;
        if !meta.is_dir() {
            return Err(Error::Other(format!(
                "{} already exists and is not a directory; goetia cannot use it as a daemon's cwd \
                 or as its log directory",
                dir.display()
            )));
        }
    }
    Ok(())
}

fn chown(path: &Path, owner: &str) -> Result<()> {
    // A bare owner, deliberately with no trailing `:` — `chown owner: path` is what `coreutils`
    // documents for "also set the group to the owner's login group", but GNU coreutils rejects that
    // form outright for a *numeric* owner (verified: `chown 65534: /x` fails with "chown: invalid
    // spec: '65534:'", exit 1, even though uid 65534 has a passwd entry; `chown nobody: /x` and
    // `chown 65534 /x` both succeed). Since the daemon runs *as* this exact account, owner-bit access
    // already suffices without also reassigning the group.
    let output = Command::new("chown")
        .arg(owner)
        .arg(path)
        .output()
        .map_err(|e| Error::Other(format!("failed to spawn chown for {}: {e}", path.display())))?;
    if !output.status.success() {
        return Err(Error::Other(format!(
            "chown {owner} {} failed: {}",
            path.display(),
            String::from_utf8_lossy(&output.stderr)
        )));
    }
    Ok(())
}

/// The account name/uid to `chown` a directory to so a non-root daemon can use it, or `None` for
/// `User::Root` (directories created by this — already-elevated — process are already root-owned).
fn chown_target(user: &User) -> Option<String> {
    match user {
        User::Root => None,
        User::Name(name) => Some(name.clone()),
        User::Id(AccountId::Uid(uid)) => Some(uid.to_string()),
        // `identity_for` rejects a SID user before `install`/`preview_install` ever reaches
        // `ensure_parent_dirs`, so a validated `spec.user` reaching this arm is impossible — not a
        // silently-skipped case.
        User::Id(AccountId::Sid(_)) => unreachable!("a SID user must already have been rejected by identity_for"),
    }
}
