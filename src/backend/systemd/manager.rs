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
//!    [`discover::dropin_dirs`] folds `<id>.service.d`'s `*.conf` contents — across every root of
//!    systemd's system unit search path, `/etc/systemd/system.control` included — into the text
//!    handed to `decide` (never into what is actually written); `decide::decide`'s own
//!    `foreign_overlay` parameter — never a backend-local override of its `Outcome` — closes the one
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
//!    `chown`s the parents of `logs` and `cwd` while still elevated — otherwise
//!    `StandardOutput=append:` fails the unit at start with an opaque status. Only path components
//!    this call actually creates are touched; an already-existing directory's mode and ownership are
//!    left alone, and verified writable by the target account instead.
//! 6. **Uninstall order** is stop -> `systemctl disable` -> remove -> `daemon-reload`. Disabling
//!    after the fragment is gone is impossible (no `[Install]` section left to read), which would
//!    leave exactly the `.wants` symlink `uninstall_leaves_nothing` checks for.
//! 7. **Absence is about the id, not about the fragment file.** `cli::uninstall` maps
//!    [`Error::NotInstalled`] — and only that variant — to exit `0` and "nothing to do", so a
//!    backend reporting it off `<id>.service`'s absence alone would certify "confirmed gone" for an
//!    id systemd still applies a drop-in to, or still enrolls at boot through a
//!    `multi-user.target.wants` link. `discover::fragmentless` is the one predicate every reporting
//!    verb's `require_installed`/`status` asks, so no two of them can describe one filesystem state
//!    differently — `list` included, which asks it of every id it enumerates with no readable
//!    fragment. A listing that asked less would leave out an id `status` answers
//!    `Error::Undetermined` for, and leaving an id out is how a listing says nothing is there.
//!
//!    A *vacant* fragment path is not an absent id either, which is the same obligation one race
//!    down: `write::replace_unit_verified` renames the fragment to a quarantine sibling and only
//!    re-creates it after the replacement is written and fsync'd, so `<id>.service` is genuinely
//!    missing for the length of every routine update. `discover::fragmentless` re-asks
//!    [`UNIT_DIR`] for both names before anything concludes absence from one of them — by `stat`
//!    for the fragment, whose name it knows exactly, and by one `getdents64` snapshot
//!    ([`snapshot::names`]) for the quarantine sibling, whose per-attempt suffix it does not. Both
//!    answer for an instant. A `read_dir` cursor does not, and this is the same directory the
//!    rename moves the name *within*, so a cursor is exactly what can see neither name.

mod dirs;
mod discover;
mod snapshot;
mod systemctl;
mod write;

use std::collections::BTreeSet;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use discover::{DROPIN_SEARCH_DIRS, RawState, absent_error, classify_and_read, discover, raw_state, require_installed};
use systemctl::{daemon_reload, daemon_reload_or_report, run_systemctl, start_impl, status_from_unit, stop_impl};
use write::{CreateOutcome, ReplaceOutcome, create_unit, quarantine_if_still_ours, replace_unit_verified};

use crate::backend::Identity;
use crate::backend::systemd::generate;
use crate::decide::{self, Outcome};
use crate::error::{Error, Result};
use crate::manager::{Installed, ServiceManager, Status};
use crate::spec::{AccountId, DaemonSpec, Id, User};

/// Where systemd looks for system unit files. Never overridden — the integration tests run for real
/// against this exact path, elevated.
const UNIT_DIR: &str = "/etc/systemd/system";

// Systemd =============================================================================================================

/// The Linux [`ServiceManager`]. Holds no state of its own: every operation re-derives what it needs
/// from the filesystem and `systemctl`, so nothing here can go stale between calls.
#[derive(Debug, Default)]
pub struct Systemd;

impl Systemd {
    pub fn new() -> Self {
        Self
    }
}

