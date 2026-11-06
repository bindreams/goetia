//! Fixtures shared by the shim's unit tests.

use std::sync::Arc;

use crate::os::fake::{Fake, KILL_REFUSED};
use crate::stop_bus::StopBus;

// The shim is its own crate and cannot reach `DaemonSpec::for_test`; share the integration suites'
// constructor.
#[path = "../../../tests/support/daemon_spec.rs"]
pub(crate) mod daemon_spec;

/// What a spawn of a bare binary name not found on `PATH` fails with: `Io(NotFound)` and no OS
/// error code (`os_tests` pins it against `Real`, with the absolute-path case, which carries
/// `ERROR_FILE_NOT_FOUND` from `CreateProcessW`).
pub(crate) fn not_found() -> cosca::error::Error {
    cosca::error::Error::Io(std::io::Error::from(std::io::ErrorKind::NotFound))
}

/// A bus over `fake`'s `Os` and `Log`: the supervisor under test.
pub(crate) fn bus(fake: &Fake) -> Arc<StopBus> {
    Arc::new(StopBus::new(fake.os(), fake.log()))
}

/// Asserts `lines` is exactly one failure line for `id`: `goetia-shim[<id>]: ` then `expected`, with
/// nothing before, between or after. The caller writes `expected` out in full (splicing in only the
/// injected error it chose), not rebuilt from production's formatters, so a change to any part of
/// the text fails the test.
pub(crate) fn assert_one_failure(lines: &[String], id: &str, expected: &str) {
    let [line] = lines else {
        panic!("expected exactly one failure line for {id}, got {lines:?}");
    };
    assert_eq!(line, &format!("goetia-shim[{id}]: {expected}"));
}

/// Asserts `lines` is exactly `expected`, in order: each one failure line for `id`, as
/// [`assert_one_failure`] checks it.
pub(crate) fn assert_failures(lines: &[String], id: &str, expected: &[String]) {
    let expected: Vec<String> = expected.iter().map(|e| format!("goetia-shim[{id}]: {e}")).collect();
    assert_eq!(lines, expected);
}

/// The line logged when `kill_tree` is refused and the direct child's kill is tried next.
pub(crate) fn tree_refused_failure() -> String {
    format!(
        "kill_tree: {KILL_REFUSED}; falling back to killing the direct child only (its own descendants, if any, cannot be reached without a containment mechanism)"
    )
}

/// The line logged when the direct child's kill is refused too.
pub(crate) fn fallback_refused_failure() -> String {
    format!(
        "fallback kill: {KILL_REFUSED}; the daemon was neither killed nor reaped and may still be running — SCM is told the service stopped with Win32 exit code 0, so that it does not start a second copy beside it, and with service-specific exit code 1003 as the record of this failure"
    )
}
