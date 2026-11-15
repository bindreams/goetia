//! The scm backend.

pub mod exit_status;
pub mod generate;
// Pure, so it builds and is tested on every host; only the Windows backend uses it, and its task verbs
// come next.
#[cfg_attr(windows, expect(dead_code, reason = "the task verbs, its callers, come next"))]
#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) mod instance;
pub(crate) mod kill;
#[cfg(windows)]
pub mod manager;
pub mod parameters;
pub(crate) mod shim;
pub(crate) mod sid;
#[cfg(test)]
pub(crate) mod test_support;
// `pub` on Windows for the shim integration tests and the marker-inertness probe; crate-private
// elsewhere.
#[cfg(windows)]
pub mod wait;
#[cfg(not(windows))]
pub(crate) mod wait;
#[cfg(windows)]
pub(crate) mod wide;
