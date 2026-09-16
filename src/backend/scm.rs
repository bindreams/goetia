//! The scm backend.

pub mod generate;
// `pub(crate)`, not `pub`: `backend` is `pub` in `lib.rs`, so `pub` here
// would publish this Windows-only vocabulary on every platform.
#[cfg(windows)]
pub mod manager;
pub(crate) mod wait;
