//! The effectful half of the systemd backend: writes `/etc/systemd/system/<id>.service` and talks
//! to `systemctl`.
//!
//! Seven correctness obligations a systemd unit-file backend must uphold, each with its own test (see
//! `tests/systemd_integration/linux.rs` and this module's own `manager_tests.rs`):
//!
//! 1. **A write must not clobber something it did not classify.** `rename(2)` unconditionally
//!    replaces its destination, so a plain classify-then-write is a TOCTOU window: a package
//!    postinst or a concurrent invocation can drop a foreign unit into the gap and have it destroyed
//!    by the very code path that exists to refuse that. [`write::create_unit`] uses
//!    [`tempfile::NamedTempFile::persist_noclobber`] (`linkat(2)` without replace semantics) for the
//!    create case; [`write::replace_unit_verified`] closes the same gap for the update/regenerate
//!    case by quarantining the current occupant under a temporary name and verifying its identity
//!    before committing to either direction.
//! 2. **A masked unit is not absent.** `systemctl mask` replaces the fragment with a symlink to
//!    `/dev/null`; reading it yields empty text, and a naive read-then-extract would see `Ok(None)`
//!    and let `install` write over it, silently unmasking a deliberately-masked service.
//!    [`discover::classify_and_read`] opens the path `O_PATH | O_NOFOLLOW` and classifies the
//!    descriptor by `fstat`, reporting any non-regular file as [`discover::RawState::NotOurs`] —
//!    always `Ownership::Foreign` — without ever opening it for reading. It answers the same for a
//!    regular file whose bytes turn out not to be UTF-8, which is the same claim by a different
//!    proof: goetia writes UTF-8 ini and nothing else. Only a read that *did not complete* is
//!    neither, and that is [`Error::Undetermined`] for `status` and [`Installed::Undetermined`] for
//!    `list`, since nothing at all about the artifact was established.
//! 3. **Drop-ins are drift.** `systemctl edit` — the officially recommended way to add exactly the
//!    `MemoryMax=`/`After=` the design cites — writes `<id>.service.d/override.conf` and leaves the
//!    fragment itself byte-identical, so drift detection over the fragment alone misses it entirely.
//!    [`discover::installed`] folds `<id>.service.d`'s `*.conf` contents — across every root of
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
//!    [`write::write_temp_unit`] `chmod`s 0644 before persisting.
//! 5. **Parent directories.** [`dirs::ensure_parent_dirs`] creates and, for a non-root account,
//!    `chown`s the parents of `logs` and `cwd` while still elevated. A missing log directory fails
//!    the unit at start (`ENOENT` opening `StandardOutput=append:`), and a missing `cwd` fails it
//!    with `200/CHDIR`. Only path components this call actually creates are touched; an
//!    already-existing directory is not checked, because install is not a start. systemd opens the
//!    log file as root before it drops privileges, so an existing log directory is never a problem
//!    for the account; it `chdir`s into `cwd` after the switch, so only a `cwd` the account cannot
//!    traverse fails at start.
//! 6. **Uninstall order** is stop -> `systemctl disable` -> remove -> `daemon-reload`. Disabling
//!    after the fragment is gone is impossible (no `[Install]` section left to read), which would
//!    leave exactly the `.wants` symlink `uninstall_leaves_nothing` checks for.
//! 7. **Absence is about the id, not about the fragment file.** `cli::uninstall` maps
//!    [`Error::NotInstalled`] — and only that variant — to exit `0` and "nothing to do", so a
//!    backend reporting it off `<id>.service`'s absence alone would certify "confirmed gone" for an
//!    id systemd still applies a drop-in to, or still enrolls at boot through a
//!    `multi-user.target.wants` link. `discover::residue` is the one predicate behind
//!    [`discover::fragmentless`], which every read of an id with no fragment asks — `install`'s and
//!    every other verb's through `read`, so no two verbs can describe one filesystem state
//!    differently, and `list`'s through `scan`, for every id it enumerates with no readable
//!    fragment. A listing that asked less would leave out an id `status` answers
//!    `Error::Undetermined` for, and leaving an id out is how a listing says nothing is there.

mod dirs;
mod discover;
mod systemctl;
mod write;

