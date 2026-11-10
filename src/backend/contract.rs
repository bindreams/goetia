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

use std::sync::Arc;

use crate::blob::Blob;
use crate::decide::ExternalConfig;
use crate::error::{Error, Result};
use crate::manager::{Budget, Installed, Sequence, ServiceManager, Status, Step, flow};
use crate::spec::{DaemonSpec, Id};

/// A marked artifact's metadata as `read` decoded it, or the failure kept in its place. Shared, so
/// every verb that reports the failure hands out the one error.
pub(crate) type Decoded = std::result::Result<Blob, Arc<Error>>;

// Backend =============================================================================================================

/// A service as it is installed, and goetia's by its marker: what [`Backend::read`] and
/// [`Backend::scan`] hand out for a marked id, and so the proof of ownership a verb is given.
pub(crate) trait InstalledService {
    fn id(&self) -> &Id;

    /// The metadata as `read` decoded it, or why it will not decode. Kept as the failure: the
    /// marker alone is proof of ownership, so `uninstall` and the other verbs must still work on a
    /// service whose blob will not decode.
    fn blob(&self) -> std::result::Result<&Blob, &Arc<Error>>;
}

/// One platform's primitives. See the module doc.
pub(crate) trait Backend {
    /// The service's definition in this manager's form. Its `Display` is the text `decide`
    /// compares and diffs.
    type Definition: std::fmt::Display;
    /// What `read` saw at a marked id; only this backend looks inside. Built only by `read` and
    /// `scan`.
    type Snapshot: InstalledService;
    /// What an install needs before it reads anything that is not a [`Self::Reservation`]. It also
    /// holds what a [`Self::write`] makes besides the definition and `install` takes back if the
    /// definition does not stand ([`Settle`]): the directories a Unix backend creates for a spec's
    /// `cwd` and `logs`.
    type InstallScope: Settle;
    /// What the verbs that send requests draw on, made by [`Self::reserve`] before anything is
    /// sent: see `bounded::Reservation`.
    type Reservation: 'static;

    // Primitives ------------------------------------------------------------------------------------------------------

    /// Everything an install needs before it reads the installed definition, held until it ends: a
    /// version gate, a lock, the check that a manager can be asked. Fails before that is read or
    /// anything is written. Run after [`Self::render_for_install`] has succeeded, so it holds or
    /// writes nothing for an install that cannot be rendered. `desired` is that render: what a
    /// backend places for the install (SCM's service host) must be what `desired` names.
    fn begin_install(&self, spec: &DaemonSpec, desired: &Self::Definition) -> Result<Self::InstallScope>;

    /// Make ready, all or none, what each of `steps` can draw under `budget`, before the first of
    /// them sends anything: a failure here means nothing was sent. See [`ServiceManager::prepare`].
    fn reserve(&self, steps: &[Step], budget: Budget) -> Result<Self::Reservation>;

    /// `spec` in this manager's form. Fails where the platform cannot express it (an account that
    /// does not resolve).
    fn render(&self, spec: &DaemonSpec) -> Result<Self::Definition>;

    /// `spec` as `render` would write it, judged against `snapshot`: what `judge`, `status` and
    /// `list` regenerate from a stored spec. A backend whose rendering depends on something that
    /// outlives the install (SCM's service host, which a provider may place elsewhere later) keeps
    /// that part from what is installed, so the comparison is of the spec alone. `install` and
    /// `preview_install` still render the desired definition, with [`Self::render_for_install`]. What it reads of the installed
    /// service must be no more than `live_status` reads, so `status` and `list` gain no failure
    /// mode; `judge` calls [`Self::installed`] first, so a failed read is never taken for a bad spec.
    /// A failed read of what is installed is [`Error::InstalledUnreadable`], which `status` and `list`
    /// report as that, not as a spec that cannot be regenerated.
    fn regenerate(&self, spec: &DaemonSpec, _snapshot: &Self::Snapshot) -> Result<Self::Definition> {
        self.render(spec)
    }

