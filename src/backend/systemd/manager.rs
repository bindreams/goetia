//! The effectful half of the systemd backend: writes `/etc/systemd/system/<id>.service` (and, for a
//! task with a schedule, `<id>.timer`) and talks to `systemctl`, `busctl` and `journalctl`.
//!
//! A service is one artifact. A task is two, kept as one by the `task` module; the obligations below are
//! stated for the service, and hold for each of a task's two files (`write::create_unit` and the other
//! write helpers take the kind of unit, and `discover` reads both kinds' drop-ins and links).
//!
//! Seven correctness obligations a systemd unit-file backend must uphold, each with its own test (see
//! `tests/systemd_integration/linux.rs` and this module's own `manager_tests.rs`):
//!
//! 1. **A write must not clobber something it did not classify.** `rename(2)` unconditionally
//!    replaces its destination, so a plain classify-then-write is a TOCTOU window: a package
//!    postinst or a concurrent invocation can drop a foreign unit into the gap and have it destroyed
//!    by the very code path that exists to refuse that. `write::create_unit` uses
//!    [`tempfile::NamedTempFile::persist_noclobber`] (`linkat(2)` without replace semantics) for the
//!    create case; `write::replace_unit_verified` closes the same gap for the update/regenerate
//!    case by quarantining the current occupant under a temporary name and verifying its identity
//!    before committing to either direction.
//! 2. **A masked unit is not absent.** `systemctl mask` replaces the fragment with a symlink to
//!    `/dev/null`; reading it yields empty text, and a naive read-then-extract would see `Ok(None)`
//!    and let `install` write over it, silently unmasking a deliberately-masked service.
//!    `discover::classify_and_read` opens the path `O_PATH | O_NOFOLLOW` and classifies the
//!    descriptor by `fstat`, reporting any non-regular file as `discover::RawState::NotOurs` —
//!    always `Ownership::Foreign` — without ever opening it for reading. It answers the same for a
//!    regular file whose bytes turn out not to be UTF-8, which is the same claim by a different
//!    proof: goetia writes UTF-8 ini and nothing else. Only a read that *did not complete* is
//!    neither, and that is [`Error::Undetermined`] for `status` and [`Installed::Undetermined`] for
//!    `list`, since nothing at all about the artifact was established.
//! 3. **Drop-ins are drift.** `systemctl edit` — the officially recommended way to add exactly the
//!    `MemoryMax=`/`After=` the design cites — writes `<id>.service.d/override.conf` and leaves the
//!    fragment itself byte-identical, so drift detection over the fragment alone misses it entirely.
//!    `discover::installed` folds `<id>.service.d`'s `*.conf` contents — across every root of
//!    systemd's system unit search path, `/etc/systemd/system.control` included — into the text
//!    handed to `decide` (never into what is actually written); `decide::decide`'s own
//!    `external` parameter — never a backend-local override of its `Outcome` — closes the one
//!    branch the folded text can't reach on its own (a stale artifact, whose version-mismatch check
//!    fires before any text comparison at all). Every successful write clears goetia's own
//!    `UNIT_DIR/<id>.service.d`, so a resolved conflict there cannot wedge the id in permanent
//!    drift; a drop-in under any other search root is reported but never removed, since goetia
//!    cannot write it and cannot tell an administrator's override from a leftover. A drop-in
//!    directory with no fragment at all is refused rather than silently adopted as `Create`. The
//!    family-wide directories `systemd.unit(5)` also reads (`my-.service.d`, the top-level
//!    `service.d`) are deliberately outside this — see `discover`'s module doc comment.
//! 4. **Permissions.** `NamedTempFile` is created mode 0600; after persisting, the unit would be
//!    root-only, breaking the promise that `list`/`show`/`diff` need no elevation.
//!    `write::write_temp_unit` `chmod`s 0644 before persisting.
//! 5. **Parent directories.** `dirs::ensure_parent_dirs` creates and, for a non-root account,
//!    `chown`s the parents of `logs` and `cwd` while still elevated. A missing log directory fails
//!    the unit at start (`ENOENT` opening `StandardOutput=append:`), and a missing `cwd` fails it
//!    with `200/CHDIR`. Only path components this call actually creates are touched; an
//!    already-existing directory is not checked, because install is not a start. systemd opens the
//!    log file as root before it drops privileges, so an existing log directory is never a problem
//!    for the account; it `chdir`s into `cwd` after the switch, so only a `cwd` the account cannot
//!    traverse fails at start.
//! 6. **Uninstall order** is stop -> `systemctl disable` -> remove -> `daemon-reload`. Disabling
//!    after the fragment is gone is impossible (no `[Install]` section left to read), which would
//!    leave exactly the `.wants` symlink `uninstall_leaves_nothing` checks for. A task's order puts
//!    its timer first, so that nothing starts the job again: see the `task` module.
//! 7. **Absence is about the id, not about the fragment file.** `cli::uninstall` maps
//!    [`Error::NotInstalled`] — and only that variant — to exit `0` and "nothing to do", so a
//!    backend reporting it off `<id>.service`'s absence alone would certify "confirmed gone" for an
//!    id systemd still applies a drop-in to, or still enrolls at boot through a
//!    `multi-user.target.wants` link. `discover::residue` is the one predicate behind
//!    `discover::fragmentless`, which every read of an id with no fragment asks — `install`'s and
//!    every other verb's through `read`, so no two verbs can describe one filesystem state
//!    differently, and `list`'s through `scan`, for every id it enumerates with no readable
//!    fragment. A listing that asked less would leave out an id `status` answers
//!    `Error::Undetermined` for, and leaving an id out is how a listing says nothing is there.

