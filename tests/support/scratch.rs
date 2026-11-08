//! A key under `HKLM\SOFTWARE` for a test to create, removed on drop. Shared by the integration
//! suites (through `support`) and the library's unit tests (as `backend::test_scratch`, by path).

use winreg::RegKey;
use winreg::enums::HKEY_LOCAL_MACHINE;

use super::cleanup::{retrying, warn_left_behind};

/// A key `HKLM\SOFTWARE\goetia-test-<name>`, with everything under it, removed on drop; a removal
/// that fails is retried as [`retrying`] does. Make it before the key it names is created, so that
/// a failed assertion still removes it. `name` is the test's to make specific.
pub struct ScratchKey {
    path: String,
}

impl ScratchKey {
    pub fn new(name: &str) -> Self {
        Self {
            path: format!(r"SOFTWARE\goetia-test-{name}"),
        }
    }

    /// The key, created if it is not there, open for everything.
    pub fn create(&self) -> RegKey {
        let (key, _) = RegKey::predef(HKEY_LOCAL_MACHINE)
            .create_subkey(&self.path)
            .expect("create the scratch key");
        key
    }

    /// Its path below `HKLM`.
    pub fn path(&self) -> &str {
        &self.path
    }

    /// The key as the kernel names it, which is what a registry link's value holds.
    pub fn nt_path(&self) -> String {
        format!(r"\Registry\Machine\{}", self.path)
    }
}

impl Drop for ScratchKey {
    /// Never panics, so a drop that runs while the test unwinds still removes the key.
    fn drop(&mut self) {
        let hklm = RegKey::predef(HKEY_LOCAL_MACHINE);
        warn_left_behind(retrying(|| match hklm.delete_subkey_all(&self.path) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => Some(format!(r"remove HKLM\{}: {e}", self.path)),
            _ => None,
        }));
    }
}
