//! The scm backend.

pub mod exit_status;
#[cfg(windows)]
pub(crate) mod folder;
pub mod generate;
// Pure, so it builds and is tested on every host; only the Windows backend uses it.
#[cfg_attr(not(windows), allow(dead_code, reason = "off Windows nothing calls it"))]
pub(crate) mod instance;
pub(crate) mod kill;
#[cfg(windows)]
pub mod manager;
pub mod parameters;
// The run records of a task: written by the shim, read by `run` and status.
#[cfg(windows)]
pub(crate) mod runs;
pub(crate) mod shim;
pub(crate) mod sid;
// Pure, like `instance`: builds and is tested on every host; only the Windows backend uses it.
#[cfg_attr(not(windows), allow(dead_code, reason = "off Windows nothing calls it"))]
pub(crate) mod task_xml;
#[cfg(test)]
pub(crate) mod test_support;
// Pure, like `instance`: builds and is tested on every host; only the Windows backend uses it.
#[cfg_attr(not(windows), allow(dead_code, reason = "off Windows nothing calls it"))]
pub(crate) mod triggers;
// `pub` on Windows for the shim integration tests and the marker-inertness probe; crate-private
// elsewhere.
#[cfg(windows)]
pub mod wait;
#[cfg(not(windows))]
pub(crate) mod wait;
#[cfg(windows)]
pub(crate) mod wide;
#[cfg(windows)]
pub(crate) mod win32;
