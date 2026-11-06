//! The scm backend.

pub mod exit_status;
pub mod generate;
pub(crate) mod kill;
#[cfg(windows)]
pub mod manager;
// `pub` on Windows so the shim integration tests arm through `SystemScmActor`; elsewhere nothing
// outside the crate uses it, so it stays crate-private. Declared last because `cargo fmt` sorts
// these alphabetically and would move a comment placed above `manager` onto it.
#[cfg(windows)]
pub mod wait;
#[cfg(not(windows))]
pub(crate) mod wait;
