//! The systemd backend.

pub mod generate;
#[cfg(target_os = "linux")]
pub mod manager;

/// The oldest systemd goetia runs against: `systemctl clean` works on a timer from 243, and arming a
/// timer uses it.
pub(crate) const SYSTEMD_FLOOR: u32 = 243;

/// The oldest systemd whose `systemctl` has `--show-transaction`, which every start and stop runs.
pub(crate) const SHOW_TRANSACTION: u32 = 242;

/// The oldest systemd that parses the `Type=exec` generated units declare.
pub(crate) const TYPE_EXEC: u32 = 240;
