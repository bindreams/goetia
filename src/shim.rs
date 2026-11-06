//! The shim host: what an embedding program configures and runs.

#[cfg(windows)]
pub use crate::backend::scm::shim::acl::{CheckPermissions, FolderCheck};
