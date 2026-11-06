//! What a service host is, and who supplies it: [`HostCommand`] and the Windows-only
//! [`ShimProvider`].

use std::path::PathBuf;

#[cfg(windows)]
use super::acl::FolderCheck;
#[cfg(windows)]
use crate::error::{Error, Result};

/// The command SCM launches for a `type: simple` service: a host built from `goetia::shim::main()`
/// and the arguments it is started with. The arguments are `String`s because the registry read
/// back is lossy, so an `OsString` would promise what it cannot keep.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostCommand {
    /// An absolute path: every provider refuses a relative one.
    pub exe: PathBuf,
    pub args: Vec<String>,
}

/// Where the service host of a `type: simple` service comes from. `Scm` consults one for such a
/// service and for no other; the checks on what it returns (the folder check, the marker, the
/// match with `preview`) are `Scm`'s, so an implementation cannot skip them.
#[cfg(windows)]
pub trait ShimProvider: Send + Sync {
    /// What `provide` would return, touching nothing. `--dry-run` and `render` use it.
    fn preview(&self) -> Result<HostCommand>;

    /// Under the install lock. Leaves the host ready at the returned path: writes, repairs or
    /// refuses. Idempotent. A provider that writes checks its folder with `folders` first.
    fn provide(&self, folders: &FolderCheck) -> Result<HostCommand>;

    /// After an uninstall, with every path a remaining service still references. Removes what
    /// this provider wrote and nothing references.
    fn sweep(&self, referenced: &[PathBuf]) -> Result<()>;
}

/// `Scm::new()`'s provider: it has no host, so a `type: simple` service cannot be installed or
/// previewed, and there is nothing to sweep.
#[cfg(windows)]
pub(crate) struct NoShim;

#[cfg(windows)]
impl ShimProvider for NoShim {
    fn preview(&self) -> Result<HostCommand> {
        Err(Error::NoShimProvider)
    }

    fn provide(&self, _folders: &FolderCheck) -> Result<HostCommand> {
        Err(Error::NoShimProvider)
    }

    fn sweep(&self, _referenced: &[PathBuf]) -> Result<()> {
        Ok(())
    }
}

#[cfg(all(windows, test))]
#[path = "provider_tests.rs"]
mod provider_tests;
