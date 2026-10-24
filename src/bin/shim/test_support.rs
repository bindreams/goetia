//! Fixtures shared by the shim's unit tests.

use std::sync::atomic::{AtomicU64, Ordering};

// rand 0.10 moved `random` off `Rng` onto `RngExt`.
use rand::RngExt as _;

use crate::{logging, stop_bus};

/// A per-test daemon id, unique in the process (the journals are keyed by id), whose drop checks
/// that `log_failure` wrote no fallback file, and that the test consumed its id's journalled lines
/// and its waiter's end. The random part avoids ids left by a killed run or a real daemon.
pub(crate) struct TestId(String);

static NEXT: AtomicU64 = AtomicU64::new(0);

impl TestId {
    pub(crate) fn new(prefix: &str) -> Self {
        Self(format!(
            "{prefix}-{:016x}-{}",
            rand::rng().random::<u64>(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ))
    }

    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }
}

impl Drop for TestId {
    /// Skipped while panicking: a failed test leaves its state behind and the original panic is the
    /// useful one.
    fn drop(&mut self) {
        if std::thread::panicking() {
            return;
        }
        let path = logging::default_log_path(&self.0);
        assert!(
            !path.exists(),
            "`log_failure` wrote the fallback log {} for {}: the test-build journal seam regressed",
            path.display(),
            self.0
        );
        let unconsumed = logging::test_hook::peek(&self.0);
        assert!(
            unconsumed.is_empty(),
            "{} ended with `log_failure` lines it never asserted on: {unconsumed:?}",
            self.0
        );
        assert!(
            !stop_bus::test_hook::has_waiter_end(&self.0),
            "{} ended without consuming its waiter's end (`waiter_end`)",
            self.0
        );
    }
}

/// Asserts `lines` is exactly one `log_failure` line for `id`: `goetia-shim[<id>]: ` then
/// `expected`, with nothing before, between or after. The caller writes `expected` out in full
/// (splicing in only the injected error it chose), not rebuilt from production's formatters, so a
/// change to any part of the text fails the test.
pub(crate) fn assert_one_failure(lines: &[String], id: &str, expected: &str) {
    let [line] = lines else {
        panic!("expected exactly one `log_failure` line for {id}, got {lines:?}");
    };
    assert_eq!(line, &format!("goetia-shim[{id}]: {expected}"));
}
