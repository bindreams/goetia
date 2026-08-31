//! The launchd backend.

pub mod generate;

#[cfg(target_os = "macos")]
pub mod manager;
