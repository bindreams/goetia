//! The systemd backend.

pub mod generate;
#[cfg(target_os = "linux")]
pub mod manager;

/// The oldest systemd goetia runs against: start and stop need `systemctl --show-transaction`.
pub(crate) const SYSTEMD_FLOOR: u32 = 242;

/// The oldest systemd that parses the `Type=exec` generated units declare.
pub(crate) const TYPE_EXEC: u32 = 240;