impl ServiceManager for Systemd {
    fn install(&self, spec: &DaemonSpec, force: bool) -> Result<Outcome> {
        let identity = identity_for(&spec.user)?;
        let desired = generate::unit(spec, &identity);

        // Looping rather than a single attempt: a `Raced` result means something appeared at this id
        // between classification and the write below (obligation 1). That is a real, detected state
        // change to react to — not a wait for time to pass — so re-classifying and trying again is
        // bounded by actual system state, not a chosen number of attempts.
        loop {
            let d = discover(spec.id.as_str())?;
            let outcome = decide::decide(
                &d.ownership,
                d.on_disk.as_deref(),
                &desired,
                spec,
                crate::version(),
                force,
                &d.overlay,
            );

            match &outcome {
                Outcome::Create => {
                    dirs::ensure_parent_dirs(spec)?;
                    match create_unit(spec.id.as_str(), &desired)? {
                        CreateOutcome::Created => {
                            daemon_reload_or_report(spec.id.as_str())?;
                            return Ok(outcome);
                        }
                        CreateOutcome::Raced => continue,
                    }
                }
                Outcome::Update { .. } | Outcome::Stale { .. } => {
                    dirs::ensure_parent_dirs(spec)?;
                    let expected_text = d
                        .fragment_text
                        .as_deref()
                        .expect("Ownership::Ours implies discover classified a regular file");
                    match replace_unit_verified(spec.id.as_str(), &desired, expected_text)? {
                        ReplaceOutcome::Replaced => {
                            // Only after the write has actually committed: clearing the drop-in
                            // first (before the write) would delete an admin's overrides even on a
                            // path that turns out `Raced` and never writes anything, or errors out
                            // partway through — `uninstall` clears its own fragment before its
                            // drop-in for the identical reason.
                            remove_dir_if_present(&dropin_dir(spec.id.as_str()))?;
                            daemon_reload_or_report(spec.id.as_str())?;
                            return Ok(outcome);
                        }
                        ReplaceOutcome::Raced => continue,
                    }
                }
                // `UpToDate` / `Conflict` (without force) / `RefuseForeign` / `RefuseUnreadable`:
                // nothing to write.
                _ => return Ok(outcome),
            }
        }
    }

    fn preview_install(&self, spec: &DaemonSpec) -> Result<Outcome> {
        let identity = identity_for(&spec.user)?;
        let desired = generate::unit(spec, &identity);
        let d = discover(spec.id.as_str())?;
        // Always previewed without `force` — see the trait doc comment.
        Ok(decide::decide(
            &d.ownership,
            d.on_disk.as_deref(),
            &desired,
            spec,
            crate::version(),
            false,
            &d.overlay,
        ))
    }

    fn uninstall(&self, id: &Id) -> Result<()> {
        let id = id.as_str();
        let expected_text = require_installed(id)?;
        let unit = unit_name(id);

        // Order matters: stop, then disable (needs the fragment's `[Install]` section to know which
        // symlinks to remove), then remove the fragment and any drop-in, then reload.
        stop_impl(&unit)?;

        let disabled = run_systemctl(&["disable", &unit])?;
        if !disabled.status.success() {
            return Err(Error::Other(format!(
                "systemctl disable {unit} failed: {}",
                String::from_utf8_lossy(&disabled.stderr)
            )));
        }

        // Verified removal, mirroring `replace_unit_verified`: the gap since `require_installed`
        // spans two full `systemctl` round-trips, wide enough for something else to have replaced
        // the fragment in the meantime (obligation 1's TOCTOU class again).
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
        let id = id.as_str();
        require_installed(id)?;
        let unit = unit_name(id);
        let output = run_systemctl(&["enable", &unit])?;
        if output.status.success() {
            Ok(())
        } else {
            Err(Error::Other(format!(
                "systemctl enable {unit} failed: {}",
                String::from_utf8_lossy(&output.stderr)
            )))
        }
    }

    fn disable(&self, id: &Id) -> Result<()> {
        let id = id.as_str();
        require_installed(id)?;
        let unit = unit_name(id);
        let output = run_systemctl(&["disable", &unit])?;
        if output.status.success() {
            Ok(())
        } else {
            Err(Error::Other(format!(
                "systemctl disable {unit} failed: {}",
                String::from_utf8_lossy(&output.stderr)
            )))
        }
    }

    fn start(&self, id: &Id) -> Result<()> {
        let id = id.as_str();
        require_installed(id)?;
        start_impl(&unit_name(id))
    }

    fn stop(&self, id: &Id) -> Result<()> {
        let id = id.as_str();
        require_installed(id)?;
        stop_impl(&unit_name(id))
    }

    fn status(&self, id: &Id) -> Result<Status> {
        let id = id.as_str();
        match raw_state(id)? {
            RawState::Absent => Err(absent_error(id)?),
            RawState::NotOurs => Err(Error::Foreign {
                id: id.to_string(),
                recovery: decide::foreign_recovery(id),
            }),
            RawState::Regular(text) => match generate::extract(&text) {
                Ok(None) => Err(Error::Foreign {
                    id: id.to_string(),
                    recovery: decide::foreign_recovery(id),
                }),
                // A decode failure must not fabricate a plausible-looking `Status` — see
                // `ServiceManager::status`'s doc comment.
                Err(e) => Err(e),
                Ok(Some(_blob)) => status_from_unit(&unit_name(id)),
            },
        }
    }

