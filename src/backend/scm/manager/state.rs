//! Install-time provisioning of a `type: simple` service's state ([`crate::backend::scm::state_file`]),
//! and its release.
//!
//! Each service has a directory of its own under the state directory, holding its record, the
//! temporary files a write goes through and its lock. The shim replaces the record by renaming a
//! temporary over it, and a rename keeps the descriptor of the file it moves, so the ACL of
//! everything the account creates in the directory is the directory's, inherited. That ACL is
//! `SYSTEM`, the Administrators and the one account the service runs as; the state directory
//! itself is closed to every service account, so none can create, replace or read another
//! service's directory. `LocalSystem` needs nothing beyond `SYSTEM`.

use std::path::Path;

use super::identity;
use crate::backend::scm::state_file::{self, host};
use crate::error::{Error, Result};

const SYSTEM: &str = "*S-1-5-18";
const ADMINISTRATORS: &str = "*S-1-5-32-544";

fn path_argument(path: &Path) -> String {
    path.display().to_string()
}

/// The `icacls` invocations that close the state directory to everyone but `SYSTEM` and the
/// Administrators, in order. Nothing is reset: the directory holds every service's directory,
/// which are not this call's to touch.
pub(super) fn dir_commands(dir: &Path) -> Vec<Vec<String>> {
    vec![
        vec![
            path_argument(dir),
            "/grant".to_string(),
            format!("{SYSTEM}:(OI)(CI)F"),
            "/grant".to_string(),
            format!("{ADMINISTRATORS}:(OI)(CI)F"),
        ],
        vec![path_argument(dir), "/inheritance:r".to_string()],
    ]
}

/// The `icacls` invocations that fix one service's directory, in order. The reset comes first, so an
/// account the service no longer runs as loses its entry, in the directory and in what is in it;
/// what it leaves is the state directory's `SYSTEM` and Administrators grants, inherited.
/// `account_sid` is the SID string of the account the service runs as, or `None` for `LocalSystem`.
pub(super) fn service_dir_commands(dir: &Path, account_sid: Option<&str>) -> Vec<Vec<String>> {
    let mut commands = vec![vec![path_argument(dir), "/reset".to_string(), "/T".to_string()]];
    if let Some(sid) = account_sid {
        commands.push(vec![
            path_argument(dir),
            "/grant".to_string(),
            format!("*{sid}:(OI)(CI)M"),
        ]);
    }
    commands
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

/// Create the state directory and `id`'s directory in it if absent, fix both ACLs for the account
/// the service runs as (`None`: `LocalSystem`), and make sure the record exists. An existing
/// record keeps its content, so a re-install does not forget a survivor or a pending restart.
/// Idempotent.
pub(super) fn provision(id: &str, account: Option<&str>) -> Result<()> {
    let account_sid = account.map(identity::account_sid_string).transpose()?;
    let account_sid = account_sid.as_deref();

    let dir = host::dir();
    std::fs::create_dir_all(&dir).map_err(|e| Error::Other(format!("create {}: {e}", dir.display())))?;
    for command in dir_commands(&dir) {
        run_icacls(&command)?;
    }

    let service_dir = host::service_dir(id);
    std::fs::create_dir_all(&service_dir)
        .map_err(|e| Error::Other(format!("create {}: {e}", service_dir.display())))?;
    for command in service_dir_commands(&service_dir, account_sid) {
        run_icacls(&command)?;
    }

    // After the ACLs, so what is created inherits them.
    let file = host::path(id);
    match std::fs::OpenOptions::new().write(true).create_new(true).open(&file) {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(e) => return Err(Error::Other(format!("create {}: {e}", file.display()))),
    }
    Ok(())
}

/// Release `id`'s state when its service goes away (or stops having a shim): delete it, and the
/// temporaries left beside it, unless it names a daemon that is still running. That one stays, with
/// only its failure dropped, so the same id registered again cannot start a second daemon beside
/// it; a later [`provision`] reuses it. Absent is fine.
pub(super) fn release(id: &str) -> Result<()> {
    let file = host::path(id);
    state_file::release(&file).map_err(|e| Error::Other(format!("release {}: {e}", file.display())))
}

#[cfg(test)]
#[path = "state_tests.rs"]
mod state_tests;
