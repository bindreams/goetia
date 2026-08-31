//! The effectful half of the systemd backend: writes `/etc/systemd/system/<id>.service` and talks
//! to `systemctl`.
//!
//! Six correctness obligations a systemd unit-file backend must uphold, each with its own test (see
//! `tests/systemd_integration/linux.rs` and this module's own `manager_tests.rs`):
//!
//! 1. **A write must not clobber something it did not classify.** `rename(2)` unconditionally
//!    replaces its destination, so a plain classify-then-write is a TOCTOU window: a package
//!    postinst or a concurrent invocation can drop a foreign unit into the gap and have it destroyed
//!    by the very code path that exists to refuse that. [`create_unit`] uses
//!    [`tempfile::NamedTempFile::persist_noclobber`] (`linkat(2)` without replace semantics) for the
//!    create case; [`replace_unit_verified`] closes the same gap for the update/regenerate case by
//!    quarantining the current occupant under a temporary name and verifying its identity before
//!    committing to either direction.
//! 2. **A masked unit is not absent.** `systemctl mask` replaces the fragment with a symlink to
//!    `/dev/null`; reading it yields empty text, and a naive read-then-extract would see `Ok(None)`
//!    and let `install` write over it, silently unmasking a deliberately-masked service.
//!    [`raw_state`] `lstat`s the path and classifies any non-regular file as [`RawState::NonRegular`]
//!    — always `Ownership::Foreign` — without ever reading its contents.
//! 3. **Drop-ins are drift.** `systemctl edit` — the officially recommended way to add exactly the
//!    `MemoryMax=`/`After=` the design cites — writes `<id>.service.d/override.conf` and leaves the
//!    fragment itself byte-identical, so drift detection over the fragment alone misses it entirely.
//!    [`dropin_marker`] folds the drop-in directory's `*.conf` contents into the text handed to
//!    `decide` (never into what is actually written); `apply_dropin_override` closes the one branch
//!    that miss doesn't reach — `decide`'s version-mismatch check fires before any text comparison at
//!    all, so a drop-in alongside a stale artifact needs its own guard. Every successful write clears
//!    the drop-in directory, so a resolved conflict cannot wedge the id in permanent drift, and a
//!    drop-in directory with no fragment at all is refused rather than silently adopted as `Create`.
//! 4. **Permissions.** `NamedTempFile` is created mode 0600; after persisting, the unit would be
//!    root-only, breaking the promise that `list`/`show`/`diff` need no elevation.
//!    [`write_temp_unit`] `chmod`s 0644 before persisting.
//! 5. **Parent directories.** [`ensure_parent_dirs`] creates and, for a non-root account, `chown`s
//!    the parents of `logs` and `cwd` while still elevated — otherwise `StandardOutput=append:` fails
//!    the unit at start with an opaque status. Only path components this call actually creates are
//!    touched; an already-existing directory's mode and ownership are left alone, and verified
//!    writable by the target account instead.
//! 6. **Uninstall order** is stop -> `systemctl disable` -> remove -> `daemon-reload`. Disabling
//!    after the fragment is gone is impossible (no `[Install]` section left to read), which would
//!    leave exactly the `.wants` symlink `uninstall_leaves_nothing` checks for.

