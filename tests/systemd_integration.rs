//! Elevated integration tests for the systemd backend (Linux only). Runs `manager::conformance`
//! against a real `Systemd`, then the obligation-specific scenarios Task 11's review found missing
//! from a naive implementation — see `src/backend/systemd/manager.rs`'s module doc comment.
//!
//! Structured like `tests/marker_inertness.rs`: `support` is declared once here, unconditionally, and
//! the platform-gated test bodies live in their own file (`systemd_integration/linux.rs`) that reaches
//! it via `crate::support`, so this still compiles (with zero tests) on the other two CI platforms.

#[path = "support/mod.rs"]
mod support;

#[cfg(target_os = "linux")]
#[path = "systemd_integration/linux.rs"]
mod linux;

fn main() {
    skuld::run_all();
}
