//! A local account a test creates and removes.

use super::{cleanup, cmd};

/// A local account, deleted on drop.
pub struct LocalUserGuard {
    name: String,
}

impl LocalUserGuard {
    pub fn create(name: &str, password: &str) -> Self {
        // Built after the add, not before: `/add` is atomic, so on failure nothing
        // was created and a guard would delete an account this test did not make.
        cmd::run("net.exe", &["user", name, password, "/add"]).expect_ok();
        Self { name: name.to_string() }
    }
}

impl Drop for LocalUserGuard {
    fn drop(&mut self) {
        // `net.exe` exits 2 for every error; the message names the Win32 one, and 2221 is
        // NERR_UserNotFound ("NET HELPMSG 2221"), the account already gone.
        cleanup::warn_left_behind(cmd::run_cleanup_unless(
            "net.exe",
            &[&["user", &self.name, "/delete"]],
            |run| run.stderr.contains("2221"),
        ));
    }
}
