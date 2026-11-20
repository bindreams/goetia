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

    /// A local account with a random name (Windows caps them at 20 characters) and [`Self::PASSWORD`].
    pub fn create_random() -> Self {
        Self::create(&format!("gt{:08x}", rand::random::<u32>()), Self::PASSWORD)
    }

    /// The password of the accounts tests make: at most 14 characters, since a longer one makes
    /// `net user ... /add` ask about pre-Windows-2000 compatibility and wait on stdin.
    pub const PASSWORD: &'static str = "Goetia!Test12";

    pub fn name(&self) -> &str {
        &self.name
    }

    fn sid(&self) -> Vec<u8> {
        goetia::testing::sid_for_account_name(&self.name)
            .unwrap_or_else(|e| panic!("resolve the SID of `{}`: {}", self.name, e.report()))
    }

    /// Whether the account holds the logon right `right`, such as `SeBatchLogonRight`.
    pub fn holds(&self, right: &str) -> bool {
        goetia::testing::account_holds_right(&self.sid(), right)
    }

    /// Takes `right` away from the account.
    pub fn revoke(&self, right: &str) {
        goetia::testing::revoke_right(&self.sid(), right);
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