    /// [`Self::render`], for the definition an install writes and `diff` previews: also fails where
    /// the host, not the daemon, cannot carry it. `list` and `status` render every daemon with
    /// [`Self::render`], which must not fail for the host.
    fn render_for_install(&self, spec: &DaemonSpec) -> Result<Self::Definition> {
        self.render(spec)
    }

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
        reservation: &mut Self::Reservation,
    ) -> Result<WriteOutcome>;

    /// Whether the definition a [`Self::write`] of `outcome` was to leave is in place, read the way
    /// this backend reads one. Asked after every write, failed or not, and only if the write made
    /// something to keep or take back.
    fn standing(
        &self,
        _spec: &DaemonSpec,
        _definition: &Self::Definition,
        _outcome: &crate::decide::Outcome,
        _snapshot: Option<&Self::Snapshot>,
    ) -> Standing {
        Standing::Yes
    }

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
    fn uninstall(&self, snapshot: Self::Snapshot, reservation: &mut Self::Reservation) -> Result<()>;

    /// What `uninstall` of an `id` that is not installed does before it says so: a backend that keeps
    /// something of its own beyond the service itself (SCM's shim files, which an uninstall that
    /// failed to sweep leaves behind) cleans it up. A failure replaces [`Error::NotInstalled`], which
    /// the CLI renders as success, so that re-running `uninstall` is how a failed cleanup is retried.
    /// Never called for an id that is foreign or whose state is undetermined.
    fn uninstall_absent(&self, _id: &Id) -> Result<()> {
        Ok(())
    }

    /// See [`ServiceManager::enable`].
    fn enable(&self, snapshot: Self::Snapshot) -> Result<()>;

    /// See [`ServiceManager::disable`].
    fn disable(&self, snapshot: Self::Snapshot) -> Result<()>;

    /// See [`ServiceManager::start`].
    fn start(&self, snapshot: Self::Snapshot, budget: Budget, reservation: &mut Self::Reservation) -> Result<()>;

    /// The manager's own restart for [`ServiceManager::request_restart`], where it has one. `None`
    /// where it has none, so that the gate is not run for a request that cannot be made.
    fn request_restart(&self) -> Option<NativeRestart<Self>> {
        None
    }

    /// See [`ServiceManager::request_start_after_stop`].
    fn request_start_after_stop(&self, snapshot: Self::Snapshot, reservation: &mut Self::Reservation) -> Result<()>;

    /// See [`ServiceManager::stop`].
    fn stop(&self, snapshot: Self::Snapshot, budget: Budget, reservation: &mut Self::Reservation) -> Result<()>;

    /// See [`ServiceManager::kill`].
    fn kill(&self, snapshot: Self::Snapshot, reservation: &mut Self::Reservation) -> Result<()>;
}

/// Whether the definition an install wrote is in place.
#[derive(Debug)]
#[cfg_attr(
    not(unix),
    allow(dead_code, reason = "only the Unix backends write anything but the definition")
)]
pub(crate) enum Standing {
    Yes,
    No,
    /// The read did not complete: why, naming what was read.
    Unknown(String),
}

/// What a [`Backend::write`] made besides the definition, in the [`Backend::InstallScope`]. `()`
/// for a backend that makes nothing.
pub(crate) trait Settle {
    /// Keep what was made, or take it back, by whether the definition stands. `standing` is asked
    /// only if something was made. What is not kept is taken back when the scope is dropped, so a
    /// write that lost a race and a later one that failed are both undone.
    fn settle(&mut self, standing: impl FnOnce() -> Standing);
}

impl Settle for () {
    fn settle(&mut self, _standing: impl FnOnce() -> Standing) {}
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
    pub blob: Decoded,
    /// What only this backend looks at: the snapshot minus its id and blob.
    pub body: B,
}

impl<B> ReadOutcome<Unnamed<B>> {
    /// The outcome at `id`, with a marked artifact made into the snapshot `make` builds.
    pub(crate) fn named<S>(self, id: &Id, make: impl FnOnce(Id, Decoded, B) -> S) -> ReadOutcome<S> {
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
    Misnamed { blob: Decoded, name: Arc<Error> },
}

impl<S> ScanEntry<S> {
    /// `found` at `name`: named an id where it is one, and otherwise [`Self::Misnamed`] if it is
    /// marked.
    pub(crate) fn of<B>(name: &str, found: ReadOutcome<Unnamed<B>>, make: impl FnOnce(Id, Decoded, B) -> S) -> Self {
        match (Id::try_from(name), found) {
            (Ok(id), found) => Self::Read(found.named(&id, make)),
            (Err(name), ReadOutcome::Marked(Unnamed { blob, .. })) => Self::Misnamed {
                blob,
                name: Arc::new(name),
            },
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
        let snapshot = match flow::owned(self, id) {
            Err(absent @ Error::NotInstalled { .. }) => {
                return Err(Backend::uninstall_absent(self, id).err().unwrap_or(absent));
            }
            owned => owned?,
        };
        // `uninstall` has no `--timeout` of its own: its stop is unbounded.
        let mut reservation = flow::reserved(self, id, Step::Uninstall, Budget::Unbounded)?;
        Backend::uninstall(self, snapshot, &mut reservation)
    }

    fn enable(&self, id: &Id) -> Result<()> {
        Backend::enable(self, flow::owned(self, id)?)
    }

    fn disable(&self, id: &Id) -> Result<()> {
        Backend::disable(self, flow::owned(self, id)?)
    }

    fn start(&self, id: &Id, budget: Budget) -> Result<()> {
        let snapshot = flow::owned(self, id)?;
        let mut reservation = flow::reserved(self, id, Step::Start, budget)?;
        Backend::start(self, snapshot, budget, &mut reservation)
    }

    fn prepare(&self, steps: &[Step], budget: Budget) -> Result<Box<dyn Sequence + '_>> {
        Ok(Box::new(Reserved {
            backend: self,
            reservation: Backend::reserve(self, steps, budget)?,
            unspent: steps.to_vec(),
        }))
    }