use std::collections::BTreeSet;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use discover::DROPIN_SEARCH_DIRS;
use systemctl::{
    GateScope, daemon_reload, daemon_reload_or_report, disable_impl, enable_impl, gate_scope, kill_impl,
    request_restart_impl, require_supported, start_impl, status_from_unit, stop_impl,
};
use write::{CreateOutcome, ReplaceOutcome, create_unit, quarantine_if_still_ours, replace_unit_verified};

use crate::backend::systemd::generate;
use crate::backend::{Backend, Identity, InstalledDefinition, ReadOutcome, Scan, WriteOutcome, bounded};
use crate::decide::Outcome;
use crate::error::{Error, Result};
use crate::manager::budget::Deadline;
use crate::manager::{Budget, Installed, Prepared, Status, Step, flow};
use crate::spec::{AccountId, DaemonSpec, Id, User};

/// Where systemd looks for system unit files. Never overridden — the integration tests run for real
/// against this exact path, elevated.
const UNIT_DIR: &str = "/etc/systemd/system";

// Systemd =============================================================================================================

/// The Linux [`ServiceManager`](crate::manager::ServiceManager). Holds no state of its own: every
/// operation re-derives what it needs from the filesystem and `systemctl`, so nothing here can go
/// stale between calls.
#[derive(Debug, Default)]
pub struct Systemd;

impl Systemd {
    pub fn new() -> Self {
        Self
    }
}

impl Backend for Systemd {
    type Definition = String;
    /// The fragment's own text, which `replace_unit_verified` and `quarantine_if_still_ours` verify
    /// the fragment against before they touch it.
    type Snapshot = String;
    type InstallScope = GateScope;

    /// Before anything is read or written: the gate is where no manager to ask refuses too. Its
    /// answer holds for the `daemon-reload` after the write.
    fn begin_install(&self, _spec: &DaemonSpec) -> Result<GateScope> {
        let gate = gate_scope();
        require_supported()?;
        Ok(gate)
    }

    fn render(&self, spec: &DaemonSpec) -> Result<String> {
        Ok(generate::unit(spec, &identity_for(&spec.user)?))
    }

    fn read(&self, id: &Id) -> Result<ReadOutcome<String>> {
        discover::read(id.as_str())
    }

    fn installed(&self, id: &Id, snapshot: &String) -> Result<InstalledDefinition<String>> {
        discover::installed(id.as_str(), snapshot)
    }

    fn write(
        &self,
        spec: &DaemonSpec,
        definition: &String,
        outcome: &Outcome,
        snapshot: Option<&String>,
        _scope: &mut GateScope,
    ) -> Result<WriteOutcome> {
        let id = spec.id.as_str();
        dirs::ensure_parent_dirs(spec)?;
        match outcome {
            Outcome::Create => match create_unit(id, definition)? {
                CreateOutcome::Created => {
                    daemon_reload_or_report(id)?;
                    Ok(WriteOutcome::Done)
                }
                CreateOutcome::Raced => Ok(WriteOutcome::Raced),
            },
            Outcome::Update { .. } | Outcome::Stale { .. } => {
                let expected_text = snapshot.expect("an id `decide` chose to update was read as marked");
                match replace_unit_verified(id, definition, expected_text)? {
                    ReplaceOutcome::Replaced => {
                        // Only after the write has actually committed: clearing the drop-in
                        // first (before the write) would delete an admin's overrides even on a
                        // path that turns out `Raced` and never writes anything, or errors out
                        // partway through — `uninstall` clears its own fragment before its
                        // drop-in for the identical reason.
                        remove_dir_if_present(&dropin_dir(id))?;
                        daemon_reload_or_report(id)?;
                        Ok(WriteOutcome::Done)
                    }
                    ReplaceOutcome::Raced => Ok(WriteOutcome::Raced),
                }
            }
            other => unreachable!("`flow::install` writes only `Create`, `Update` and `Stale`, got {other:?}"),
        }
    }

    fn live_status(&self, id: &Id, _snapshot: &String) -> Result<Status> {
        status_from_unit(&unit_name(id.as_str()))
    }

