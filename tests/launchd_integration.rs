//! Elevated integration tests for the effectful launchd backend
//! ([`goetia::backend::launchd::manager::LaunchdManager`]), plus the shared
//! conformance suite run against it for real.
//!
//! `fn main` is unconditional (never `#![cfg(target_os = "macos")]`'d at the
//! crate root) so this binary still links and reports zero tests on
//! non-macOS CI cells, rather than failing to find `main` at all — the same
//! shape `tests/marker_inertness.rs` already uses. Every actual test lives
//! in `launchd`, which is the part gated to macOS.

#[path = "support/mod.rs"]
mod support;

#[cfg(target_os = "macos")]
mod launchd;

fn main() {
    // Dispatched before the harness starts; see `support::sentinel`. This
    // binary (`CARGO_BIN_EXE_...`) is itself the program the manager
    // installs and starts in every test below.
    if support::sentinel::run_if_requested() {
        return;
    }
    skuld::run_all();
}
