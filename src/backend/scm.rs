//! The scm backend.

pub mod exit_status;
pub mod generate;
pub(crate) mod kill;
#[cfg(windows)]
pub mod manager;
pub mod state_file;
// `pub(crate)`, not `pub`: `backend` is `pub` in `lib.rs`, so `pub` here
// would publish this Windows-only vocabulary on every platform. Declared
// last because `cargo fmt` sorts these alphabetically and would move a
// comment placed above `manager` onto it.
pub(crate) mod wait;
