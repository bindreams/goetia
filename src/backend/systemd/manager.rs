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
//!    descriptor by `fstat`, reporting any non-regular file as [`discover::RawState::NonRegular`] —
//!    always `Ownership::Foreign` — without ever opening it for reading. A *regular* file whose
//!    bytes could not be obtained is neither: it is [`Error::Undetermined`] for `status` and
//!    [`Installed::Undetermined`] for `list`, since nothing about its content was established.
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
//!    `multi-user.target.wants` link. `discover::residue` is the one predicate both `install`'s
//!    `discover` and every other verb's `require_installed`/`status` ask, so no two verbs can
//!    describe one filesystem state differently.

mod dirs;
mod discover;
mod systemctl;
mod write;

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use discover::{RawState, absent_error, classify_and_read, discover, raw_state, require_installed};
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
            RawState::NonRegular => Err(Error::Foreign {
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
        let scan = scan_unit_dir(Path::new(UNIT_DIR));

        let mut out = Vec::new();
        for (id, path) in &scan.units {
            let id = id.as_str();

            // The same classification `status` performs, so the two cannot describe one machine
            // differently.
            let text = match classify_and_read(path) {
                Ok(RawState::Regular(text)) => text,
                // Gone between the scan and the open, or never a fragment to read (a masked unit's
                // symlink, a FIFO, a `.d` directory): obligations 2 and 3.
                Ok(RawState::Absent | RawState::NonRegular) => continue,
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
        // Last, so a scan that got some way in still reports what it named before what it could
        // not — the order `cli::support::partition_installed` imposes on the rendered output too.
        out.extend(scan.incomplete);
        Ok(out)
    }
}

// Enumeration =========================================================================================================

/// What one pass over a unit directory found: the `<id>.service` fragments it reached, and — when
/// the pass stopped early — the entry standing for whatever it never did.
#[derive(Debug)]
struct UnitScan {
    units: Vec<(String, PathBuf)>,
    incomplete: Option<Installed>,
}

/// Enumerate `dir`. A pass that cannot start, or cannot finish, is *reported* rather than
/// propagated: an `Err` out of `list` would throw away every id this same call already classified
/// and reach the CLI as an empty document on exit `1`, which is `list` saying the host has no
/// daemons — the one claim a scan that did not finish cannot support.
///
/// `dir` not existing is neither: absence is *established* there, so it is an empty scan with
/// nothing outstanding — the answer [`ServiceManager::list`]'s doc comment requires of every
/// backend, and the one launchd already gave for its own missing staging directory.
fn scan_unit_dir(dir: &Path) -> UnitScan {
    match fs::read_dir(dir) {
        Ok(entries) => collect_units(dir, entries.map(|entry| entry.map(|entry| entry.path()))),
        Err(e) if e.kind() == io::ErrorKind::NotFound => UnitScan {
            units: Vec::new(),
            incomplete: None,
        },
        Err(e) => UnitScan {
            units: Vec::new(),
            incomplete: Some(Installed::scan_incomplete(
                &dir.display().to_string(),
                &format!("failed to read directory: {e}"),
            )),
        },
    }
}

/// The pass itself, over an iterator of paths rather than [`fs::read_dir`] directly — which is what
/// makes the mid-pass fault reachable from a test, since no `readdir` fails on demand. That fault
/// is the half that matters: everything already collected comes back alongside the aggregate,
/// rather than being discarded because a *later* dirent could not be read.
fn collect_units(dir: &Path, entries: impl Iterator<Item = io::Result<PathBuf>>) -> UnitScan {
    let mut units = Vec::new();
    for entry in entries {
        let path = match entry {
            Ok(path) => path,
            Err(e) => {
                return UnitScan {
                    units,
                    incomplete: Some(Installed::scan_incomplete(
                        &dir.display().to_string(),
                        &format!("failed to read a directory entry: {e}"),
                    )),
                };
            }
        };
        // Lossy, exactly as before: a non-UTF-8 unit name still has to be reported, and every read
        // below goes through `path` itself rather than through this rendering of it.
        let Some(file_name) = path.file_name().map(|name| name.to_string_lossy().into_owned()) else {
            continue;
        };
        let Some(id) = file_name.strip_suffix(".service") else {
            continue;
        };
        units.push((id.to_string(), path));
    }
    UnitScan {
        units,
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
