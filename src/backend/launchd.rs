//! The launchd backend.

pub(crate) mod calendar;
pub mod generate;
// Off macOS, `jiff` is there only with the `cli` feature; the module's tests run wherever it is.
#[cfg(any(all(test, feature = "cli"), target_os = "macos"))]
pub(crate) mod next;
pub(crate) mod refuse;
pub(crate) mod state;
pub(crate) mod task;

#[cfg(target_os = "macos")]
pub mod manager;

/// Where `install` writes a plist that is not enabled at boot. launchd never scans this directory,
/// so a job here is inert until `start` bootstraps it or `enable` moves it into [`ENABLED_DIR`].
pub const STAGING_DIR: &str = "/Library/Application Support/Goetia/daemons";

/// The system LaunchDaemons directory. A plist here is loaded at boot: being here is the
/// enrollment bit.
pub const ENABLED_DIR: &str = "/Library/LaunchDaemons";
