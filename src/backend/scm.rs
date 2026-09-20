//! The scm backend.

pub mod generate;
#[cfg(windows)]
pub mod manager;
// `pub(crate)`, not `pub`: `backend` is `pub` in `lib.rs`, so `pub` here
// would publish this Windows-only vocabulary on every platform. Declared
// last because `cargo fmt` sorts these alphabetically and would move a
// comment placed above `manager` onto it.
pub(crate) mod wait;