mod busctl;
mod dirs;
mod discover;
mod journal;
mod systemctl;
mod task;
mod write;

use std::collections::BTreeSet;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use busctl::Busctl;
use discover::DROPIN_SEARCH_DIRS;
use journal::{Journal, Journalctl};
use systemctl::{Held, Systemctl};
pub(crate) use task::Fragment;

use crate::backend::systemd::generate::{self, Units};
use crate::backend::{
    Backend, Identity, InstalledDefinition, InstalledService, NativeRestart, ReadOutcome, Scan, ScanEntry,
    WriteOutcome, cannot_run_tasks,
};
use crate::decide::Outcome;
use crate::error::{Error, Reason, Result};
use crate::manager::{Budget, Installed, Status, Step, flow};
use crate::spec::{AccountId, DaemonKind, DaemonSpec, Id, User};

/// Where systemd looks for system unit files. Never overridden — the integration tests run for real
/// against this exact path, elevated.
const UNIT_DIR: &str = "/etc/systemd/system";

// Systemd =============================================================================================================

/// The Linux [`ServiceManager`](crate::manager::ServiceManager). Holds no state of its own beyond
/// how it reaches `systemctl`, `busctl` and the journal: every operation re-derives what it needs from
/// the filesystem and the manager, so nothing here can go stale between calls.
pub struct Systemd {
    systemctl: Systemctl,
    /// For when a task's timer elapses next, which `systemctl show` cannot say to the microsecond.
    busctl: Busctl,
    /// For a task's last run and its result.
    journal: Box<dyn Journal>,
}

impl Systemd {
    pub fn new() -> Self {
        Self::with(Systemctl::native())
    }

    /// A manager reaching `systemd` through `systemctl`, which tests build over a stated client, and
    /// the real `busctl` and journal.
    pub(crate) fn with(systemctl: Systemctl) -> Self {
        Self {
            systemctl,
            busctl: Busctl::native(),
            journal: Box::new(Journalctl::native()),
        }
    }

    /// A manager over stated `systemctl`, `busctl` and journal.
    #[cfg(test)]
    fn with_seams(systemctl: Systemctl, busctl: Busctl, journal: Box<dyn Journal>) -> Self {
        Self {
            systemctl,
            busctl,
            journal,
        }
    }
}

impl Default for Systemd {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Debug for Systemd {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Systemd").finish_non_exhaustive()
    }
}

impl Backend for Systemd {
    type Definition = Units;
    type Snapshot = Fragment;
    type InstallScope = ();
    type Reservation = Held;
    type Run = std::convert::Infallible;

