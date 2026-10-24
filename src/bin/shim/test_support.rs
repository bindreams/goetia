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

/// Asserts `lines` is exactly one `log_failure` line for `id`: `goetia-shim[<id>]: ` and then
/// `fragments`, in order, the first one immediately. The text is pinned by the caller, not rebuilt
/// from production's formatters.
pub(crate) fn assert_one_failure(lines: &[String], id: &str, fragments: &[&str]) {
    let [line] = lines else {
        panic!("expected exactly one `log_failure` line for {id}, got {lines:?}");
    };
    let Some(mut rest) = line.strip_prefix(&format!("goetia-shim[{id}]: ")) else {
        panic!("not a line for {id}: {line:?}");
    };
    for (i, fragment) in fragments.iter().enumerate() {
        let at = rest
            .find(fragment)
            .unwrap_or_else(|| panic!("{fragment:?} is missing, or out of order, in {line:?}"));
        assert!(i > 0 || at == 0, "{line:?} does not begin with {fragment:?}");
        rest = &rest[at + fragment.len()..];
    }
}

#[path = "test_support_tests.rs"]
mod test_support_tests;
