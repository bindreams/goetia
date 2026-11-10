//! Integration tests for `goetia-shim` and the `type: simple` SCM path — see
//! `goetia::backend::scm::manager`'s own module doc comment for the five
//! traps `tests/scm_integration.rs` covers for `type: managed`;
//! this file is the `type: simple` analogue, plus the shim's own failure
//! paths (a commanded stop must not be restarted, a logged-not-crashed spawn
//! failure, and the version-skew fallback log) and the SCM recovery actions
//! that do every restart (`restart_integration`).
//!
//! Every test here registers and removes real Windows services and needs
//! `CARGO_BIN_EXE_goetia-shim`: `common::manager()` hands it to the manager as a `StandaloneShim`,
//! since `current_exe()` from inside this test binary is `target/<profile>/deps/<...>.exe`, whose
//! sibling directory never holds a shim.

#[path = "support/mod.rs"]
mod support;

#[cfg(windows)]
#[path = "shim_integration/common.rs"]
mod common;
// Shared with `tests/scm_integration.rs` by path, the same way both binaries
// share `support/mod.rs`: denying a read is one mechanism, and a second copy
// of it here would desync on a one-sided edit.
#[cfg(windows)]
#[path = "scm_integration/deny.rs"]
mod deny;
#[cfg(windows)]
#[path = "shim_integration/fixture.rs"]
mod fixture;
#[cfg(windows)]
#[path = "shim_integration/install_helper.rs"]
mod install_helper;
#[cfg(windows)]
#[path = "shim_integration/provision.rs"]
mod provision;
#[cfg(windows)]
#[path = "shim_integration/restart_integration.rs"]
mod restart_integration;
#[cfg(windows)]
#[path = "shim_integration/simple.rs"]
mod simple;
#[cfg(windows)]
#[path = "shim_integration/supervisor_integration.rs"]
mod supervisor_integration;

fn main() {
    #[cfg(windows)]
    {
        if fixture::run_if_requested() || install_helper::run_if_requested() {
            return;
        }
    }
    skuld::run_all();
}
