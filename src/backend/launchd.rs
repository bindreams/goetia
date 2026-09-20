//! The launchd backend.

pub mod generate;
pub(crate) mod state;

#[cfg(target_os = "macos")]
pub mod manager;