    /// Each id the unit directories name, read as `read` would. An id whose read did not complete
    /// establishes nothing about it — least of all that it is absent, which is what omitting it from
    /// the enumeration would say — so it is reported by name instead.
    fn scan(&self) -> Scan<String> {
        let host = scan_host();
        let reads = host
            .units
            .into_iter()
            .map(|(id, path)| {
                let read = discover::read_at(&id, &path);
                (id, read)
            })
            // Obligation 7 again: what occupies an id is not the fragment file, so an id whose
            // *drop-in* could not be read is exactly as unclassified as one whose fragment could not
            // be — and `status` says so for both.
            .chain(host.dropin_only.into_iter().map(|id| {
                let read = discover::fragmentless(&id);
                (id, read)
            }));

        let mut entries = Vec::new();
        let mut reported = Vec::new();
        for (id, read) in reads {
            match read {
                Ok(read) => entries.push((id, read)),
                Err(failure) => reported.push(Installed::Undetermined {
                    name: Some(id),
                    reason: failure.detail(),
                }),
            }
        }
        // Last, so a scan that got some way in still reports what it named before what it could
        // not — the order `cli::support::partition_installed` imposes on the rendered output too.
        reported.extend(host.incomplete);
        Scan { entries, reported }
    }

    fn uninstall(&self, id: &Id) -> Result<()> {
        let expected_text = flow::owned(self, id)?;
        let id = id.as_str();
        // One version gate for the stop, the `disable` and the `daemon-reload`: the stop's.
        let _gate = gate_scope();

        // Order matters: stop, then disable (needs the fragment's `[Install]` section to know which
        // symlinks to remove), then remove the fragment and any drop-in, then reload.
        // Deliberately `Budget::Unbounded`: `uninstall` has no `--timeout` of its own, this stop
        // is a means rather than an end, and bounding it would turn a slow-stopping service into a
        // failed uninstall where today it succeeds.
        stop_impl(id, Budget::Unbounded, Budget::Unbounded.start())?;

        disable_impl(id)?;

        // Verified removal, mirroring `replace_unit_verified`: the gap since `flow::owned` read the
        // fragment spans two full `systemctl` round-trips, wide enough for something else to have
        // replaced the fragment in the meantime (obligation 1's TOCTOU class again).
        match quarantine_if_still_ours(id, &expected_text)? {
            Some(backup_path) => remove_file_if_present(&backup_path)?,
            None => {
                return Err(Error::Other(format!(
                    "the service at `{id}` changed after being confirmed installed; nothing was \
                     removed — re-run uninstall"
                )));
            }
        }

        // Both attempted regardless of whether the first failed, and both failures reported
        // together: a partial failure here must not leave systemd's loaded view silently stale, the
        // same reasoning `daemon_reload_or_report` documents on the `install` side.
        let dropin_result = remove_dir_if_present(&dropin_dir(id));
        let reload_result = daemon_reload();
        match (dropin_result, reload_result) {
            (Ok(()), Ok(())) => Ok(()),
            (Err(e), Ok(())) => Err(e),
            (Ok(()), Err(e)) => Err(e),
            (Err(e1), Err(e2)) => Err(Error::Other(format!(
                "uninstall for `{id}` partially failed: removing the drop-in directory failed \
                 ({e1}), and `systemctl daemon-reload` also failed ({e2}); the unit fragment is \
                 already gone"
            ))),
        }
    }

    fn enable(&self, id: &Id) -> Result<()> {
        flow::owned(self, id)?;
        enable_impl(id.as_str())
    }

    fn disable(&self, id: &Id) -> Result<()> {
        flow::owned(self, id)?;
        disable_impl(id.as_str())
    }

    fn start(&self, id: &Id, budget: Budget) -> Result<()> {
        start_with(id, budget, &REAL_STEPS)
    }

    /// Makes ready now what each start or stop in `steps` needs to watch its one `systemctl` under
    /// `budget`: a thread for each of its two streams, the stderr it announces its job on and its
    /// stdout — never a temp file. `install` runs no watched `systemctl`, and a budget that does not wait, or never
    /// stops waiting, watches none. While it is held, the steps also share one version gate — see
    /// `systemctl::GateScope`.
    fn prepare(&self, steps: &[Step], budget: Budget) -> Result<Prepared> {
        let watched = steps
            .iter()
            .filter(|step| matches!(step, Step::Start | Step::Stop) && systemctl::watched(budget))
            .count();
        let spares = bounded::spare(bounded::Needs {
            listeners: 2 * watched,
            ..bounded::Needs::default()
        })
        .map_err(|e| {
            Error::Other(format!(
                "no thread could be made to watch a `systemctl` request, so none was sent: {e}"
            ))
        })?;
        Ok(Prepared::holding((spares, gate_scope())))
    }

