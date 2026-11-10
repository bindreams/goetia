//! A copy of the test binary that service accounts can run.

use std::path::PathBuf;

use super::{cleanup, cmd, program_data};

/// A copy of the current test executable, placed under `%ProgramData%` with
/// an explicit grant of read+execute to `Everyone` (by well-known SID
/// `S-1-1-0`, not by localized name), removed again on drop.
///
/// `LocalService` cannot be assumed to have execute access to the repo's
/// own `target/` tree — CI checks that out under an ACL scoped to the
/// runner's own (interactive/build) account, not to every built-in service
/// identity. A plain copy is not enough on its own either: it inherits
/// `%ProgramData%`'s own ACL, which does not grant `Everyone` execute
/// either, hence the explicit `icacls` grant.
pub struct WorldReadableExe {
    path: PathBuf,
    _remove: cleanup::FileGuard,
}

impl WorldReadableExe {
    pub fn new(id: &str) -> Self {
        let path = program_data().join(format!("{id}.exe"));
        let guard = Self {
            _remove: cleanup::FileGuard::new(path.clone()),
            path,
        };
        let exe = std::env::current_exe().expect("locate the test binary");
        std::fs::copy(&exe, &guard.path)
            .unwrap_or_else(|e| panic!("copy {} to {}: {e}", exe.display(), guard.path.display()));

        let path_str = guard.path_str();
        cmd::run("icacls", &[&path_str, "/grant", "*S-1-1-0:(RX)"]).expect_ok();

        guard
    }

    pub fn path_str(&self) -> String {
        self.path
            .to_str()
            .unwrap_or_else(|| panic!("{} is not UTF-8", self.path.display()))
            .to_string()
    }
}