    fn list(&self) -> Result<Vec<Installed>> {
        let scan = scan_host();

        let mut out = Vec::new();
        for (id, path) in &scan.units {
            let id = id.as_str();

            // The same classification `status` performs, so the two cannot describe one machine
            // differently.
            let text = match classify_and_read(path) {
                Ok(RawState::Regular(text)) => text,
                // Established as nothing goetia wrote (a masked unit's symlink, a FIFO, a `.d`
                // directory, bytes that are not UTF-8): obligations 2 and 3, and foreign entries
                // are what `list` omits.
                Ok(RawState::NotOurs) => continue,
                // Vacant between the scan and the open. That settles the *path*, not the id —
                // obligation 7 — so it takes the same re-ask every other fragmentless id gets.
                Ok(RawState::Absent) => {
                    out.extend(fragmentless_entry(id));
                    continue;
                }
                // A read that did not complete establishes nothing about the id — least of all that
                // it is absent, which is what omitting it from the enumeration would say.
                Err(failure) => {
                    out.push(Installed::Undetermined {
                        name: Some(id.to_string()),
                        reason: failure.detail(),
                    });
                    continue;
                }
            };

            match generate::extract(&text) {
                Ok(None) => {} // foreign: `list` reports only what Goetia owns.
                // A `systemctl show` failure for this one unit must not take down the listing of
                // every other Goetia-managed service on the host — the same per-entry fault
                // tolerance the decode-failure arm just below already provides, extended to a
                // status-query failure instead of a marker-decode failure.
                Ok(Some(blob)) => match status_from_unit(&unit_name(id)) {
                    Ok(status) => out.push(Installed::Ours {
                        spec: blob.spec,
                        state: status.state,
                        pid: status.pid,
                        enabled: status.enabled,
                    }),
                    Err(e) => out.push(Installed::OursUnreadable {
                        name: id.to_string(),
                        reason: format!("decoded, but its live state could not be queried: {e}"),
                    }),
                },
                Err(e) => out.push(Installed::OursUnreadable {
                    name: id.to_string(),
                    reason: e.to_string(),
                }),
            }
        }
        // The ids with no fragment of their own. Obligation 7 again: what occupies an id is not the
        // fragment file, so an id whose *drop-in* could not be read — or whose fragment the scan
        // caught mid-replacement, under its quarantine name — is exactly as unclassified as one
        // whose fragment could not be, and `status` says so for all three.
        for id in &scan.fragmentless {
            out.extend(fragmentless_entry(id));
        }
        // Last, so a scan that got some way in still reports what it named before what it could
        // not — the order `cli::support::partition_installed` imposes on the rendered output too.
        out.extend(scan.incomplete);
        Ok(out)
    }
}

/// What `list` reports for an id with no readable fragment, from the re-ask `status` answers off:
/// both determinate outcomes are reported by omission — nothing at the id is an unoccupied one, and
/// residue under an absent fragment is `Ownership::Foreign`, which `list` leaves out like any other
/// foreign id — and only an id that stayed unclassified gets an entry, because omission would claim
/// it absent.
fn fragmentless_entry(id: &str) -> Option<Installed> {
    match discover::fragmentless(id) {
        discover::Fragmentless::Unoccupied | discover::Fragmentless::Residue(_) => None,
        discover::Fragmentless::Unsettled(unsettled) => Some(Installed::Undetermined {
            name: Some(id.to_string()),
            reason: unsettled.detail(),
        }),
    }
}

// Enumeration =========================================================================================================

