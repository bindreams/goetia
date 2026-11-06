//! The shim host's installer-side machinery.

pub(crate) mod acl;
pub(crate) mod marker;
#[cfg(windows)]
pub(crate) mod run;
