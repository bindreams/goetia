//! Elevated probes of launchd's own behaviour, measured on a real macOS host.
//!
//! Each probe writes what it measured through `support::record_probe`, which CI uploads as
//! `probe-results-<os>`. `fn main` is unconditional so this binary still links and reports zero
//! tests off macOS, like `tests/launchd_integration.rs`.

#[path = "support/mod.rs"]
mod support;

#[cfg(target_os = "macos")]
#[path = "launchd_probes/kickstart_kill.rs"]
mod kickstart_kill;

fn main() {
    // Dispatched before the harness starts; see `support::sentinel`. This binary is the program
    // the probe's job runs.
    if support::sentinel::run_if_requested() {
        return;
    }
    skuld::run_all();
}
