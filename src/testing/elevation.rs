//! The skuld precondition and label of every test that writes real system state.

/// Opt-out marker for the tests that write real system state: `SKULD_LABELS='!elevated'`.
#[skuld::label]
pub const ELEVATED: skuld::Label;

/// Skuld precondition. CI runs the test binaries under elevation precisely so
/// this never reports unavailable there — an unmet precondition in CI is a
/// silent skip of the tests that gate the metadata design.
pub fn elevated() -> Result<(), String> {
    if cosca::elevation::is_elevated() {
        Ok(())
    } else {
        Err("writes real system state; re-run elevated (sudo, or an elevated shell)".to_string())
    }
}
