//! The systemd backend.

pub mod generate;
#[cfg(target_os = "linux")]
pub mod manager;
