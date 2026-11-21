//! Integration tests for `goetia-shim` and the `protocol: plain` SCM path — see
//! `goetia::backend::scm::manager`'s own module doc comment for the five
//! traps `tests/scm_integration.rs` covers for `protocol: native`;
//! this file is the `protocol: plain` analogue, plus the shim's own failure
//! paths (a commanded stop must not be restarted, a logged-not-crashed spawn
//! failure, and the version-skew fallback log) and the SCM recovery actions
//! that do every restart (`restart_integration`).
//!
//! Every test here registers and removes real Windows services and needs
//! `CARGO_BIN_EXE_goetia-shim`: `common::manager()` hands it to the manager as a `StandaloneShim`,
//! since `current_exe()` is not beside `goetia-shim.exe`.

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
#[path = "shim_integration/graceful_stop.rs"]
mod graceful_stop;
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
#[cfg(windows)]
#[path = "shim_integration/tasks.rs"]
mod tasks;

fn main() {
    #[cfg(windows)]
    {
        // A host of the shim that did not initialise: before the call below, which every other invocation makes.
        fixture::run_uninitialised_host_if_requested();
        // First thing, as a program that hosts the shim must: the stop's proxy is a copy of this binary.
        goetia::shim::init_host();
        if fixture::run_if_requested() || install_helper::run_if_requested() {
            return;
        }
    }
    goetia::testing::run_all();
}