    fn request_restart(&self, id: &Id) -> Option<Result<()>> {
        let restart = Backend::request_restart(self)?;
        Some(flow::owned(self, id).and_then(|snapshot| restart(self, snapshot)))
    }

    fn request_start_after_stop(&self, id: &Id) -> Result<()> {
        let snapshot = flow::owned(self, id)?;
        let mut reservation = flow::reserved(self, id, Step::RequestStartAfterStop, Budget::Immediate)?;
        Backend::request_start_after_stop(self, snapshot, &mut reservation)
    }

    fn stop(&self, id: &Id, budget: Budget) -> Result<()> {
        let snapshot = flow::owned(self, id)?;
        let mut reservation = flow::reserved(self, id, Step::Stop, budget)?;
        Backend::stop(self, snapshot, budget, &mut reservation)
    }

    fn kill(&self, id: &Id) -> Result<()> {
        let snapshot = flow::owned(self, id)?;
        let mut reservation = flow::reserved(self, id, Step::Kill, Budget::Unbounded)?;
        Backend::kill(self, snapshot, &mut reservation)
    }

    fn status(&self, id: &Id) -> Result<Status> {
        flow::status(self, id)
    }

    fn list(&self) -> Result<Vec<Installed>> {
        flow::list(self)
    }
}

// Sequence ============================================================================================================

/// [`ServiceManager::prepare`]'s sequence over a backend: the verbs the ownership gate lets through,
/// each drawing on the reservation made for the sequence.
struct Reserved<'a, B: Backend> {
    backend: &'a B,
    reservation: B::Reservation,
    /// The prepared steps no verb has used yet.
    unspent: Vec<Step>,
}

impl<B: Backend> Reserved<'_, B> {
    /// Use up one of `steps`, the first that is still unspent: see [`Sequence`].
    fn spend(&mut self, steps: &[Step]) {
        let found = steps
            .iter()
            .find_map(|step| self.unspent.iter().position(|unspent| unspent == step));
        debug_assert!(
            found.is_some(),
            "{steps:?} was not prepared for, or already ran: it would draw on a reservation not counted for it"
        );
        if let Some(found) = found {
            self.unspent.swap_remove(found);
        }
    }
}

impl<B: Backend> Sequence for Reserved<'_, B> {
    fn stop(&mut self, id: &Id, budget: Budget) -> Result<()> {
        let snapshot = flow::owned(self.backend, id)?;
        self.spend(&[Step::Stop]);
        Backend::stop(self.backend, snapshot, budget, &mut self.reservation)
    }

    fn start(&mut self, id: &Id, budget: Budget) -> Result<()> {
        let snapshot = flow::owned(self.backend, id)?;
        self.spend(&[Step::Start]);
        Backend::start(self.backend, snapshot, budget, &mut self.reservation)
    }

    /// Spends [`Step::RequestStartAfterStop`] or, failing that, [`Step::Start`], which counts the
    /// longest start: `restart` prepares `Start` and runs this in its place under a budget that does
    /// not wait.
    fn request_start_after_stop(&mut self, id: &Id) -> Result<()> {
        let snapshot = flow::owned(self.backend, id)?;
        self.spend(&[Step::RequestStartAfterStop, Step::Start]);
        Backend::request_start_after_stop(self.backend, snapshot, &mut self.reservation)
    }
}
