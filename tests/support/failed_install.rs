//! What a failed install's error says.

/// The install failed on a path under a regular file (`ENOTDIR`), and not earlier for some other
/// reason: the tests that put such a path after the directories to undo need to know the failure is
/// the one they planted.
pub fn assert_failed_on_a_path_under_a_file(err: &goetia::Error) {
    let cause = std::error::Error::source(err)
        .map(ToString::to_string)
        .unwrap_or_default();
    assert!(cause.contains("ENOTDIR"), "{err}: {cause}");
}
