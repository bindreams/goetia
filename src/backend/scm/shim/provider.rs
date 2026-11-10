//! What a service host is, and who supplies it: [`HostCommand`] and the Windows-only
//! [`ShimProvider`].

use std::path::PathBuf;

#[cfg(windows)]
use super::acl::FolderCheck;
#[cfg(windows)]
use super::sweep::SweepFailure;
#[cfg(windows)]
use crate::error::Result;

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

    /// The folder this provider's host is in, which its `sweep` covers. Needs neither the host's
    /// bytes nor a successful `preview`: an uninstall names the folder whatever `preview` says. An
    /// error when the folder cannot be known (a relative path, a default folder that cannot be
    /// located). Touches nothing.
    fn folder(&self) -> Result<PathBuf>;

    /// Under the install lock. Leaves the host ready at the returned path: writes, repairs or
    /// refuses. Idempotent. A provider that writes checks its folder with `folders` first.
    fn provide(&self, folders: &FolderCheck) -> Result<HostCommand>;

    /// Under the install lock, as `provide`: `Scm` calls it before an install provides, after an
    /// uninstall, and for an uninstall of an id that is not installed. `referenced` is every file a
    /// registered service or a running process names, as a path that has been normalised. Removes
    /// what this provider wrote and nothing references, tries every file, and reports every failure
    /// after the last, with the files it could not delete. It checks each folder it deletes in with
    /// `folders`: a folder that fails is a failure, not swept.
    fn sweep(&self, referenced: &[PathBuf], folders: &FolderCheck) -> std::result::Result<(), SweepFailure>;
}