    /// Before anything is read or written: the gate is where no manager to ask refuses too. Its
    /// answer holds for the `daemon-reload` after the write, which draws on the same reservation.
    fn begin_install(&self, _spec: &DaemonSpec, _desired: &Units, reservation: &mut Held) -> Result<()> {
        self.systemctl.require_supported(reservation)
    }

    /// The listener threads each watched start or stop in `steps` reads its streams with, made all
    /// or none: see [`Held::of`]. `install` and unwatched budgets need none.
    fn reserve(&self, steps: &[Step], budget: Budget) -> Result<Held> {
        Held::of(steps, budget).map_err(|source| Error::Reservation { id: None, source })
    }

    fn render(&self, spec: &DaemonSpec) -> Result<Units> {
        generate::units(spec, &identity_for(&spec.common().user)?)
    }

    /// The units as `snapshot` holds them, unmodified: see [`task::regenerate`].
    fn regenerate(&self, spec: &DaemonSpec, snapshot: &Fragment) -> Result<Units> {
        task::regenerate(spec, snapshot)
    }

    fn read(&self, id: &Id) -> Result<ReadOutcome<Fragment>> {
        discover::read(id)
    }

    fn installed(&self, snapshot: &Fragment) -> Result<InstalledDefinition<Units>> {
        discover::installed(snapshot)
    }

    fn write(
        &self,
        spec: &DaemonSpec,
        definition: &Units,
        outcome: &Outcome,
        snapshot: Option<&Fragment>,
        _scope: &mut (),
        reservation: &mut Held,
    ) -> Result<WriteOutcome> {
        dirs::ensure_parent_dirs(spec)?;
        match outcome {
            Outcome::Create => self.create_units(spec.common().id.as_str(), definition, reservation),
            Outcome::Update { .. } | Outcome::Stale { .. } => {
                let old = snapshot.expect("an id `decide` chose to update was read as marked");
                self.update_units(spec, definition, old, reservation)
            }
            other => unreachable!("`flow::install` writes only `Create`, `Update` and `Stale`, got {other:?}"),
        }
    }

    fn live_status(&self, snapshot: &Fragment) -> Result<Status> {
        // A read: it draws nothing and asks no gate.
        self.status_of(snapshot)
    }

    /// Each id the unit directories name, read as `read` would. An id whose read did not complete
    /// establishes nothing about it — least of all that it is absent, which is what omitting it from
    /// the enumeration would say — so it is reported by name instead.
    fn scan(&self) -> Scan<Fragment> {
        let host = scan_host();
        let reads = host
            .units
            .into_iter()
            // Absence is about the id, not the fragment file: an id with no job (a timer alone, or a
            // *drop-in* that could not be read) is exactly as unclassified as one whose job could not
            // be — and `status` says so for both.
            .chain(host.unjobbed.into_iter().map(|id| {
                let path = unit_path(&id, UnitKind::Service);
                (id, path)
            }))
            .map(|(id, path)| {
                let read = discover::read_at(&id, &path).map(|found| ScanEntry::of(&id, found, Fragment::new));
                (id, read)
            });

        let mut entries = Vec::new();
        let mut reported = Vec::new();
        for (id, read) in reads {
            match read {
                Ok(read) => entries.push((id, read)),
                Err(failure) => reported.push(Installed::Undetermined {
                    name: Some(id),
                    reason: Reason::failed(failure.into_os_error()),
                }),
            }
        }
        // Last, so a scan that got some way in still reports what it named before what it could
        // not — the order `cli::support::partition_installed` imposes on the rendered output too.
        reported.extend(host.incomplete);
        Scan { entries, reported }
    }

    /// A task, or a service with a timer of goetia's: the timer disarmed, the job stopped, the timer
    /// then the job removed. A plain service: stop, disable, remove, reload. See [`task`].
    fn uninstall(&self, snapshot: Fragment, reservation: &mut Held) -> Result<()> {
        self.uninstall_units(snapshot, reservation)
    }

    // `disable` is handed none and asks the gate for itself.
    fn enable(&self, snapshot: Fragment, reservation: &mut Held) -> Result<()> {
        self.enable_units(&snapshot, reservation)
    }

