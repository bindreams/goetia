//! The shim host's installer-side machinery.

pub(crate) mod acl;
pub(crate) mod embedded;
#[cfg(windows)]
pub(crate) mod lock;
pub(crate) mod marker;
#[cfg(windows)]
pub(crate) mod processes;
pub(crate) mod provider;
#[cfg(windows)]
pub(crate) mod run;
#[cfg(windows)]
pub(crate) mod standalone;
pub(crate) mod sweep;
