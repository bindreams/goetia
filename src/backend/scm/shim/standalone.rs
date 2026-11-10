//! [`StandaloneShim`]: a service host the program already has.

use std::path::PathBuf;

use super::acl::FolderCheck;
use super::provider::{HostCommand, ShimProvider};
use super::sweep::SweepFailure;
use crate::error::{Error, Result};

/// A host at a fixed path with fixed arguments: the file an installer placed, or the program
/// itself started with an argument that makes it call `goetia::shim::main()`. It writes nothing
/// and removes nothing.
pub struct StandaloneShim {
    exe: PathBuf,
    args: Vec<String>,
}

impl StandaloneShim {
    pub fn new(exe: PathBuf, args: Vec<String>) -> Self {
        Self { exe, args }
    }
}

impl ShimProvider for StandaloneShim {
    /// The command as given. A relative `exe` is [`Error::ShimPathRelative`].
    fn preview(&self) -> Result<HostCommand> {
        if !self.exe.is_absolute() {
            return Err(Error::ShimPathRelative { path: self.exe.clone() });
        }
        Ok(HostCommand {
            exe: self.exe.clone(),
            args: self.args.clone(),
        })
    }

    /// The folder of `exe`: where the host is, not a folder the provider sweeps, since its `sweep`
    /// removes nothing. A relative `exe` is [`Error::ShimPathRelative`].
    fn folder(&self) -> Result<PathBuf> {
        let command = self.preview()?;
        match command.exe.parent() {
            Some(folder) => Ok(folder.to_path_buf()),
            None => Err(Error::os_at(
                "find the folder of",
                &command.exe,
                std::io::Error::from(std::io::ErrorKind::InvalidInput),
            )),
        }
    }

    /// The same, once the file is confirmed to exist.
    fn provide(&self, _folders: &FolderCheck) -> Result<HostCommand> {
        let command = self.preview()?;
        std::fs::metadata(&command.exe).map_err(|e| Error::os_at("stat", &command.exe, e))?;
        Ok(command)
    }

    fn sweep(&self, _referenced: &[PathBuf], _folders: &FolderCheck) -> std::result::Result<(), SweepFailure> {
        Ok(())
    }
}

#[cfg(test)]
#[path = "standalone_tests.rs"]
mod standalone_tests;