    /// One `systemctl restart --no-block`: one job, so no start can overtake
    /// the stop. systemd stops the unit and starts it again, or, while it is
    /// still activating, folds the restart into the start already running —
    /// see `request_restart_impl`.
    fn request_restart(&self, id: &Id) -> Option<Result<()>> {
        Some(flow::owned(self, id).and_then(|_| request_restart_impl(id.as_str())))
    }

    /// The plain request-only start. `restart` never issues it here — see
    /// [`Self::request_restart`] — since after a stop nobody waited for, it
    /// replaces a stop job systemd has not yet run.
    fn request_start_after_stop(&self, id: &Id) -> Result<()> {
        start_with(id, Budget::Immediate, &REAL_STEPS)
    }

    fn stop(&self, id: &Id, budget: Budget) -> Result<()> {
        stop_with(id, budget, &REAL_STEPS)
    }

    fn kill(&self, id: &Id) -> Result<()> {
        flow::owned(self, id)?;
        kill_impl(id.as_str())
    }
}

// start/stop ==========================================================================================================

/// The steps `start`/`stop` take, behind a seam so their ORDER is assertable without a real systemd
/// — the precedent is `cli::restart`'s `start_clock`. `manager_tests.rs` pins it.
struct VerbSteps<'a> {
    start_clock: &'a dyn Fn(Budget) -> Deadline,
    owned: &'a dyn Fn(&Id) -> Result<String>,
    start: &'a dyn Fn(&str, Budget, Deadline) -> Result<()>,
    stop: &'a dyn Fn(&str, Budget, Deadline) -> Result<()>,
}

const REAL_STEPS: VerbSteps<'static> = VerbSteps {
    start_clock: &Budget::start,
    owned: &|id: &Id| flow::owned(&Systemd, id),
    start: &start_impl,
    stop: &stop_impl,
};

fn start_with(id: &Id, budget: Budget, steps: &VerbSteps<'_>) -> Result<()> {
    // Derived at verb entry, so discovery spends the budget too. It bounds only the wait for the job,
    // never whether the job is enqueued — see `run_verb`.
    let deadline = (steps.start_clock)(budget);
    (steps.owned)(id)?;
    (steps.start)(id.as_str(), budget, deadline)
}

fn stop_with(id: &Id, budget: Budget, steps: &VerbSteps<'_>) -> Result<()> {
    // Derived at verb entry, so discovery spends the budget too. It bounds only the wait for the job,
    // never whether the job is enqueued — see `run_verb`.
    let deadline = (steps.start_clock)(budget);
    (steps.owned)(id)?;
    (steps.stop)(id.as_str(), budget, deadline)
}

// Enumeration =========================================================================================================

/// Every id `Systemd::list` has to answer for, and the entries standing for whatever the passes
/// that produced it never reached.
///
/// # Which ids those are
///
/// The same two names `discover` calls this id's artifact, asked of the same directories `discover`
/// asks: `<id>.service` in [`UNIT_DIR`] alone, the only fragment path this backend ever reads or
/// writes, and `<id>.service.d` under every root of [`DROPIN_SEARCH_DIRS`], since
/// `discover::residue` counts one there as much as one in `/etc`.
///
/// Reaching exactly as far as `residue` is the point. An id whose drop-in could not be read is
/// `Error::Undetermined` to `status`; a listing that never enumerated it says, by leaving it out,
/// that nothing is installed there — the negative conclusion [`Installed::Undetermined`] exists to
/// forbid. That still bounds the scan well short of the unit load path: only directory *names* come
/// out of each root, and only the ids they name are asked about.
///
/// A fragment under a root other than `UNIT_DIR` deliberately names no id here. `raw_state` looks
/// for `<id>.service` in `UNIT_DIR` and nowhere else, so a unit shipped in `/usr/lib` is already
/// `NotInstalled` to every verb goetia has.
///
/// # The half of `residue` this does not name: enablement links
///
/// `residue` also stats `multi-user.target.wants/<id>.service` under four roots, and an id whose
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
    /// Ids named only by a `<id>.service.d` directory. Disjoint from `units` by construction: an id
    /// with a fragment is classified through that, and one entry per id is what
    /// `cli::support::partition_installed` asserts.
    dropin_only: Vec<String>,
    /// One per pass that started and did not finish — a root each, since a root that could not be
    /// enumerated says nothing about the next one.
    incomplete: Vec<Installed>,
}

