//! The skuld precondition and label of every test that writes real system state.
//!
//! Shared by the integration suites (through `support`) and the library's unit tests (as
//! `backend::test_elevation`, by path).

/// Opt-out marker for the tests that write real system state: `SKULD_LABELS='!elevated'`.
#[skuld::label]
pub const ELEVATED: skuld::Label;

/// Skuld precondition. CI runs the test binaries under elevation precisely so
/// this never reports unavailable there — an unmet precondition in CI is a
/// silent skip of the tests that gate the metadata design.
pub fn elevated() -> Result<(), String> {
    #[cfg(unix)]
    {
        if unsafe { libc::geteuid() } == 0 {
            Ok(())
        } else {
            Err("writes real system state; re-run under sudo".to_string())
        }
    }
    #[cfg(windows)]
    {
        use windows_service::service_manager::{ServiceManager, ServiceManagerAccess};

        // The capability, not the token: what these tests need is what a filtered admin token
        // refuses, and opening SCM for every access asks for it.
        ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::ALL_ACCESS)
            .map(drop)
            .map_err(|e| format!("writes real system state; re-run from an elevated shell (SCM open failed: {e})"))
    }
}
