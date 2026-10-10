//! Fixtures shared by the shim's unit tests.

use std::sync::atomic::{AtomicU64, Ordering};

// rand 0.10 moved `random` off `Rng` onto `RngExt`.
use rand::RngExt as _;

use crate::logging;

/// A daemon id unique to one test, and a check on drop that its `log_failure` calls wrote no
/// fallback log file.
///
/// Uniqueness within the process matters: the journals are process-global maps keyed by id, shared
/// by tests running in parallel, and only the counter guarantees it. The random part keeps a run
/// from reusing the id of a file a killed run left behind, and keeps ids clear of any
/// really-installed daemon's.
pub(crate) struct TestId(String);

static NEXT: AtomicU64 = AtomicU64::new(0);

fn compose(prefix: &str, random: u64, sequence: u64) -> String {
    format!("{prefix}-{random:016x}-{sequence}")
}

impl TestId {
    pub(crate) fn new(prefix: &str) -> Self {
        Self(compose(
            prefix,
            rand::rng().random::<u64>(),
            NEXT.fetch_add(1, Ordering::Relaxed),
        ))
    }

    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }
}

impl Drop for TestId {
    /// In a test build `log_failure` journals instead of writing (see `logging::test_hook`), so
    /// the file existing means that seam regressed. The file is left for inspection.
    fn drop(&mut self) {
        debug_assert!(
            std::thread::panicking() || !logging::default_log_path(&self.0).exists(),
            "`log_failure` wrote the fallback log for {}: the test-build journal seam regressed",
            self.0
        );
    }
}

#[path = "test_support_tests.rs"]
mod test_support_tests;