/// Every id `Systemd::list` has to answer for, and the entries standing for whatever the passes
/// that produced it never reached.
///
/// # Which ids those are
///
/// The same names `discover` calls this id's artifact, asked of the same directories `discover`
/// asks: `<id>.service` in [`UNIT_DIR`] alone, the only fragment path this backend ever reads or
/// writes, and `<id>.service.d` under every root of [`DROPIN_SEARCH_DIRS`], since
/// `discover::residue` counts one there as much as one in `/etc`. Plus the name that *is* the
/// fragment while it is being replaced: a `write::quarantine_prefix` sibling in `UNIT_DIR`. An id
/// caught mid-`install` has no `<id>.service` for this pass to see, and naming nothing for it is
/// how the listing would say it is not installed.
///
/// Reaching exactly as far as `discover::fragmentless` is the point. An id whose drop-in could not
/// be read is `Error::Undetermined` to `status`; a listing that never enumerated it says, by
/// leaving it out, that nothing is installed there — the negative conclusion
/// [`Installed::Undetermined`] exists to forbid. That still bounds the scan well short of the unit
/// load path: only directory *names* come out of each root, and only the ids they name are asked
/// about.
///
/// A fragment under a root other than `UNIT_DIR` deliberately names no id here. `raw_state` looks
/// for `<id>.service` in `UNIT_DIR` and nowhere else, so a unit shipped in `/usr/lib` is already
/// `NotInstalled` to every verb goetia has.
///
/// # The half of `residue` this cannot name: enablement links
///
/// `residue` also stats `multi-user.target.wants/<id>.service` under four roots, and an id whose
/// *only* trace is such a link is still not named here. Enumerating those directories would ask
/// `residue` about every unit enabled on the host, all foreign by construction, which is the sweep
/// this bound exists to avoid — and where the stats succeed, an id with a link and nothing else is
/// `Ownership::Foreign`, which `list` omits by design anyway.
///
/// What is not left to that argument is the case where the stats *do not* succeed. There, `status`
/// answers `Error::Undetermined` for every fragmentless id while a listing that never stat'd
/// anything reports a clean, empty `undetermined` — and a consumer following the documented rule
/// (an empty `undetermined` is what licenses reading a missing id as uninstalled) draws exactly the
/// negative the code cannot support. [`probe_wants_dirs`] closes that: one stat per root, of the
/// same kind `residue` performs, standing an aggregate entry only when the read `residue` itself
/// depends on is the one that fails.
#[derive(Debug)]
struct HostScan {
    /// Ids with a fragment in [`UNIT_DIR`], with its path.
    units: Vec<(String, PathBuf)>,
    /// Ids named by something other than a fragment: a `<id>.service.d` directory under any search
    /// root, or a quarantined fragment in [`UNIT_DIR`]. Disjoint from `units` by construction: an
    /// id with a fragment is classified through that, and one entry per id is what
    /// `cli::support::partition_installed` asserts.
    fragmentless: Vec<String>,
    /// One per pass that started and did not finish — a root each, since a root that could not be
    /// enumerated says nothing about the next one.
    incomplete: Vec<Installed>,
}

/// The enumeration behind [`HostScan`], one [`snapshot::names`] per root.
fn scan_host() -> HostScan {
    let unit_dir = scan_unit_dir(Path::new(UNIT_DIR));
    let mut incomplete: Vec<Installed> = unit_dir.incomplete.into_iter().collect();
    let mut named: BTreeSet<String> = unit_dir.dropins.into_iter().chain(unit_dir.quarantined).collect();
    for root in DROPIN_SEARCH_DIRS.iter().filter(|root| **root != UNIT_DIR) {
        let scan = scan_unit_dir(Path::new(root));
        // Fragments outside `UNIT_DIR` are not this backend's, and a quarantine is only ever
        // written into `UNIT_DIR` — see [`HostScan`].
        named.extend(scan.dropins);
        incomplete.extend(scan.incomplete);
    }
    let fragmentless = {
        let with_fragment: BTreeSet<&str> = unit_dir.units.iter().map(|(id, _)| id.as_str()).collect();
        named
            .into_iter()
            .filter(|id| !with_fragment.contains(id.as_str()))
            .collect()
    };
    incomplete.extend(probe_wants_dirs());
    HostScan {
        units: unit_dir.units,
        fragmentless,
        incomplete,
    }
}

/// One stat per enablement-link root, of a name that is not there: `residue`'s own question, asked
/// once for the whole host instead of once per id, and answered by the same syscall.
///
/// This is the [`HostScan`] doc comment's last gap. It is deliberately *not* a readability probe —
/// a `read_dir` would answer a different question than `residue` asks, denying a mode-`0111`
/// directory that every stat `residue` performs succeeds in, and would stand a permanent aggregate
/// entry and a permanent exit `4` on a host where nothing goetia reads fails. `ENOENT` (the usual
/// answer, and the one an absent root gives) establishes that this root holds no link for the
/// probed name and would have answered for any other; only a stat that did not *complete* leaves
/// every id's enablement link unread, and that is what gets an entry.
fn probe_wants_dirs() -> Vec<Installed> {
    wants_probe(discover::wants_dirs())
}

