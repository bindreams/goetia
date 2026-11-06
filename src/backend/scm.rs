//! The scm backend.

pub mod exit_status;
pub mod generate;
pub(crate) mod kill;
#[cfg(windows)]
pub mod manager;
// `pub` so the shim integration tests arm through `SystemScmActor`; only the actor, `ScmActor` and
// the types in its signatures are `pub` inside. Declared last because `cargo fmt` sorts these
// alphabetically and would move a comment placed above `manager` onto it.
pub mod wait;
