//! Integration tests for the effectful SCM backend
//! (`goetia::backend::scm::manager`), `protocol: native` — see that
//! module's own doc comment for the traps this covers. Running a `protocol: plain`
//! service needs `goetia-shim`, so those tests live there; here one is only
//! registered and compared, never started.
//!
//! Every test here registers and removes real Windows services, so it needs
//! elevation exactly like `tests/marker_inertness.rs`'s `scm.rs` probe (see
//! `support::scm_group`), and shares that module's cleanup discipline via
//! `support::ServiceGuard`.

#[path = "support/mod.rs"]
mod support;

#[cfg(windows)]
#[path = "scm_integration/common.rs"]
mod common;
#[cfg(windows)]
#[path = "scm_integration/deny.rs"]
mod deny;
#[cfg(windows)]
#[path = "scm_integration/fixture.rs"]
mod fixture;
#[cfg(windows)]
#[path = "scm_integration/install_helper.rs"]
mod install_helper;
#[cfg(windows)]
#[path = "scm_integration/kill.rs"]
mod kill;
#[cfg(windows)]
#[path = "scm_integration/managed.rs"]
mod managed;

fn main() {
    // Dispatched before the harness starts, exactly like
    // `support::sentinel::run_if_requested` — this process may be the SCM
    // itself launching a daemon this test suite installed (`fixture`), or a
    // one-shot helper child process (`install_helper`), not a test run.
    #[cfg(windows)]
    {
        if fixture::run_if_requested() {
            return;
        }
        if install_helper::run_if_requested() {
            return;
        }
    }
    goetia::testing::run_all();
}
