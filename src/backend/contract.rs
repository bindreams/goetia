//! [`Backend`]: what one platform contributes to a [`ServiceManager`].
//!
//! A backend answers primitives: render a definition, read what is installed, write one. The
//! flows built from them (`install`, `preview_install`, `status`, `list`, the ownership gate) are
//! written once, in [`crate::manager::flow`], and the blanket impl below makes every [`Backend`] a
//! [`ServiceManager`]. A backend therefore cannot skip [`crate::decide::decide`] or classify an id
//! differently from its neighbours: it has no `install` to write.
//!
//! The gate is structural. A verb takes the [`Backend::Snapshot`] of the service it acts on, which
//! only [`Backend::read`] and [`Backend::scan`] make and only for a service carrying goetia's
//! marker, so a backend cannot act on a service goetia did not create. The blanket impl is the one
//! place that turns an id into a snapshot.

use crate::blob::Blob;
use crate::decide::ExternalConfig;
use crate::error::{Error, Result};
use crate::manager::{Budget, Installed, Prepared, ServiceManager, Status, Step, flow};
use crate::spec::{DaemonSpec, Id};

// Backend =============================================================================================================

/// A service as it is installed, and goetia's by its marker: what [`Backend::read`] and
/// [`Backend::scan`] hand out for a marked id, and so the proof of ownership a verb is given.
pub(crate) trait InstalledService {
    fn id(&self) -> &Id;

    /// The metadata as `read` decoded it, or why it will not decode. Kept as the failure: the
    /// marker alone is proof of ownership, so `uninstall` and the other verbs must still work on a
    /// service whose blob is unreadable.
    fn blob(&self) -> std::result::Result<&Blob, &Error>;
}

/// One platform's primitives. See the module doc.
pub(crate) trait Backend {
    /// The service's definition in this manager's form. Its `Display` is the text `decide`
    /// compares and diffs.
    type Definition: std::fmt::Display;
    /// What `read` saw at a marked id; only this backend looks inside. Built only by `read` and
    /// `scan`.
    type Snapshot: InstalledService;
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

    /// The definition as installed, and the configuration applied to it from outside, read for a
    /// service `read` found marked.
    fn installed(&self, snapshot: &Self::Snapshot) -> Result<InstalledDefinition<Self::Definition>>;

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

    /// The live state of a service `read` found marked. `Error::NotInstalled` where the query
    /// completed and found the service gone, which `list` omits; never for one that is present.
    fn live_status(&self, snapshot: &Self::Snapshot) -> Result<Status>;

    /// Every id this backend can enumerate, as `read` would classify it, and what the enumeration
    /// itself reports (ids it could not read, a scan that stopped early).
    fn scan(&self) -> Scan<Self::Snapshot>;

    // Verbs -----------------------------------------------------------------------------------------------------------
    //
    // The [`ServiceManager`] methods other than the four flows, with the same contracts, on the
    // snapshot the ownership gate ([`flow::owned`]) yielded. Taken by value so a verb can let go of
    // what the snapshot holds when it must (SCM's open handle keeps a deleted service alive).

    /// See [`ServiceManager::uninstall`].
    fn uninstall(&self, snapshot: Self::Snapshot) -> Result<()>;

    /// See [`ServiceManager::enable`].
    fn enable(&self, snapshot: Self::Snapshot) -> Result<()>;

    /// See [`ServiceManager::disable`].
    fn disable(&self, snapshot: Self::Snapshot) -> Result<()>;

    /// See [`ServiceManager::start`].
    fn start(&self, snapshot: Self::Snapshot, budget: Budget) -> Result<()>;

    /// See [`ServiceManager::prepare`].
    fn prepare(&self, _steps: &[Step], _budget: Budget) -> Result<Prepared> {
        Ok(Prepared::nothing())
    }

    /// The manager's own restart for [`ServiceManager::request_restart`], where it has one. `None`
    /// where it has none, so that the gate is not run for a request that cannot be made.
    fn request_restart(&self) -> Option<NativeRestart<Self>> {
        None
    }

    /// See [`ServiceManager::request_start_after_stop`].
    fn request_start_after_stop(&self, snapshot: Self::Snapshot) -> Result<()>;

    /// See [`ServiceManager::stop`].
    fn stop(&self, snapshot: Self::Snapshot, budget: Budget) -> Result<()>;

    /// See [`ServiceManager::kill`].
    fn kill(&self, snapshot: Self::Snapshot) -> Result<()>;
}

/// One [`Backend`]'s native restart of a service the gate passed.
pub(crate) type NativeRestart<B> = fn(&B, <B as Backend>::Snapshot) -> Result<()>;

// Data ================================================================================================================

