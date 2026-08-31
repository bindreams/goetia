//! Goetia: install system daemons described in `goetia.yaml` as native
//! Windows/SCM, macOS/launchd, and Linux/systemd services.

pub mod backend;
pub mod blob;
pub mod decide;
pub mod diff;
pub mod error;
pub mod spec;

#[cfg(test)]
mod version_tests;

pub use error::Error;

/// The running crate version, as declared in `Cargo.toml`.
pub fn version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

#[cfg(test)]
fn main() {
    skuld::run_all();
}
