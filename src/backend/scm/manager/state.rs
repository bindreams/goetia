//! Install-time provisioning of a `type: simple` service's state file
//! ([`crate::backend::scm::state_file`]), and its removal.
//!
//! The shim writes the file as the service account, and replaces it by rename, so the account
//! needs to create a file in the directory and to delete the one it replaces. The directory is
//! closed to everyone but `SYSTEM`, the Administrators and the accounts services run as; each
//! file is closed to the one account its service runs as. `LocalSystem` needs nothing beyond
//! `SYSTEM`.

use std::path::Path;

use super::identity;
use crate::backend::scm::state_file::{self, host};
use crate::error::{Error, Result};

const SYSTEM: &str = "*S-1-5-18";
const ADMINISTRATORS: &str = "*S-1-5-32-544";

fn path_argument(path: &Path) -> String {
    path.display().to_string()
}

/// The `icacls` invocations that fix the directory's ACL, in order. `account_sid` is the SID
/// string of the account the service runs as, or `None` for `LocalSystem`.
///
/// Grants come before the inheritance cut, and nothing is reset: the directory holds the grants of
/// every service, which are not this call's to take away.
pub(super) fn dir_commands(dir: &Path, account_sid: Option<&str>) -> Vec<Vec<String>> {
    let mut grants = vec![
        path_argument(dir),
        "/grant".to_string(),
        format!("{SYSTEM}:(OI)(CI)F"),
        "/grant".to_string(),
        format!("{ADMINISTRATORS}:(OI)(CI)F"),
    ];
    if let Some(sid) = account_sid {
        grants.push("/grant".to_string());
        grants.push(format!("*{sid}:(OI)(CI)M"));
    }
    vec![grants, vec![path_argument(dir), "/inheritance:r".to_string()]]
}

/// The `icacls` invocations that fix one service's file ACL, in order. Reset first, so an account
/// the service no longer runs as loses its entry; inheritance is cut last, so the service account
/// is never without access in between (the reset leaves it the directory's inherited grant).
pub(super) fn file_commands(file: &Path, account_sid: Option<&str>) -> Vec<Vec<String>> {
    let mut grants = vec![
        path_argument(file),
        "/grant".to_string(),
        format!("{SYSTEM}:F"),
        "/grant".to_string(),
        format!("{ADMINISTRATORS}:F"),
    ];
    if let Some(sid) = account_sid {
        grants.push("/grant".to_string());
        grants.push(format!("*{sid}:M"));
    }
    vec![
        vec![path_argument(file), "/reset".to_string()],
        grants,
        vec![path_argument(file), "/inheritance:r".to_string()],
    ]
}

fn run_icacls(args: &[String]) -> Result<()> {
    let output = std::process::Command::new("icacls.exe")
        .args(args)
        .output()
        .map_err(|e| Error::Other(format!("spawn `icacls.exe {}`: {e}", args.join(" "))))?;
    if output.status.success() {
        Ok(())
    } else {
        Err(Error::Other(format!(
            "`icacls.exe {}` failed ({:?}): {}{}",
            args.join(" "),
            output.status.code(),
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        )))
    }
}

/// Create the state directory and `id`'s file if absent, and fix their ACLs for the account the
/// service runs as (`None`: `LocalSystem`). An existing file keeps its content, so a re-install
/// does not forget a survivor or a pending restart. Idempotent.
pub(super) fn provision(id: &str, account: Option<&str>) -> Result<()> {
    let account_sid = account.map(identity::account_sid_string).transpose()?;
    let account_sid = account_sid.as_deref();

    let dir = host::dir();
    std::fs::create_dir_all(&dir).map_err(|e| Error::Other(format!("create {}: {e}", dir.display())))?;
    for command in dir_commands(&dir, account_sid) {
        run_icacls(&command)?;
    }

    let file = host::path(id);
    match std::fs::OpenOptions::new().write(true).create_new(true).open(&file) {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(e) => return Err(Error::Other(format!("create {}: {e}", file.display()))),
    }
    for command in file_commands(&file, account_sid) {
        run_icacls(&command)?;
    }
    Ok(())
}

/// Delete `id`'s state file (and a temporary left beside it). Absent is fine.
pub(super) fn remove(id: &str) -> Result<()> {
    let file = host::path(id);
    state_file::remove(&file).map_err(|e| Error::Other(format!("remove {}: {e}", file.display())))
}

#[cfg(test)]
#[path = "state_tests.rs"]
mod state_tests;
