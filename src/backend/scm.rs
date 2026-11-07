//! The scm backend.

pub mod exit_status;
pub mod generate;
pub(crate) mod kill;
#[cfg(windows)]
pub mod manager;
pub mod parameters;
// `pub` on Windows for the shim integration tests; crate-private elsewhere. Declared last because
// `cargo fmt` sorts these alphabetically and would move a comment placed above `manager` onto it.
#[cfg(windows)]
pub mod wait;
#[cfg(not(windows))]
pub(crate) mod wait;
