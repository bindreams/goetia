//! [`Backend`]: what one platform contributes to a [`ServiceManager`].
//!
//! A backend answers primitives: render a definition, read what is installed, write one. The
//! flows built from them (`install`, `preview_install`, `status`, `list`, the ownership gate) are
//! written once, in [`crate::manager::flow`], and the blanket impl below makes every [`Backend`] a
//! [`ServiceManager`]. A backend therefore cannot skip [`crate::decide::decide`] or classify an id
//! differently from its neighbours: it has no `install` to write.

use crate::blob::Blob;
use crate::decide::ExternalConfig;
use crate::error::Result;
use crate::manager::{Budget, Installed, Prepared, ServiceManager, Status, Step, flow};
use crate::spec::{DaemonSpec, Id};

// Backend =============================================================================================================

/// One platform's primitives. See the module doc.
pub(crate) trait Backend {
    /// The service's definition in this manager's form. Its `Display` is the text `decide`
    /// compares and diffs.
    type Definition: std::fmt::Display;
    /// What `read` saw; only this backend looks inside.
    type Snapshot;
    type InstallScope;

    // Primitives ------------------------------------------------------------------------------------------------------

    /// Everything an install needs before it reads anything, held until it ends: a version gate, a
    /// lock, the check that a manager can be asked. Fails before anything is read or written.
    fn begin_install(&self, spec: &DaemonSpec) -> Result<Self::InstallScope>;

    /// `spec` in this manager's form. Fails where the platform cannot express it (an account that
    /// does not resolve).
    fn render(&self, spec: &DaemonSpec) -> Result<Self::Definition>;

    /// What is at `id`. `Err` where the read did not complete: neither absence nor ownership is
    /// established.
    fn read(&self, id: &Id) -> Result<ReadOutcome<Self::Snapshot>>;

    /// The definition as installed, and the configuration applied to it from outside, read for an
    /// id `read` found marked.
    fn installed(&self, id: &Id, snapshot: &Self::Snapshot) -> Result<InstalledDefinition<Self::Definition>>;

    /// Write `definition` for the `Create`, `Update` or `Stale` `outcome` that `decide` chose over
    /// `snapshot` (`None` for `Create`).
    fn write(
        &self,
        spec: &DaemonSpec,
        definition: &Self::Definition,
        outcome: &crate::decide::Outcome,
        snapshot: Option<&Self::Snapshot>,
        scope: &mut Self::InstallScope,
    ) -> Result<WriteOutcome>;

    /// The live state of an id `read` found marked.
    fn live_status(&self, id: &Id, snapshot: &Self::Snapshot) -> Result<Status>;

    /// Every id this backend can enumerate, as `read` would classify it, and what the enumeration
    /// itself reports (ids it could not read, a scan that stopped early).
    fn scan(&self) -> Scan<Self::Snapshot>;

    // Verbs -----------------------------------------------------------------------------------------------------------
    //
    // The [`ServiceManager`] methods other than the four flows, with the same contracts. The
    // ownership gate they run first is [`flow::owned`].

    /// See [`ServiceManager::uninstall`].
    fn uninstall(&self, id: &Id) -> Result<()>;

    /// See [`ServiceManager::enable`].
    fn enable(&self, id: &Id) -> Result<()>;

    /// See [`ServiceManager::disable`].
    fn disable(&self, id: &Id) -> Result<()>;

    /// See [`ServiceManager::start`].
    fn start(&self, id: &Id, budget: Budget) -> Result<()>;

    /// See [`ServiceManager::prepare`].
    fn prepare(&self, _steps: &[Step], _budget: Budget) -> Result<Prepared> {
        Ok(Prepared::nothing())
    }

    /// See [`ServiceManager::request_restart`].
    fn request_restart(&self, _id: &Id) -> Option<Result<()>> {
        None
    }

    /// See [`ServiceManager::request_start_after_stop`].
    fn request_start_after_stop(&self, id: &Id) -> Result<()>;

    /// See [`ServiceManager::stop`].
    fn stop(&self, id: &Id, budget: Budget) -> Result<()>;

    /// See [`ServiceManager::kill`].
    fn kill(&self, id: &Id) -> Result<()>;
}

// Data ================================================================================================================

/// What [`Backend::read`] found at an id.
// `Marked` holds a whole `Blob`, which embeds a full `DaemonSpec`, so it is much larger than the
// other variants. Boxing it would ripple into every match arm for no behavioral benefit: one
// `ReadOutcome` is built per daemon per verb, never in a hot loop. Same call as `decide::Ownership`.
#[allow(clippy::large_enum_variant)]
pub(crate) enum ReadOutcome<S> {
    /// Nothing goetia-attributable is here.
    Absent,
    /// Something is here that carries no Goetia marker, or a trace the platform still applies
    /// without an artifact of its own. `recovery` is the backend's own remedy where it has one;
    /// `None` means [`crate::decide::foreign_recovery`]. `status` and the ownership gate use it;
    /// `install` always uses `foreign_recovery`.
    Foreign { recovery: Option<String> },
    /// Goetia's marker is here. `blob` is its decoded metadata, or why it will not decode.
    Marked { blob: Result<Blob>, snapshot: S },
}

/// What [`Backend::installed`] read back.
pub(crate) struct InstalledDefinition<D> {
    pub definition: D,
    pub external: ExternalConfig,
}

/// How a [`Backend::write`] went.
pub(crate) enum WriteOutcome {
    Done,
    /// Something changed at the id between `read` and the write, which therefore wrote nothing.
    Raced,
}

/// What [`Backend::scan`] enumerated.
pub(crate) struct Scan<S> {
    /// Every id the scan classified, by name.
    pub entries: Vec<(String, ReadOutcome<S>)>,
    /// What the scan itself reports, listed after the classified entries.
    pub reported: Vec<Installed>,
}

// ServiceManager ======================================================================================================

impl<B: Backend> ServiceManager for B {
    fn install(&self, spec: &DaemonSpec, force: bool) -> Result<crate::decide::Outcome> {
        flow::install(self, spec, force)
    }

    fn preview_install(&self, spec: &DaemonSpec) -> Result<crate::decide::Outcome> {
        flow::preview(self, spec)
    }

    fn uninstall(&self, id: &Id) -> Result<()> {
        Backend::uninstall(self, id)
    }

    fn enable(&self, id: &Id) -> Result<()> {
        Backend::enable(self, id)
    }

    fn disable(&self, id: &Id) -> Result<()> {
        Backend::disable(self, id)
    }

    fn start(&self, id: &Id, budget: Budget) -> Result<()> {
        Backend::start(self, id, budget)
    }

    fn prepare(&self, steps: &[Step], budget: Budget) -> Result<Prepared> {
        Backend::prepare(self, steps, budget)
    }

    fn request_restart(&self, id: &Id) -> Option<Result<()>> {
        Backend::request_restart(self, id)
    }

    fn request_start_after_stop(&self, id: &Id) -> Result<()> {
        Backend::request_start_after_stop(self, id)
    }

    fn stop(&self, id: &Id, budget: Budget) -> Result<()> {
        Backend::stop(self, id, budget)
    }

    fn kill(&self, id: &Id) -> Result<()> {
        Backend::kill(self, id)
    }

    fn status(&self, id: &Id) -> Result<Status> {
        flow::status(self, id)
    }

    fn list(&self) -> Result<Vec<Installed>> {
        flow::list(self)
    }
}
