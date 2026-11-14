//! The launchd backend.

pub(crate) mod calendar;
pub mod generate;
pub(crate) mod refuse;
pub(crate) mod state;

#[cfg(target_os = "macos")]
pub mod manager;