    fn disable(&self, snapshot: Fragment) -> Result<()> {
        self.disable_units(&snapshot)
    }

    fn start(&self, snapshot: Fragment, budget: Budget, reservation: &mut Held) -> Result<()> {
        let id = snapshot.id().as_str();
        // A task's start requests a run and returns: the run's end is not what a start waits for.
        if snapshot.kind() == Some(DaemonKind::Task) {
            return self.systemctl.start_job(id, reservation).map(drop);
        }
        // Derived at verb entry. It bounds only the wait for the job, never whether the job is
        // enqueued — see `run_verb`.
        let deadline = budget.start();
        self.systemctl.start_impl(id, budget, deadline, reservation)
    }

    /// One `systemctl restart --no-block`: one job, so no start can overtake
    /// the stop. systemd stops the unit and starts it again, or, while it is
    /// still activating, folds the restart into the start already running —
    /// see `request_restart_impl`. A task's is the same request, of its job.
    fn request_restart(&self) -> Option<NativeRestart<Self>> {
        Some(|mgr, snapshot| {
            mgr.systemctl
                .request_restart_impl(snapshot.id().as_str(), &mut Held::default())
        })
    }

    /// The plain request-only start. `restart` never issues it here — see
    /// [`Self::request_restart`] — since after a stop nobody waited for, it
    /// replaces a stop job systemd has not yet run.
    fn request_start_after_stop(&self, snapshot: Fragment, reservation: &mut Held) -> Result<()> {
        Backend::start(self, snapshot, Budget::Immediate, reservation)
    }

    /// A task's job is its service unit, so its stop and kill are a service's.
    fn stop(&self, snapshot: Fragment, budget: Budget, reservation: &mut Held) -> Result<()> {
        // Derived at verb entry, as for `start`.
        let deadline = budget.start();
        self.systemctl
            .stop_impl(snapshot.id().as_str(), budget, deadline, reservation)
    }

    fn kill(&self, snapshot: Fragment, reservation: &mut Held) -> Result<()> {
        self.systemctl.kill_impl(snapshot.id().as_str(), reservation)
    }

    /// Interim: replaced when the run is built on the journal.
    fn begin_run(&self, snapshot: Fragment, _reservation: &mut Held) -> Result<std::convert::Infallible> {
        Err(cannot_run_tasks("systemd", snapshot.id()))
    }
}

// Enumeration =========================================================================================================

/// Every id `Systemd::list` has to answer for, and the entries standing for whatever the passes
/// that produced it never reached.
///
/// # Which ids those are
///
/// The names `discover::read` and `discover::residue` treat as this id's artifact, in the same
/// directories they read: `<id>.service` and `<id>.timer` in [`UNIT_DIR`] alone, the only fragment
/// paths this backend ever reads or writes, and `<id>.service.d` and `<id>.timer.d` under every root
/// of [`DROPIN_SEARCH_DIRS`], since `discover::residue` counts one there as much as one in `/etc`.
///
/// Reaching exactly as far as `residue` is the point. An id whose drop-in could not be read is
/// `Error::Undetermined` to `status`; a listing that never enumerated it says, by leaving it out,
/// that nothing is installed there — the negative conclusion [`Installed::Undetermined`] exists to
/// forbid. That still bounds the scan well short of the unit load path: only directory *names* come
/// out of each root, and only the ids they name are asked about.
///
/// A fragment under a root other than `UNIT_DIR` deliberately names no id here. `raw_state` looks
/// for `<id>.service` in `UNIT_DIR` and nowhere else, so a unit shipped in `/usr/lib` is already
/// `NotInstalled` to every verb goetia has. A timer alone at an id (a task whose job is gone) names
/// it, and `read` classifies it: ours and unusable if it is goetia's, foreign if it is not.
///
/// # The half of `residue` this does not name: enablement links
///
/// `residue` also stats `multi-user.target.wants/<id>.service` and `timers.target.wants/<id>.timer`
/// under four roots each, and an id whose
/// *only* trace is such a link is not named here. A stated limitation: an unsearchable wants
/// directory makes `status` answer `Error::Undetermined` for every fragmentless id, this scan has
/// no name for one whose sole trace was a link there, and `show` would call that id absent.
///
/// Both ways of closing it are worse than the hole. A readability probe of each wants directory
/// answers a *different* question than `residue` asks — mode `0111` denies `read_dir` while every
/// stat `residue` performs succeeds — so it would stand a permanent aggregate entry, and a
/// permanent exit `4`, on a host where nothing goetia reads fails. Enumerating their contents
/// instead asks `residue` about every unit enabled on the host, all foreign by construction, which
/// is the sweep this bound exists to avoid. The drop-in half needs neither: a handful of directories
/// name themselves in their own parent's listing, rather than one per enabled unit.
#[derive(Debug)]
struct HostScan {
    /// Ids with a fragment in [`UNIT_DIR`], with its path.
    units: Vec<(String, PathBuf)>,
    /// Ids with no job in [`UNIT_DIR`] that a `<id>.timer` there, or a `<id>.service.d` or
    /// `<id>.timer.d` directory anywhere, names. Disjoint from `units` by construction: an id with a
    /// job is classified through that, and one entry per id is what
    /// `cli::support::partition_installed` asserts.
    unjobbed: Vec<String>,
    /// One per pass that started and did not finish — a root each, since a root that could not be
    /// enumerated says nothing about the next one.
    incomplete: Vec<Installed>,
}

