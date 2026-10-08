//! Fixtures shared by the shim's unit tests.

// rand 0.10 moved `random` off `Rng` onto `RngExt`.
use rand::RngExt as _;

use crate::logging;

/// A per-run daemon id, and the removal of the fallback log file its `log_failure` calls would
/// write if they reached the filesystem.
///
/// In a test build `log_failure` journals instead (see `logging::test_hook`), so nothing is
/// written unless that seam regresses; removing the file on drop stops such a regression from
/// leaking one file per run.
///
/// Every test needs its own id: the journal is one process-global map shared by tests running in
/// parallel. Random rather than fixed also because tests assert the file is absent, and a fixed id
/// would keep failing that check after a killed run left the file behind; a random id can never be
/// a really-installed daemon's either. (`tests/support`'s `random_test_id` is `tests/`-only; a
/// binary crate cannot reach it.)
pub(crate) struct TestId(String);

impl TestId {
    pub(crate) fn new(prefix: &str) -> Self {
        Self(format!("{prefix}-{:016x}", rand::rng().random::<u64>()))
    }

    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }
}

impl Drop for TestId {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(logging::default_log_path(&self.0));
    }
}
