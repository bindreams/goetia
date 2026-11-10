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

pub use crate::backend::scm::shim::acl::{CHANGE_PERMISSIONS_OF, WRITE_TO};

/// `sid` as Windows writes it. A SID Windows cannot show fails the calling test, naming its bytes:
/// a test never takes a placeholder for a SID.
#[cfg(windows)]
pub fn shown_sid(sid: &[u8]) -> String {
    crate::backend::scm::sid::try_sid_display(sid).unwrap_or_else(|| panic!("{sid:?} is not a SID Windows can show"))
}

/// The SID of the local account `account_name`, as the library looks it up when it installs a
/// daemon that runs as it.
#[cfg(windows)]
pub fn sid_for_account_name(account_name: &str) -> Result<Vec<u8>> {
    crate::backend::scm::manager::sid_for_account_name(account_name)
}

/// One entry of a DACL as the library reads it.
#[cfg(windows)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DaclEntry {
    /// The trustee's SID bytes.
    pub trustee: Vec<u8>,
    pub mask: u32,
    /// Allows, else denies.
    pub allow: bool,
    /// The ACE's flags, `INHERIT_ONLY_ACE` among them.
    pub flags: u8,
    /// A callback entry, which has a condition after its SID that the library does not read.
    pub conditional: bool,
}

/// The DACL of the kernel object `handle`, which is open with `READ_CONTROL`: whether it is
/// protected, and its entries (`None` for a NULL DACL). Read by the library's own parser, which
/// fails on an entry type it does not understand. The owner is not read. `name` stands for the
/// object in an error.
#[cfg(windows)]
pub fn kernel_object_dacl(
    handle: *mut std::ffi::c_void,
    name: &std::path::Path,
) -> Result<(bool, Option<Vec<DaclEntry>>)> {
    use crate::backend::scm::shim::acl::{Read, kernel_object_security};

    let dacl = kernel_object_security(handle, name, Read::Dacl)?
        .dacl
        .expect("the DACL was asked for");
    let entries = dacl.aces.map(|aces| {
        aces.into_iter()
            .map(|ace| DaclEntry {
                trustee: ace.trustee,
                mask: ace.mask,
                allow: ace.allow,
                flags: ace.flags,
                conditional: ace.conditional,
            })
            .collect()
    });
    Ok((dacl.protected, entries))
}

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