/// The enumeration behind [`HostScan`], one `read_dir` per root.
fn scan_host() -> HostScan {
    let unit_dir = scan_unit_dir(Path::new(UNIT_DIR));
    let mut incomplete: Vec<Installed> = unit_dir.incomplete.into_iter().collect();
    let mut dropin_ids: BTreeSet<String> = unit_dir.dropins.into_iter().collect();
    for root in DROPIN_SEARCH_DIRS.iter().filter(|root| **root != UNIT_DIR) {
        let scan = scan_unit_dir(Path::new(root));
        // Fragments outside `UNIT_DIR` are not this backend's — see [`HostScan`].
        dropin_ids.extend(scan.dropins);
        incomplete.extend(scan.incomplete);
    }
    let dropin_only = {
        let with_fragment: BTreeSet<&str> = unit_dir.units.iter().map(|(id, _)| id.as_str()).collect();
        dropin_ids
            .into_iter()
            .filter(|id| !with_fragment.contains(id.as_str()))
            .collect()
    };
    HostScan {
        units: unit_dir.units,
        dropin_only,
        incomplete,
    }
}

/// What one pass over one directory found: the `<id>.service` fragments and `<id>.service.d`
/// drop-in directories it reached, and — when the pass stopped early — the entry standing for
/// whatever it never did.
#[derive(Debug)]
struct UnitScan {
    units: Vec<(String, PathBuf)>,
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
    let mut dropins = Vec::new();
    let incomplete = flow::scan(
        &dir.display().to_string(),
        fs::read_dir(dir),
        |entries| Box::new(entries.map(|entry| entry.map(|entry| entry.path()))),
        |op, e| format!("failed to {op} the directory: {e}"),
        |path: PathBuf| {
            // Lossy: a non-UTF-8 unit name still has to be reported, and every read goes through
            // `path` itself rather than through this rendering of it.
            let Some(file_name) = path.file_name().map(|name| name.to_string_lossy().into_owned()) else {
                return;
            };
            // The name alone, with nothing stat'd: a drop-in directory that is not one — the regular
            // file `install_refuses_an_unreadable_dropin_over_our_own_fragment` seeds, whose
            // `read_dir` answers `ENOTDIR` for every uid — is exactly an id this pass must not drop.
            if let Some(id) = file_name.strip_suffix(".service.d") {
                dropins.push(id.to_string());
            } else if let Some(id) = file_name.strip_suffix(".service") {
                units.push((id.to_string(), path));
            }
        },
    );
    UnitScan {
        units,
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
/// `generate::unit` and be written verbatim into `User=`, failing the unit at start with an opaque
/// status — exactly what obligation 5 goes out of its way to avoid for directory permissions.
fn identity_for(user: &User) -> Result<Identity> {
    let name = match user {
        User::Root => "0".to_string(),
        User::Name(name) => name.clone(),
        User::Id(AccountId::Uid(uid)) => uid.to_string(),
        User::Id(AccountId::Sid(_)) => {
            return Err(Error::Other(
                "a Windows account id (SID) is not a valid user for the systemd backend".to_string(),
            ));
        }
    };
    Ok(Identity { user: name })
}

// Paths ===============================================================================================================

fn unit_name(id: &str) -> String {
    format!("{id}.service")
}

fn unit_path(id: &str) -> PathBuf {
    Path::new(UNIT_DIR).join(unit_name(id))
}

fn dropin_dir(id: &str) -> PathBuf {
    Path::new(UNIT_DIR).join(format!("{id}.service.d"))
}

// Shared removal helpers ==============================================================================================

fn remove_file_if_present(path: &Path) -> Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(io_err("remove", path, e)),
    }
}

fn remove_dir_if_present(path: &Path) -> Result<()> {
    match fs::remove_dir_all(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(io_err("remove", path, e)),
    }
}

// Errors ==============================================================================================================

fn io_err(op: &str, path: &Path, source: io::Error) -> Error {
    Error::Other(format!("failed to {op} {}: {source}", path.display()))
}

// Test-only id generator, mirroring `manager::conformance`'s: unique enough for a single test binary
// run given `manager_tests.rs`'s own RAII cleanup, without pulling in a dev-only randomness crate for
// a plain library unit test.
#[cfg(test)]
static TEST_COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

#[cfg(test)]
fn test_id() -> String {
    let n = TEST_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    format!("goetia-test-{pid:x}-{n:x}", pid = std::process::id())
}

#[cfg(test)]
#[path = "manager_tests.rs"]
mod manager_tests;
