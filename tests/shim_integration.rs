//! Integration tests for `goetia-shim` and the `type: simple` SCM path — see
//! `goetia::backend::scm::manager`'s own module doc comment for the five
//! traps `tests/scm_integration.rs` covers for `type: managed`;
//! this file is the `type: simple` analogue, plus the shim's own four
//! failure paths (a commanded stop must not respawn, the restart-delay
//! default, a logged-not-crashed spawn failure, and the version-skew
//! fallback log).
//!
//! Every test here registers and removes real Windows services and needs
//! `CARGO_BIN_EXE_goetia-shim` — see `main` below for how `GOETIA_SHIM_PATH`
//! is set once, before any test runs, so
//! `goetia::backend::scm::manager::shim_path` resolves the real shim binary
//! rather than guessing from this test binary's own (wrong, for a shim)
//! `current_exe`.

#[path = "support/mod.rs"]
mod support;

#[cfg(windows)]
#[path = "shim_integration/common.rs"]
mod common;
#[cfg(windows)]
#[path = "shim_integration/fixture.rs"]
mod fixture;
#[cfg(windows)]
#[path = "shim_integration/simple.rs"]
mod simple;
#[cfg(windows)]
#[path = "shim_integration/supervisor_integration.rs"]
mod supervisor_integration;

fn main() {
    #[cfg(windows)]
    {
        // Set once, here, strictly before `skuld::run_all()` spawns any test
        // thread — a single write with a happens-before edge to every
        // reader, not the per-test mutation `identity::service_password`'s
        // doc comment (and `tests/scm_integration/install_helper.rs`'s)
        // explains `std::env::set_var` cannot safely do. See
        // `backend::scm::manager::shim_path`'s own doc comment for why this
        // env var exists at all: `current_exe()` from inside this test
        // binary resolves to `target/<profile>/deps/<...>.exe`, whose
        // sibling directory never contains `goetia-shim.exe`.
        unsafe {
            std::env::set_var("GOETIA_SHIM_PATH", env!("CARGO_BIN_EXE_goetia-shim"));
        }
        if fixture::run_if_requested() {
            return;
        }
    }
    skuld::run_all();
}
