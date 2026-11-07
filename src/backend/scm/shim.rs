//! The shim host's installer-side machinery.

pub(crate) mod acl;
pub(crate) mod embedded;
pub(crate) mod marker;
pub(crate) mod provider;
#[cfg(windows)]
pub(crate) mod run;
#[cfg(windows)]
pub(crate) mod standalone;