/// What [`Backend::read`] found at an id.
// `Marked` holds a whole snapshot, which embeds a full `DaemonSpec` through its blob, so it is much
// larger than the other variants. Boxing it would ripple into every match arm for no behavioral
// benefit: one `ReadOutcome` is built per daemon per verb, never in a hot loop. Same call as
// `decide::Ownership`.
#[allow(clippy::large_enum_variant)]
pub(crate) enum ReadOutcome<S> {
    /// Nothing goetia-attributable is here.
    Absent,
    /// Something is here that carries no Goetia marker, or a trace the platform still applies
    /// without an artifact of its own. `recovery` is the backend's own remedy where it has one;
    /// `None` means [`crate::decide::foreign_recovery`]. `status` and the ownership gate use it;
    /// `install` always uses `foreign_recovery`.
    Foreign { recovery: Option<String> },
    /// Goetia's marker is here, whether or not its metadata decodes.
    Marked(S),
}

/// A marked artifact as a backend's classifier found it, before its name is checked to be an id:
/// what `read` and `scan` share, and each turns into a snapshot by naming it ([`ReadOutcome::named`],
/// [`ScanEntry::of`]).
pub(crate) struct Unnamed<B> {
    pub blob: Result<Blob>,
    /// What only this backend looks at: the snapshot minus its id and blob.
    pub body: B,
}

impl<B> ReadOutcome<Unnamed<B>> {
    /// The outcome at `id`, with a marked artifact made into the snapshot `make` builds.
    pub(crate) fn named<S>(self, id: &Id, make: impl FnOnce(Id, Result<Blob>, B) -> S) -> ReadOutcome<S> {
        match self {
            Self::Absent => ReadOutcome::Absent,
            Self::Foreign { recovery } => ReadOutcome::Foreign { recovery },
            Self::Marked(Unnamed { blob, body }) => ReadOutcome::Marked(make(id.clone(), blob, body)),
        }
    }
}

/// One name [`Backend::scan`] classified.
// `Misnamed` holds a whole `Blob`, as `ReadOutcome::Marked` does through its snapshot; same call.
#[allow(clippy::large_enum_variant)]
pub(crate) enum ScanEntry<S> {
    /// As [`Backend::read`] would have.
    Read(ReadOutcome<S>),
    /// Goetia's marker is here, at a name that is no id, so there is no snapshot to make (a snapshot
    /// names its id). `blob` is the metadata as [`InstalledService::blob`] would say, and `name`
    /// why the name is no id.
    Misnamed { blob: Result<Blob>, name: Error },
}

impl<S> ScanEntry<S> {
    /// `found` at `name`: named an id where it is one, and otherwise [`Self::Misnamed`] if it is
    /// marked.
    pub(crate) fn of<B>(
        name: &str,
        found: ReadOutcome<Unnamed<B>>,
        make: impl FnOnce(Id, Result<Blob>, B) -> S,
    ) -> Self {
        match (Id::try_from(name), found) {
            (Ok(id), found) => Self::Read(found.named(&id, make)),
            (Err(name), ReadOutcome::Marked(Unnamed { blob, .. })) => Self::Misnamed { blob, name },
            (Err(_), ReadOutcome::Absent) => Self::Read(ReadOutcome::Absent),
            (Err(_), ReadOutcome::Foreign { recovery }) => Self::Read(ReadOutcome::Foreign { recovery }),
        }
    }
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
    pub entries: Vec<(String, ScanEntry<S>)>,
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
        Backend::uninstall(self, flow::owned(self, id)?)
    }

    fn enable(&self, id: &Id) -> Result<()> {
        Backend::enable(self, flow::owned(self, id)?)
    }

    fn disable(&self, id: &Id) -> Result<()> {
        Backend::disable(self, flow::owned(self, id)?)
    }

    fn start(&self, id: &Id, budget: Budget) -> Result<()> {
        Backend::start(self, flow::owned(self, id)?, budget)
    }

    fn prepare(&self, steps: &[Step], budget: Budget) -> Result<Prepared> {
        Backend::prepare(self, steps, budget)
    }

    fn request_restart(&self, id: &Id) -> Option<Result<()>> {
        let restart = Backend::request_restart(self)?;
        Some(flow::owned(self, id).and_then(|snapshot| restart(self, snapshot)))
    }

    fn request_start_after_stop(&self, id: &Id) -> Result<()> {
        Backend::request_start_after_stop(self, flow::owned(self, id)?)
    }

    fn stop(&self, id: &Id, budget: Budget) -> Result<()> {
        Backend::stop(self, flow::owned(self, id)?, budget)
    }

    fn kill(&self, id: &Id) -> Result<()> {
        Backend::kill(self, flow::owned(self, id)?)
    }

    fn status(&self, id: &Id) -> Result<Status> {
        flow::status(self, id)
    }

    fn list(&self) -> Result<Vec<Installed>> {
        flow::list(self)
    }
}