use std::collections::BTreeMap;
use std::fs;
use std::io::{self, Write as _};
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::backend::Identity;
use crate::backend::systemd::generate;
use crate::decide::{self, Outcome, Ownership};
use crate::error::{Error, Result};
use crate::manager::{Installed, ServiceManager, State, Status};
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
            );
            let outcome = apply_dropin_override(outcome, &d, force);

            match &outcome {
                Outcome::Create => {
                    ensure_parent_dirs(spec)?;
                    remove_dir_if_present(&dropin_dir(spec.id.as_str()))?;
                    match create_unit(spec.id.as_str(), &desired)? {
                        CreateOutcome::Created => {
                            daemon_reload_or_report(spec.id.as_str())?;
                            return Ok(outcome);
                        }
                        CreateOutcome::Raced => continue,
                    }
                }
                Outcome::Update { .. } | Outcome::Stale { .. } => {
                    ensure_parent_dirs(spec)?;
                    // Clears any drop-in unconditionally: whatever led here — a forced conflict
                    // resolution or a routine stale regenerate — makes the drop-in stale too, and
                    // leaving it behind is exactly the permanent-wedge obligation 3 exists to rule
                    // out.
                    remove_dir_if_present(&dropin_dir(spec.id.as_str()))?;
                    let expected_text = d
                        .fragment_text
                        .as_deref()
                        .expect("Ownership::Ours implies discover classified a regular file");
                    match replace_unit_verified(spec.id.as_str(), &desired, expected_text)? {
                        ReplaceOutcome::Replaced => {
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
        let outcome = decide::decide(
            &d.ownership,
            d.on_disk.as_deref(),
            &desired,
            spec,
            crate::version(),
            false,
        );
        Ok(apply_dropin_override(outcome, &d, false))
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
            RawState::Absent => Err(Error::NotInstalled { id: id.to_string() }),
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
        let dir = Path::new(UNIT_DIR);
        let entries = fs::read_dir(dir).map_err(|e| io_err("read directory", dir, e))?;

        let mut out = Vec::new();
        for entry in entries {
            let entry = entry.map_err(|e| io_err("read a directory entry in", dir, e))?;
            let file_name = entry.file_name();
            let name = file_name.to_string_lossy();
            let Some(id) = name.strip_suffix(".service") else {
                continue;
            };

            // A masked unit (symlink to /dev/null) or a `<id>.service.d` drop-in directory both fail
            // this check — neither is a fragment `list` should read, per obligations 2 and 3.
            let file_type = match entry.file_type() {
                Ok(t) => t,
                // `list` runs unelevated by design (obligation 4): a foreign unit shipped
                // non-world-readable (units carrying `LoadCredential=` commonly are 0600) cannot
                // carry a decodable goetia marker either way, so it is not ours to report — the same
                // disposition as a concurrent-uninstall race.
                Err(e) if is_benign_list_error(&e) => continue,
                Err(e) => return Err(io_err("stat", &entry.path(), e)),
            };
            if !file_type.is_file() {
                continue;
            }

            let path = entry.path();
            let text = match fs::read_to_string(&path) {
                Ok(t) => t,
                Err(e) if is_benign_list_error(&e) => continue,
                Err(e) => return Err(io_err("read", &path, e)),
            };

            match generate::extract(&text) {
                Ok(None) => {} // foreign: `list` reports only what Goetia owns.
                Ok(Some(blob)) => {
                    let status = status_from_unit(&unit_name(id))?;
                    out.push(Installed::Ours {
                        spec: blob.spec,
                        state: status.state,
                        enabled: status.enabled,
                    });
                }
                Err(e) => out.push(Installed::OursUnreadable {
                    name: id.to_string(),
                    reason: e.to_string(),
                }),
            }
        }
        Ok(out)
    }
}

fn is_benign_list_error(e: &io::Error) -> bool {
    matches!(e.kind(), io::ErrorKind::NotFound | io::ErrorKind::PermissionDenied)
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

// Discovery ===========================================================================================================

/// What's physically present at `id`'s unit path, before any interpretation of its content.
enum RawState {
    Absent,
    /// A symlink (a masked unit) or any other non-regular file — obligation 2. Its contents are never
    /// read: a masked unit's target is `/dev/null`, and reading through it would look identical to
    /// "nothing here".
    NonRegular,
    Regular(String),
}

fn raw_state(id: &str) -> Result<RawState> {
    let path = unit_path(id);
    match fs::symlink_metadata(&path) {
        Ok(meta) if meta.file_type().is_file() => {
            let text = fs::read_to_string(&path).map_err(|e| io_err("read", &path, e))?;
            Ok(RawState::Regular(text))
        }
        Ok(_) => Ok(RawState::NonRegular),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(RawState::Absent),
        Err(e) => Err(io_err("stat", &path, e)),
    }
}

/// What `install`/`preview_install` classified at `id`: an [`Ownership`] plus everything specific to
/// this backend that `decide` cannot see on its own.
struct Discovery {
    ownership: Ownership,
    /// `None` iff `ownership` is `Ownership::Absent`.
    on_disk: Option<String>,
    /// The fragment's own raw text — distinct from `on_disk`, which may have a drop-in marker folded
    /// in — present iff `ownership` came from a regular file (`Ours` or `OursUnreadable`). Used to
    /// verify identity before a later write/removal touches this exact fragment; see
    /// `quarantine_if_still_ours`. Content, not inode, is the identity that matters here: an inode
    /// number can be reused by the kernel moments after its file is unlinked, so two genuinely
    /// different files can share one — verified empirically triggering the false-positive this would
    /// otherwise cause.
    fragment_text: Option<String>,
    /// Whether `<id>.service.d/` currently holds any `*.conf` file — obligation 3. `decide` cannot
    /// see this: its vocabulary is artifact *text*, and this is filesystem structure alongside it.
    dropin_present: bool,
}

fn discover(id: &str) -> Result<Discovery> {
    match raw_state(id)? {
        RawState::Absent => match dropin_marker(id)? {
            None => Ok(Discovery {
                ownership: Ownership::Absent,
                on_disk: None,
                fragment_text: None,
                dropin_present: false,
            }),
            // A drop-in directory with no fragment at all: never silently adopt it as `Create`, or
            // the resulting unit inherits overrides goetia never wrote and cannot show — refuse it
            // the same way any other pre-existing, unmarked artifact is refused.
            Some(marker) => Ok(Discovery {
                ownership: Ownership::Foreign,
                on_disk: Some(marker),
                fragment_text: None,
                dropin_present: true,
            }),
        },
        RawState::NonRegular => Ok(Discovery {
            ownership: Ownership::Foreign,
            on_disk: Some(String::new()),
            fragment_text: None,
            dropin_present: false,
        }),
        RawState::Regular(text) => {
            // Obligation 3: fold any drop-in content into the text `decide` compares, without ever
            // writing that folded text back. Neither `desired` nor `regenerated` (both pure
            // `generate()` output) can ever contain this marker, so a non-empty drop-in forces
            // `Conflict` whenever `decide` reaches a text comparison at all — the one branch that
            // doesn't (a stale version, checked before any text comparison) is `apply_dropin_override`'s
            // job.
            let dropin = dropin_marker(id)?;
            let dropin_present = dropin.is_some();
            let on_disk = match &dropin {
                Some(marker) => format!("{text}{marker}"),
                None => text.clone(),
            };

            let ownership = match generate::extract(&text) {
                Ok(None) => Ownership::Foreign,
                Ok(Some(blob)) => match identity_for(&blob.spec.user) {
                    Ok(identity) => {
                        let regenerated = generate::unit(&blob.spec, &identity);
                        Ownership::Ours { blob, regenerated }
                    }
                    // An embedded spec naming a SID user is not decodable into anything this backend
                    // can regenerate — surfaced the same way any other blob invariant violation is.
                    Err(e) => Ownership::OursUnreadable { reason: e.to_string() },
                },
                Err(e) => Ownership::OursUnreadable { reason: e.to_string() },
            };
            Ok(Discovery {
                ownership,
                on_disk: Some(on_disk),
                fragment_text: Some(text),
                dropin_present,
            })
        }
    }
}

/// Closes the one drop-in case `discover`'s on-disk folding cannot reach: `decide`'s version-mismatch
/// check (`Outcome::Stale`) fires unconditionally, before it ever compares `on_disk` against anything
/// — see `src/decide.rs`. Without this, a drop-in surviving a version bump is silently absorbed into
/// a routine "regenerated" message instead of being reported as the drift obligation 3 exists to
/// surface. Every other `decide` outcome already accounts for the drop-in correctly through the
/// folded text, so this only ever touches `Stale`.
fn apply_dropin_override(outcome: Outcome, discovery: &Discovery, force: bool) -> Outcome {
    if !discovery.dropin_present {
        return outcome;
    }
    match outcome {
        Outcome::Stale { .. } if !force => {
            let regenerated = match &discovery.ownership {
                Ownership::Ours { regenerated, .. } => regenerated.as_str(),
                _ => "",
            };
            let on_disk = discovery.on_disk.as_deref().unwrap_or("");
            Outcome::Conflict {
                artifact_diff: crate::diff::artifact_diff(regenerated, on_disk),
            }
        }
        other => other,
    }
}

/// A deterministic representation of `<id>.service.d`'s `*.conf` files — the only ones
/// `systemd.unit(5)` reads as drop-ins — or `None` if the directory does not exist or holds none.
fn dropin_marker(id: &str) -> Result<Option<String>> {
    let dir = dropin_dir(id);
    let mut entries = match fs::read_dir(&dir) {
        Ok(rd) => rd
            .collect::<io::Result<Vec<_>>>()
            .map_err(|e| io_err("read", &dir, e))?,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(io_err("read", &dir, e)),
    };
    entries.sort_by_key(std::fs::DirEntry::file_name);

    let mut marker = String::new();
    for entry in &entries {
        let file_name = entry.file_name();
        if !file_name.to_string_lossy().ends_with(".conf") {
            continue;
        }
        let path = entry.path();
        // `fs::metadata` follows symlinks, deliberately unlike `raw_state`'s `lstat` of the fragment
        // itself: systemd follows a drop-in symlink exactly like a regular file when applying
        // overrides (common under ansible/stow/nix-managed `/etc`), so drift detection must too.
        let meta = match fs::metadata(&path) {
            Ok(m) => m,
            Err(e) if e.kind() == io::ErrorKind::NotFound => continue, // dangling symlink
            Err(e) => return Err(io_err("stat", &path, e)),
        };
        if !meta.is_file() {
            continue;
        }
        let content = fs::read_to_string(&path).map_err(|e| io_err("read", &path, e))?;
        marker.push_str(&format!(
            "\n# --- drop-in: {} ---\n{content}",
            file_name.to_string_lossy()
        ));
    }
    Ok(if marker.is_empty() { None } else { Some(marker) })
}

/// The narrower "is this even ours" gate every verb but `install` needs: the marker alone is proof of
/// ownership, matching [`crate::manager::fake::Fake`]'s `require_ours` (an undecodable blob still
/// passes — `uninstall`'s recovery text names exactly that verb as the way out). Returns the
/// fragment's own text for a caller that goes on to remove or replace it — see
/// `quarantine_if_still_ours`.
fn require_installed(id: &str) -> Result<String> {
    match raw_state(id)? {
        RawState::Absent => Err(Error::NotInstalled { id: id.to_string() }),
        RawState::NonRegular => Err(Error::Foreign {
            id: id.to_string(),
            recovery: decide::foreign_recovery(id),
        }),
        RawState::Regular(text) => match generate::extract(&text) {
            Ok(None) => Err(Error::Foreign {
                id: id.to_string(),
                recovery: decide::foreign_recovery(id),
            }),
            Ok(Some(_)) | Err(_) => Ok(text),
        },
    }
}

// Writing =============================================================================================================

enum CreateOutcome {
    Created,
    /// Something now exists at the target path that wasn't there when `install` classified it —
    /// obligation 1. The caller re-discovers and re-decides rather than clobbering it.
    Raced,
}

enum ReplaceOutcome {
    Replaced,
    /// The fragment at `id` is no longer the one `install` classified — see
    /// `quarantine_if_still_ours`. The caller re-discovers and re-decides.
    Raced,
}

/// Write `text` as a brand-new unit at `id`, never replacing an existing file — see the module doc
/// comment's obligation 1.
fn create_unit(id: &str, text: &str) -> Result<CreateOutcome> {
    let final_path = unit_path(id);
    let tmp = write_temp_unit(id, text)?;
    match tmp.persist_noclobber(&final_path) {
        Ok(_file) => {
            fsync_unit_dir()?;
            Ok(CreateOutcome::Created)
        }
        Err(e) if e.error.kind() == io::ErrorKind::AlreadyExists => Ok(CreateOutcome::Raced),
        Err(e) => Err(io_err("create", &final_path, e.error)),
    }
}

/// Overwrite `id`'s unit with `text`, but only if the fragment currently at `unit_path(id)` still has
/// exactly the content `expected_text` names — closing the same TOCTOU obligation 1 already closes
/// for `create_unit`, reached here from the `Update`/`Stale` arm instead: the gap between
/// classification and this write spans `decide`, `ensure_parent_dirs` (which can spawn a `chown`
/// subprocess), and building the replacement's contents, wide enough for something else to land at
/// this exact path in between (e.g. a concurrent `systemctl mask` on this same, already-managed id).
///
/// There is no kernel-level "replace only if identity X" primitive, so this is done in two safe
/// `rename`s: quarantine the current occupant under a private name and verify its content
/// (`quarantine_if_still_ours`), then place the new content and either discard the quarantined file
/// (verified match) or put it back (mismatch, reported as a race to retry).
fn replace_unit_verified(id: &str, text: &str, expected_text: &str) -> Result<ReplaceOutcome> {
    let Some(backup_path) = quarantine_if_still_ours(id, expected_text)? else {
        return Ok(ReplaceOutcome::Raced);
    };

    let final_path = unit_path(id);
    let tmp = write_temp_unit(id, text)?;
    match tmp.persist_noclobber(&final_path) {
        Ok(_file) => {
            fsync_unit_dir()?;
            remove_file_if_present(&backup_path)?;
            Ok(ReplaceOutcome::Replaced)
        }
        Err(e) if e.error.kind() == io::ErrorKind::AlreadyExists => {
            // Someone else placed a *third* file at `final_path` in the moment it was empty between
            // the quarantine above and this write. Restoring the quarantined original here would
            // clobber that third file — exactly the bug this function exists to prevent — so this
            // reports the situation instead of guessing which side should win.
            Err(Error::Other(format!(
                "install for `{id}` raced twice: the original fragment is safely quarantined at {}, \
                 but a different file has since appeared at {}; resolve manually and re-run install",
                backup_path.display(),
                final_path.display(),
            )))
        }
        Err(e) => Err(io_err("replace", &final_path, e.error)),
    }
}

/// Rename `unit_path(id)` to a private backup name, then confirm the moved file's content is still
/// exactly `expected_text` — the same content `discover`/`require_installed` classified as ours. If
/// it is not (or the path no longer exists at all), the backup is put back where it came from and
/// this returns `Ok(None)`: something changed after classification, which the caller must treat as a
/// race, not a license to keep going. On a match, `Ok(Some(backup_path))` — the verified former
/// occupant, now safely out of the way for the caller to finish with (replace it with new content, or
/// delete it outright).
///
/// Content, not inode, is the identity checked: a file's inode number can be reused by the kernel
/// moments after it is unlinked (confirmed directly — a test using inode comparison here saw a freshly
/// unlinked-and-recreated file reported as "unchanged"), so two genuinely different files can share
/// one. Comparing the actual bytes is both simpler and immune to that, and it is what this whole
/// system already treats as an artifact's identity everywhere else (`decide`'s entire vocabulary is
/// text).
fn quarantine_if_still_ours(id: &str, expected_text: &str) -> Result<Option<PathBuf>> {
    let final_path = unit_path(id);
    let backup_path = Path::new(UNIT_DIR).join(format!(".{id}.service.goetia-quarantine"));

    match fs::rename(&final_path, &backup_path) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(io_err("quarantine", &final_path, e)),
    }

    let actual = fs::read_to_string(&backup_path).map_err(|e| io_err("read", &backup_path, e))?;
    if actual == expected_text {
        Ok(Some(backup_path))
    } else {
        fs::rename(&backup_path, &final_path).map_err(|e| io_err("restore", &backup_path, e))?;
        Ok(None)
    }
}

/// A temp file in `UNIT_DIR` itself (so the later `persist`/`persist_noclobber` is a same-filesystem
/// link, never a cross-device copy), containing `text`, already `chmod`ed 0644 (obligation 4) and
/// `fsync`ed to stable storage — `NamedTempFile`'s `Write` impl forwards `flush()` straight to
/// `std::fs::File`, whose `flush` is a documented no-op, so without a real `sync_all()` here a crash
/// between this write and the later rename can leave the fragment on disk fully or partially
/// zero-filled: no `[X-Goetia]` marker, `extract` returns `Ok(None)`, and every verb then refuses an
/// id goetia itself created.
fn write_temp_unit(id: &str, text: &str) -> Result<tempfile::NamedTempFile> {
    let dir = Path::new(UNIT_DIR);
    let mut tmp = tempfile::Builder::new()
        .prefix(&format!(".goetia-{id}-"))
        .suffix(".service.tmp")
        .tempfile_in(dir)
        .map_err(|e| io_err("create a temp file in", dir, e))?;
    tmp.write_all(text.as_bytes())
        .map_err(|e| io_err("write", tmp.path(), e))?;
    tmp.as_file().sync_all().map_err(|e| io_err("fsync", tmp.path(), e))?;
    tmp.as_file()
        .set_permissions(fs::Permissions::from_mode(0o644))
        .map_err(|e| io_err("chmod", tmp.path(), e))?;
    Ok(tmp)
}

/// `fsync`s `UNIT_DIR` itself, so a just-completed rename's new directory entry is durable — a file's
/// own `fsync` (see `write_temp_unit`) says nothing about the directory entry pointing to it.
fn fsync_unit_dir() -> Result<()> {
    let dir = Path::new(UNIT_DIR);
    let f = fs::File::open(dir).map_err(|e| io_err("open", dir, e))?;
    f.sync_all().map_err(|e| io_err("fsync", dir, e))
}

/// Create (and, for a non-root account, `chown`) the parent of `logs` and the `cwd` directory itself
/// — obligation 5. Runs while still elevated, at install time, so a later `start` never fails with an
/// opaque status over a directory the target account could not write.
fn ensure_parent_dirs(spec: &DaemonSpec) -> Result<()> {
    if let Some(logs) = &spec.logs {
        if let Some(parent) = logs.parent() {
            if !parent.as_os_str().is_empty() {
                ensure_writable_dir(parent, &spec.user)?;
            }
        }
    }
    if let Some(cwd) = &spec.cwd {
        ensure_writable_dir(cwd, &spec.user)?;
    }
    Ok(())
}

/// Create `dir` (and any missing parents), `chmod`ing and `chown`ing only the path components this
/// call actually creates. `spec.cwd`/`logs` are user-supplied absolute paths with no restriction to a
/// Goetia-owned root, so reassigning mode/ownership unconditionally — including when `dir` or an
/// ancestor already existed — would let a manifest silently widen or reassign an arbitrary existing
/// system directory (`/var/log`, `/etc`, another user's home). A directory that already existed is
/// left exactly as it was; if the target account cannot write it, that is reported as an error
/// instead.
fn ensure_writable_dir(dir: &Path, user: &User) -> Result<()> {
    let dir_preexisted = fs::symlink_metadata(dir).is_ok();

    let mut missing = Vec::new();
    let mut cursor = dir;
    loop {
        match fs::symlink_metadata(cursor) {
            Ok(_) => break,
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                missing.push(cursor);
                match cursor.parent() {
                    Some(parent) if !parent.as_os_str().is_empty() => cursor = parent,
                    _ => break,
                }
            }
            Err(e) => return Err(io_err("stat", cursor, e)),
        }
    }

    let owner = chown_target(user);
    // Outermost-first, so each `create_dir` always has an existing parent.
    for path in missing.into_iter().rev() {
        fs::create_dir(path).map_err(|e| io_err("create directory", path, e))?;
        fs::set_permissions(path, fs::Permissions::from_mode(0o755)).map_err(|e| io_err("chmod", path, e))?;
        if let Some(owner) = &owner {
            chown(path, owner)?;
        }
    }

    if dir_preexisted {
        verify_writable_by(dir, user)?;
    }
    Ok(())
}

/// Whether the account `user` names can write to the already-existing `dir`, checked by literally
/// asking the OS as that account (`runuser`) rather than hand-rolling permission-bit/ACL math.
/// `dir`'s mode/ownership is never touched here — see `ensure_writable_dir`'s doc comment for why —
/// so this exists purely to surface the same opaque-status-at-start failure obligation 5 exists to
/// avoid, at install time instead of discovered later by the daemon itself.
fn verify_writable_by(dir: &Path, user: &User) -> Result<()> {
    let User::Root = user else {
        let account = chown_target(user).expect("a SID user is rejected by identity_for before this is reached");
        let output = Command::new("runuser")
            .args(["-u", &account, "--", "test", "-w"])
            .arg(dir)
            .output()
            .map_err(|e| Error::Other(format!("failed to spawn runuser to verify {}: {e}", dir.display())))?;
        if !output.status.success() {
            return Err(Error::Other(format!(
                "{} already exists and is not writable by `{account}`; goetia only creates and owns \
                 directories it did not find already present, so fix its ownership/permissions \
                 manually, or point `cwd`/`logs` elsewhere",
                dir.display()
            )));
        }
        return Ok(());
    };
    Ok(())
}

fn chown(path: &Path, owner: &str) -> Result<()> {
    let output = Command::new("chown")
        .arg(format!("{owner}:"))
        .arg(path)
        .output()
        .map_err(|e| Error::Other(format!("failed to spawn chown for {}: {e}", path.display())))?;
    if !output.status.success() {
        return Err(Error::Other(format!(
            "chown {owner}: {} failed: {}",
            path.display(),
            String::from_utf8_lossy(&output.stderr)
        )));
    }
    Ok(())
}

/// The account name/uid to `chown` a directory to so a non-root daemon can use it, or `None` for
/// `User::Root` (directories created by this — already-elevated — process are already root-owned).
fn chown_target(user: &User) -> Option<String> {
    match user {
        User::Root => None,
        User::Name(name) => Some(name.clone()),
        User::Id(AccountId::Uid(uid)) => Some(uid.to_string()),
        // `identity_for` rejects a SID user before `install`/`preview_install` ever reaches
        // `ensure_parent_dirs`, so a validated `spec.user` reaching this arm is impossible — not a
        // silently-skipped case.
        User::Id(AccountId::Sid(_)) => unreachable!("a SID user must already have been rejected by identity_for"),
    }
}

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

// systemctl ===========================================================================================================

fn run_systemctl(args: &[&str]) -> Result<std::process::Output> {
    Command::new("systemctl")
        .args(args)
        .output()
        .map_err(|e| Error::Other(format!("failed to run `systemctl {}`: {e}", args.join(" "))))
}

fn daemon_reload() -> Result<()> {
    let output = run_systemctl(&["daemon-reload"])?;
    if output.status.success() {
        Ok(())
    } else {
        Err(Error::Other(format!(
            "systemctl daemon-reload failed: {}",
            String::from_utf8_lossy(&output.stderr)
        )))
    }
}

/// `daemon-reload` after a unit file write that itself succeeded. A failure here must not be reported
/// as a successful install — the on-disk artifact and systemd's loaded view of it have diverged.
fn daemon_reload_or_report(id: &str) -> Result<()> {
    daemon_reload().map_err(|e| {
        Error::Other(format!(
            "wrote the unit for `{id}` but `systemctl daemon-reload` failed, so systemd may not have \
             picked it up yet: {e}"
        ))
    })
}

/// `systemctl start` blocks until its job completes — the real synchronization primitive, no polling
/// needed. Idempotent: starting an already-active unit is a no-op that still exits 0.
fn start_impl(unit: &str) -> Result<()> {
    let output = run_systemctl(&["start", unit])?;
    if output.status.success() {
        Ok(())
    } else {
        Err(Error::Other(format!(
            "systemctl start {unit} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        )))
    }
}

/// Idempotent per `ServiceManager::stop`'s doc comment. Exit code 5 ("unit not loaded") means there
/// was nothing to stop — the same convention `tests/support/service_guard.rs` already uses for
/// cleanup — which is success here, not a failure to stop something that was never running.
fn stop_impl(unit: &str) -> Result<()> {
    let output = run_systemctl(&["stop", unit])?;
    if output.status.success() || output.status.code() == Some(5) {
        Ok(())
    } else {
        Err(Error::Other(format!(
            "systemctl stop {unit} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        )))
    }
}

fn show_properties(unit: &str, props: &[&str]) -> Result<BTreeMap<String, String>> {
    let joined = props.join(",");
    let output = run_systemctl(&["show", "--property", &joined, unit])?;
    if !output.status.success() {
        return Err(Error::Other(format!(
            "systemctl show {unit} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        )));
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut map = BTreeMap::new();
    for line in stdout.lines() {
        if let Some((k, v)) = line.split_once('=') {
            map.insert(k.to_string(), v.to_string());
        }
    }
    Ok(map)
}

/// `UnitFileState` is the same install-state systemd derives for `systemctl is-enabled` — one query
/// covers state, pid, and boot-enablement together.
fn status_from_unit(unit: &str) -> Result<Status> {
    let props = show_properties(unit, &["ActiveState", "MainPID", "UnitFileState"])?;
    let state = match props.get("ActiveState").map(String::as_str) {
        Some("active") => State::Running,
        Some("inactive") => State::Stopped,
        Some("failed") => State::Failed,
        _ => State::Unknown,
    };
    let pid = props
        .get("MainPID")
        .and_then(|s| s.parse::<u32>().ok())
        .filter(|&p| p != 0);
    let enabled = props.get("UnitFileState").is_some_and(|s| s == "enabled");
    Ok(Status { state, pid, enabled })
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