/// The enumeration behind [`HostScan`], one `read_dir` per root.
fn scan_host() -> HostScan {
    let unit_dir = scan_unit_dir(Path::new(UNIT_DIR));
    let mut incomplete: Vec<Installed> = unit_dir.incomplete.into_iter().collect();
    let mut named: BTreeSet<String> = unit_dir.dropins.into_iter().chain(unit_dir.timers).collect();
    for root in DROPIN_SEARCH_DIRS.iter().filter(|root| **root != UNIT_DIR) {
        let scan = scan_unit_dir(Path::new(root));
        // Fragments outside `UNIT_DIR` are not this backend's — see [`HostScan`].
        named.extend(scan.dropins);
        incomplete.extend(scan.incomplete);
    }
    let unjobbed = {
        let with_fragment: BTreeSet<&str> = unit_dir.units.iter().map(|(id, _)| id.as_str()).collect();
        named
            .into_iter()
            .filter(|id| !with_fragment.contains(id.as_str()))
            .collect()
    };
    HostScan {
        units: unit_dir.units,
        unjobbed,
        incomplete,
    }
}

/// What one pass over one directory found: the `<id>.service` fragments, the ids of the `<id>.timer`
/// fragments, and the ids of the `<id>.service.d` and `<id>.timer.d` drop-in directories it reached,
/// and — when the pass stopped early — the entry standing for whatever it never did.
#[derive(Debug)]
struct UnitScan {
    units: Vec<(String, PathBuf)>,
    timers: Vec<String>,
    dropins: Vec<String>,
    incomplete: Option<Installed>,
}

/// Enumerate `dir`. A pass that cannot start, or cannot finish, is *reported* rather than
/// propagated: an `Err` out of `list` would throw away every id this same call already classified
/// and reach the CLI as an empty document on exit `1`, which is `list` saying the host has no
/// daemons — the one claim a scan that did not finish cannot support. `dir` not existing is neither:
/// absence is *established* there. See [`flow::scan`].
fn scan_unit_dir(dir: &Path) -> UnitScan {
    let mut units = Vec::new();
    let mut timers = Vec::new();
    let mut dropins = Vec::new();
    let incomplete = flow::scan(
        &dir.display().to_string(),
        fs::read_dir(dir),
        |entries| Box::new(entries.map(|entry| entry.map(|entry| entry.path()))),
        flow::directory_detail,
        |path: PathBuf| {
            // Lossy: a non-UTF-8 unit name still has to be reported, and every read goes through
            // `path` itself rather than through this rendering of it.
            let Some(file_name) = path.file_name().map(|name| name.to_string_lossy().into_owned()) else {
                return;
            };
            // The name alone, with nothing stat'd: a drop-in directory that is not one — the regular
            // file `install_refuses_an_unreadable_dropin_over_our_own_fragment` seeds, whose
            // `read_dir` answers `ENOTDIR` for every uid — is exactly an id this pass must not drop.
            if let Some(id) = UnitKind::ALL
                .into_iter()
                .find_map(|kind| file_name.strip_suffix(&format!("{}.d", kind.suffix())))
            {
                dropins.push(id.to_string());
            } else if let Some(id) = file_name.strip_suffix(UnitKind::Service.suffix()) {
                units.push((id.to_string(), path));
            } else if let Some(id) = file_name.strip_suffix(UnitKind::Timer.suffix()) {
                timers.push(id.to_string());
            }
        },
    );
    UnitScan {
        units,
        timers,
        dropins,
        incomplete,
    }
}