/// The probe itself, over the directories rather than [`discover::wants_dirs`] directly — which is
/// what makes the failing half reachable from a test, since the real roots answer for the host the
/// tests run on.
fn wants_probe(dirs: impl Iterator<Item = PathBuf>) -> Vec<Installed> {
    dirs.filter_map(|dir| {
        let probe = dir.join(PROBE_LINK);
        match fs::symlink_metadata(&probe) {
            Ok(_) => None,
            Err(e) if e.kind() == io::ErrorKind::NotFound => None,
            Err(e) => Some(Installed::scan_incomplete(
                &dir.display().to_string(),
                &format!(
                    "the stat every id's enablement link there needs did not complete: failed to \
                     stat {probe}: {e}",
                    probe = probe.display()
                ),
            )),
        }
    })
    .collect()
}

/// The name [`probe_wants_dirs`] stats. Which name it is does not matter — every answer but a
/// failure means the stat completed — so it is one that says what it is if it ever shows up in a
/// strace.
const PROBE_LINK: &str = ".goetia-probe.service";

/// What one snapshot of one directory held: the `<id>.service` fragments and `<id>.service.d`
/// drop-in directories in it, and — when the snapshot could not be taken at all — the entry
/// standing for everything it would have named.
#[derive(Debug)]
struct UnitScan {
    units: Vec<(String, PathBuf)>,
    dropins: Vec<String>,
    /// Ids named by a `write::quarantine_prefix` sibling: an `install` replacing this id's fragment
    /// has it renamed out of the way right now, or an interrupted one left it there. Meaningful in
    /// [`UNIT_DIR`] only, which is the sole directory goetia writes one into.
    quarantined: Vec<String>,
    incomplete: Option<Installed>,
}

/// Snapshot `dir`. A snapshot that cannot be taken is *reported* rather than propagated: an `Err`
/// out of `list` would throw away every id this same call already classified and reach the CLI as
/// an empty document on exit `1`, which is `list` saying the host has no daemons — the one claim a
/// scan that did not finish cannot support.
///
/// `dir` not existing is neither: absence is *established* there, so it is an empty scan with
/// nothing outstanding — the answer [`ServiceManager::list`]'s doc comment requires of every
/// backend, and the one launchd already gave for its own missing staging directory.
///
/// [`snapshot::names`] rather than [`fs::read_dir`], for the reason that module's own doc comment
/// gives: this is the directory `write::replace_unit_verified` renames a fragment *within*, and a
/// cursor walking it can pass the quarantine name's slot before the rename and the fragment's after
/// it — naming neither, which hands `list` no id to ask about and leaves an installed daemon out of
/// a listing that claims to be complete.
fn scan_unit_dir(dir: &Path) -> UnitScan {
    match snapshot::names(dir) {
        Ok(names) => collect_units(dir, names.into_iter()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => UnitScan {
            units: Vec::new(),
            dropins: Vec::new(),
            quarantined: Vec::new(),
            incomplete: None,
        },
        Err(e) => UnitScan {
            units: Vec::new(),
            dropins: Vec::new(),
            quarantined: Vec::new(),
            incomplete: Some(Installed::scan_incomplete(
                &dir.display().to_string(),
                &format!("failed to read directory: {e}"),
            )),
        },
    }
}

/// What the snapshot's names amount to, over an iterator of them rather than over
/// [`snapshot::names`] directly so the classification is reachable from a test without a directory
/// to seed.
fn collect_units(dir: &Path, names: impl Iterator<Item = std::ffi::OsString>) -> UnitScan {
    let mut units = Vec::new();
    let mut dropins = Vec::new();
    let mut quarantined = Vec::new();
    for name in names {
        let path = dir.join(&name);
        // Lossy, exactly as before: a non-UTF-8 unit name still has to be reported, and every read
        // below goes through `path` itself rather than through this rendering of it.
        let file_name = name.to_string_lossy().into_owned();
        // The name alone, with nothing stat'd: a drop-in directory that is not one — the regular
        // file `install_refuses_an_unreadable_dropin_over_our_own_fragment` seeds, whose `read_dir`
        // answers `ENOTDIR` for every uid — is exactly an id this pass must not drop.
        if let Some(id) = file_name.strip_suffix(".service.d") {
            dropins.push(id.to_string());
            continue;
        }
        // A quarantine name ends in neither suffix: it is the fragment's own name with a private
        // prefix and a per-attempt suffix around it, and the id it stands for is in the middle.
        if let Some(id) = write::quarantined_id(&file_name) {
            quarantined.push(id.to_string());
            continue;
        }
        let Some(id) = file_name.strip_suffix(".service") else {
            continue;
        };
        units.push((id.to_string(), path));
    }
    UnitScan {
        units,
        dropins,
        quarantined,
        incomplete: None,
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
