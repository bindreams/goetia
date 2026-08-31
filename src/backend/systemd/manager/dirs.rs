//! Creating and owning the parent directories `logs` and `cwd` need — obligation 5. Only path
//! components a call actually creates are touched; an already-existing directory's mode and
//! ownership are left alone and merely verified writable, never reassigned.

use std::fs;
use std::io;
use std::os::unix::fs::PermissionsExt as _;
use std::path::Path;
use std::process::Command;

use crate::error::{Error, Result};
use crate::spec::{AccountId, DaemonSpec, User};

use super::io_err;

/// Create (and, for a non-root account, `chown`) the parent of `logs` and the `cwd` directory itself
/// — obligation 5. Runs while still elevated, at install time, so a later `start` never fails with an
/// opaque status over a directory the target account could not write.
pub(super) fn ensure_parent_dirs(spec: &DaemonSpec) -> Result<()> {
    if let Some(logs) = &spec.logs {
        if let Some(parent) = logs.parent() {
            if !parent.as_os_str().is_empty() {
                ensure_writable_dir(parent, &spec.user)?;
            }
        }
    }
    if let Some(cwd) = &spec.cwd {
        ensure_writable_dir(cwd, &spec.user)?;
    }
    Ok(())
}

/// Create `dir` (and any missing parents), `chmod`ing and `chown`ing only the path components this
/// call actually creates. `spec.cwd`/`logs` are user-supplied absolute paths with no restriction to a
/// Goetia-owned root, so reassigning mode/ownership unconditionally — including when `dir` or an
/// ancestor already existed — would let a manifest silently widen or reassign an arbitrary existing
/// system directory (`/var/log`, `/etc`, another user's home). A directory that already existed is
/// left exactly as it was; if the target account cannot write it, that is reported as an error
/// instead.
pub(super) fn ensure_writable_dir(dir: &Path, user: &User) -> Result<()> {
    let dir_preexisted = fs::symlink_metadata(dir).is_ok();

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
    // Outermost-first, so each `create_dir` always has an existing parent.
    for path in missing.into_iter().rev() {
        match fs::create_dir(path) {
            Ok(()) => {
                fs::set_permissions(path, fs::Permissions::from_mode(0o755)).map_err(|e| io_err("chmod", path, e))?;
                if let Some(owner) = &owner {
                    chown(path, owner)?;
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

    if dir_preexisted {
        verify_writable_by(dir, user)?;
    }
    Ok(())
}

/// Whether the account `user` names can write to the already-existing `dir`, checked by literally
/// asking the OS as that account (`runuser`) rather than hand-rolling permission-bit/ACL math.
/// `dir`'s mode/ownership is never touched here — see `ensure_writable_dir`'s doc comment for why —
/// so this exists purely to surface the same opaque-status-at-start failure obligation 5 exists to
/// avoid, at install time instead of discovered later by the daemon itself.
pub(super) fn verify_writable_by(dir: &Path, user: &User) -> Result<()> {
    let User::Root = user else {
        let account = chown_target(user).expect("a SID user is rejected by identity_for before this is reached");
        // `runuser -u` resolves its argument via `getpwnam` only — a bare numeric uid is rejected
        // outright even when a passwd entry exists for it (verified: `runuser -u 65534 -- true` fails
        // with "user 65534 does not exist or the user entry does not contain all the required
        // fields", while `runuser -u nobody -- true` succeeds for that same uid). Resolve a numeric
        // account to its name first so this never mistakes "runuser rejected the uid form" for "not
        // writable".
        let account = resolve_account_name(&account)?;
        let output = Command::new("runuser")
            .args(["-u", &account, "--", "test", "-w"])
            .arg(dir)
            .output()
            .map_err(|e| Error::Other(format!("failed to spawn runuser to verify {}: {e}", dir.display())))?;
        if !output.status.success() {
            return Err(Error::Other(format!(
                "{} already exists and is not writable by `{account}`; goetia only creates and owns \
                 directories it did not find already present, so fix its ownership/permissions \
                 manually, or point `cwd`/`logs` elsewhere",
                dir.display()
            )));
        }
        return Ok(());
    };
    Ok(())
}

/// Resolve a numeric uid to its account name; passes a non-numeric account straight through. A
/// lookup failure is reported explicitly rather than folded into a later "not writable" verdict — see
/// `verify_writable_by`'s call site.
fn resolve_account_name(account: &str) -> Result<String> {
    if !account.bytes().all(|b| b.is_ascii_digit()) {
        return Ok(account.to_string());
    }
    let output = Command::new("id")
        .args(["-nu", account])
        .output()
        .map_err(|e| Error::Other(format!("failed to spawn `id -nu {account}`: {e}")))?;
    if !output.status.success() {
        return Err(Error::Other(format!(
            "uid {account} has no passwd entry (`id -nu {account}` failed: {})",
            String::from_utf8_lossy(&output.stderr)
        )));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
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