// Identity ============================================================================================================

/// Resolve `user` to the account name systemd's `User=` directive wants. Root becomes `"0"`, exactly
/// as `cli::install`'s dry-run preview renders it, so a preview and a real install never disagree.
/// Unlike launchd/SCM, systemd needs no OS lookup here — it resolves usernames itself via NSS at
/// start time — so this is otherwise a pure formatting step. The one case rejected outright is a
/// Windows account SID: `spec::resolve` only checks it for unemittable characters, never for
/// platform applicability, so a manifest naming `user: {id: "S-1-5-..."}` would otherwise reach
/// `generate::units` and be written verbatim into `User=`, failing the unit at start with an opaque
/// status — exactly what `ensure_parent_dirs` goes out of its way to avoid for directory permissions.
fn identity_for(user: &User) -> Result<Identity> {
    let name = match user {
        User::Root => "0".to_string(),
        User::Name(name) => name.clone(),
        User::Id(AccountId::Uid(uid)) => uid.to_string(),
        User::Id(AccountId::Sid(_)) => {
            return Err(Error::AccountLookup {
                detail: "a Windows account id (SID) is not a valid user for the systemd backend".to_string(),
                source: None,
            });
        }
    };
    Ok(Identity { user: name })
}

// Paths ===============================================================================================================

/// Which of a daemon's unit files: a service is its job alone, and a task is its job
/// (`<id>.service`) and, with a schedule, its timer (`<id>.timer`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum UnitKind {
    Service,
    Timer,
}

impl UnitKind {
    const ALL: [UnitKind; 2] = [UnitKind::Service, UnitKind::Timer];

    fn suffix(self) -> &'static str {
        match self {
            UnitKind::Service => ".service",
            UnitKind::Timer => ".timer",
        }
    }

    /// The one `*.target.wants` directory `systemctl enable` links a unit of this kind into:
    /// `generate::units`'s `[Install]` sections are exactly `WantedBy=multi-user.target` for a service
    /// and `WantedBy=timers.target` for a timer, and goetia emits no `RequiredBy=`, so there is no
    /// `.requires` counterpart.
    fn wants_dir(self) -> &'static str {
        match self {
            UnitKind::Service => "multi-user.target.wants",
            UnitKind::Timer => "timers.target.wants",
        }
    }
}

fn unit_name(id: &str, kind: UnitKind) -> String {
    format!("{id}{}", kind.suffix())
}

fn unit_path(id: &str, kind: UnitKind) -> PathBuf {
    Path::new(UNIT_DIR).join(unit_name(id, kind))
}

/// The name of `kind`'s drop-in directory for `id`, under any root of [`DROPIN_SEARCH_DIRS`].
fn dropin_name(id: &str, kind: UnitKind) -> String {
    format!("{}.d", unit_name(id, kind))
}

fn dropin_dir(id: &str, kind: UnitKind) -> PathBuf {
    Path::new(UNIT_DIR).join(dropin_name(id, kind))
}

// Shared removal helpers ==============================================================================================

fn remove_file_if_present(path: &Path) -> Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(Error::os_at("remove", path, e)),
    }
}

fn remove_dir_if_present(path: &Path) -> Result<()> {
    match fs::remove_dir_all(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(Error::os_at("remove", path, e)),
    }
}

#[cfg(test)]
#[path = "manager_tests.rs"]
mod manager_tests;
