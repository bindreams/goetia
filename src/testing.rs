//! Helpers the library's unit tests and its integration tests share. An integration test reaches the
//! library only through its public API, so what both need is exported here, under the `testing`
//! feature (and under `cfg(test)`, for the unit tests). Not public API: nothing here is stable.

#[cfg(windows)]
use std::path::PathBuf;
#[cfg(windows)]
use std::sync::{Arc, Mutex};

#[cfg(windows)]
use crate::error::Result;
#[cfg(windows)]
use crate::shim::{FolderCheck, HostCommand, ShimProvider, SweepFailure};
use crate::spec::{DaemonFields, Id, Kind, Restart, User};

#[cfg(windows)]
pub use crate::backend::scm::sid::sid_display;

pub mod cleanup;
pub mod elevation;
#[cfg(windows)]
pub mod raw_registry;
#[cfg(windows)]
pub mod scratch;

/// A plain root, `restart: never`, `type: managed` daemon named `id`; panics on an invalid `Id`.
/// Vary fields with struct-update syntax, then `DaemonSpec::new`.
pub fn plain_fields(id: &str, command: Vec<String>) -> DaemonFields {
    DaemonFields {
        id: Id::try_from(id).unwrap_or_else(|e| panic!("`{id}` is not a valid Id: {e}")),
        name: id.to_string(),
        command,
        cwd: None,
        env: std::collections::BTreeMap::new(),
        user: User::Root,
        restart: Restart::Never,
        restart_delay: None,
        logs: None,
        kind: Kind::Managed,
    }
}

/// A [`ShimProvider`] that records the calls it gets (`"preview"`, `"provide"`, `"sweep"`, in
/// order) and answers with the commands it was given. It touches nothing.
#[cfg(windows)]
pub struct FakeProvider {
    calls: Arc<Mutex<Vec<&'static str>>>,
    preview: HostCommand,
    provide: HostCommand,
}

#[cfg(windows)]
impl FakeProvider {
    /// The provider, answering `preview` with `preview` and `provide` with `provide`, and the
    /// calls it records.
    pub fn new(preview: HostCommand, provide: HostCommand) -> (Self, Arc<Mutex<Vec<&'static str>>>) {
        let calls = Arc::default();
        let provider = Self {
            calls: Arc::clone(&calls),
            preview,
            provide,
        };
        (provider, calls)
    }
}

#[cfg(windows)]
impl ShimProvider for FakeProvider {
    fn preview(&self) -> Result<HostCommand> {
        self.calls.lock().unwrap().push("preview");
        Ok(self.preview.clone())
    }

    /// None, without recording a call: its `sweep` removes nothing, so it covers no folder.
    fn folder(&self) -> Result<Option<PathBuf>> {
        Ok(None)
    }

    fn provide(&self, _folders: &FolderCheck) -> Result<HostCommand> {
        self.calls.lock().unwrap().push("provide");
        Ok(self.provide.clone())
    }

    fn sweep(&self, _referenced: &[PathBuf], _folders: &FolderCheck) -> std::result::Result<(), SweepFailure> {
        self.calls.lock().unwrap().push("sweep");
        Ok(())
    }
}

/// A host that runs `exe` with no arguments.
#[cfg(windows)]
pub fn command(exe: impl Into<PathBuf>) -> HostCommand {
    HostCommand {
        exe: exe.into(),
        args: vec![],
    }
}
