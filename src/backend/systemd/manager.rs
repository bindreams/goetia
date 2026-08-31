//! The effectful half of the systemd backend: writes `/etc/systemd/system/<id>.service` and talks
//! to `systemctl`.
//!
//! Six obligations the review found missing from a naive classify-then-write implementation, each
//! with its own test (see `tests/systemd_integration.rs` and this module's own `manager_tests.rs`):
//!
//! 1. **Create must not clobber.** `rename(2)` unconditionally replaces its destination, so a plain
//!    classify-then-rename is a TOCTOU window: a package postinst or a concurrent invocation can drop
//!    a foreign unit into the gap and have it destroyed by the very code path that exists to refuse
//!    that. [`create_unit`] uses [`tempfile::NamedTempFile::persist_noclobber`] (`linkat(2)` without
//!    replace semantics) for the create case; [`replace_unit`] uses plain `persist`/`rename` only once
//!    a path is already classified as ours.
//! 2. **A masked unit is not absent.** `systemctl mask` replaces the fragment with a symlink to
//!    `/dev/null`; reading it yields empty text, and a naive read-then-extract would see `Ok(None)`
//!    and let `install` rename over it, silently unmasking a deliberately-masked service.
//!    [`raw_state`] `lstat`s the path and classifies any non-regular file as [`RawState::NonRegular`]
//!    — always `Ownership::Foreign` — without ever reading its contents.
//! 3. **Drop-ins are drift.** `systemctl edit` — the officially recommended way to add exactly the
//!    `MemoryMax=`/`After=` the design cites — writes `<id>.service.d/override.conf` and leaves the
//!    fragment itself byte-identical, so drift detection over the fragment alone misses it entirely.
//!    [`dropin_marker`] folds the drop-in directory's contents into the text handed to `decide` (never
//!    into what is actually written), so any drop-in forces `Conflict` regardless of whether the
//!    fragment changed. [`uninstall`] removes the directory, or it would poison the next install of
//!    the same id.
//! 4. **Permissions.** `NamedTempFile` is created mode 0600; after persisting, the unit would be
//!    root-only, breaking the promise that `list`/`show`/`diff` need no elevation. Both
//!    [`create_unit`] and [`replace_unit`] `chmod` 0644 before persisting.
//! 5. **Parent directories.** [`ensure_parent_dirs`] creates and, for a non-root account, `chown`s the
//!    parents of `logs` and `cwd` while still elevated — otherwise `StandardOutput=append:` fails the
//!    unit at start with an opaque status.
//! 6. **Uninstall order** is stop -> `systemctl disable` -> remove -> `daemon-reload`. Disabling after
//!    the fragment is gone is impossible (no `[Install]` section left to read), which would leave
//!    exactly the `.wants` symlink `uninstall_leaves_nothing` checks for.

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
        let identity = identity_for(&spec.user);
        let desired = generate::unit(spec, &identity);

        // Looping rather than a single attempt: `create_unit`'s `AlreadyExists` means something
        // appeared at this id between `discover` and the write below (obligation 1). That is a real,
        // detected state change to react to — not a wait for time to pass — so re-classifying and
        // trying again is bounded by actual system state, not a chosen number of attempts.
        loop {
            let (found, on_disk) = discover(spec.id.as_str())?;
            let outcome = decide::decide(&found, on_disk.as_deref(), &desired, spec, crate::version(), force);

            match &outcome {
                Outcome::Create => {
                    ensure_parent_dirs(spec)?;
                    match create_unit(spec.id.as_str(), &desired)? {
                        CreateOutcome::Created => {
                            daemon_reload_or_report(spec.id.as_str())?;
                            return Ok(outcome);
                        }
                        CreateOutcome::Raced => continue,
                    }
                }
                Outcome::Update { .. } | Outcome::Stale { .. } => {
                    // Already classified `Ours` by `discover` above: a plain replace is safe here,
                    // unlike the `Create` arm.
                    ensure_parent_dirs(spec)?;
                    replace_unit(spec.id.as_str(), &desired)?;
                    daemon_reload_or_report(spec.id.as_str())?;
                    return Ok(outcome);
                }
                // `UpToDate` / `Conflict` (without force) / `RefuseForeign` / `RefuseUnreadable`:
                // nothing to write.
                _ => return Ok(outcome),
            }
        }
    }

    fn preview_install(&self, spec: &DaemonSpec) -> Result<Outcome> {
        let identity = identity_for(&spec.user);
        let desired = generate::unit(spec, &identity);
        let (found, on_disk) = discover(spec.id.as_str())?;
        // Always previewed without `force` — see the trait doc comment.
        Ok(decide::decide(
            &found,
            on_disk.as_deref(),
            &desired,
            spec,
            crate::version(),
            false,
        ))
    }

    fn uninstall(&self, id: &Id) -> Result<()> {
        let id = id.as_str();
        require_installed(id)?;
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

        remove_file_if_present(&unit_path(id))?;
        remove_dir_if_present(&dropin_dir(id))?;

        daemon_reload()
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
                // A benign race with a concurrent uninstall between `read_dir` and this stat.
                Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
                Err(e) => return Err(io_err("stat", &entry.path(), e)),
            };
            if !file_type.is_file() {
                continue;
            }

            let path = entry.path();
            let text = match fs::read_to_string(&path) {
                Ok(t) => t,
                // A benign race with a concurrent uninstall between `read_dir` and this read.
                Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
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

// Identity ============================================================================================================

/// Resolve `user` to the account name systemd's `User=` directive wants. Root becomes `"0"`, exactly
/// as `cli::install`'s dry-run preview renders it, so a preview and a real install never disagree.
/// Unlike launchd/SCM, systemd needs no OS lookup here — it resolves usernames itself via NSS at
/// start time — so this is a pure formatting step rather than genuinely effectful I/O.
fn identity_for(user: &User) -> Identity {
    Identity {
        user: match user {
            User::Root => "0".to_string(),
            User::Name(name) => name.clone(),
            User::Id(AccountId::Uid(uid)) => uid.to_string(),
            User::Id(AccountId::Sid(sid)) => sid.clone(),
        },
    }
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

/// Classify what's at `id`, exactly as `install` needs to: an [`Ownership`] plus the on-disk text
/// `decide` compares against. `None` iff `Ownership::Absent`.
fn discover(id: &str) -> Result<(Ownership, Option<String>)> {
    match raw_state(id)? {
        RawState::Absent => Ok((Ownership::Absent, None)),
        RawState::NonRegular => Ok((Ownership::Foreign, Some(String::new()))),
        RawState::Regular(text) => {
            // Obligation 3: fold any drop-in content into the text `decide` compares, without ever
            // writing that folded text back. Neither `desired` nor `regenerated` (both pure
            // `generate()` output) can ever contain this marker, so a non-empty drop-in forces
            // `Conflict` unconditionally — exactly "drop-ins are drift".
            let on_disk = match dropin_marker(id)? {
                Some(marker) => format!("{text}{marker}"),
                None => text.clone(),
            };

            let found = match generate::extract(&text) {
                Ok(None) => Ownership::Foreign,
                Ok(Some(blob)) => {
                    let identity = identity_for(&blob.spec.user);
                    let regenerated = generate::unit(&blob.spec, &identity);
                    Ownership::Ours { blob, regenerated }
                }
                Err(e) => Ownership::OursUnreadable { reason: e.to_string() },
            };
            Ok((found, Some(on_disk)))
        }
    }
}

/// A deterministic representation of `<id>.service.d`'s contents, or `None` if the directory does not
/// exist or holds no regular files. Never `None` for a directory `systemctl edit` created — its
/// `override.conf` is exactly the case obligation 3 exists for.
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
        let file_type = entry.file_type().map_err(|e| io_err("stat", &entry.path(), e))?;
        if !file_type.is_file() {
            continue;
        }
        let path = entry.path();
        let content = fs::read_to_string(&path).map_err(|e| io_err("read", &path, e))?;
        marker.push_str(&format!(
            "\n# --- drop-in: {} ---\n{content}",
            entry.file_name().to_string_lossy()
        ));
    }
    Ok(if marker.is_empty() { None } else { Some(marker) })
}

/// The narrower "is this even ours" gate every verb but `install` needs: the marker alone is proof of
/// ownership, matching [`crate::manager::fake::Fake`]'s `require_ours` (an undecodable blob still
/// passes — `uninstall`'s recovery text names exactly that verb as the way out).
fn require_installed(id: &str) -> Result<()> {
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
            Ok(Some(_)) | Err(_) => Ok(()),
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

/// Write `text` as a brand-new unit at `id`, never replacing an existing file — see the module doc
/// comment's obligation 1.
fn create_unit(id: &str, text: &str) -> Result<CreateOutcome> {
    let final_path = unit_path(id);
    let tmp = write_temp_unit(id, text)?;
    match tmp.persist_noclobber(&final_path) {
        Ok(_file) => Ok(CreateOutcome::Created),
        Err(e) if e.error.kind() == io::ErrorKind::AlreadyExists => Ok(CreateOutcome::Raced),
        Err(e) => Err(io_err("create", &final_path, e.error)),
    }
}

/// Overwrite `id`'s unit with `text`. Only ever called once `id` has already been classified `Ours`
/// by `discover` in the same `install` call — see the module doc comment's obligation 1.
fn replace_unit(id: &str, text: &str) -> Result<()> {
    let final_path = unit_path(id);
    let tmp = write_temp_unit(id, text)?;
    tmp.persist(&final_path)
        .map_err(|e| io_err("replace", &final_path, e.error))?;
    Ok(())
}

/// A temp file in `UNIT_DIR` itself (so the later `persist`/`persist_noclobber` is a same-filesystem
/// link, never a cross-device copy), containing `text`, already `chmod`ed 0644 — obligation 4.
fn write_temp_unit(id: &str, text: &str) -> Result<tempfile::NamedTempFile> {
    let dir = Path::new(UNIT_DIR);
    let mut tmp = tempfile::Builder::new()
        .prefix(&format!(".goetia-{id}-"))
        .suffix(".service.tmp")
        .tempfile_in(dir)
        .map_err(|e| io_err("create a temp file in", dir, e))?;
    tmp.write_all(text.as_bytes())
        .map_err(|e| io_err("write", tmp.path(), e))?;
    tmp.flush().map_err(|e| io_err("flush", tmp.path(), e))?;
    tmp.as_file()
        .set_permissions(fs::Permissions::from_mode(0o644))
        .map_err(|e| io_err("chmod", tmp.path(), e))?;
    Ok(tmp)
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

fn ensure_writable_dir(dir: &Path, user: &User) -> Result<()> {
    fs::create_dir_all(dir).map_err(|e| io_err("create directory", dir, e))?;
    fs::set_permissions(dir, fs::Permissions::from_mode(0o755)).map_err(|e| io_err("chmod", dir, e))?;

    if let Some(owner) = chown_target(user) {
        let output = Command::new("chown")
            .arg(format!("{owner}:"))
            .arg(dir)
            .output()
            .map_err(|e| Error::Other(format!("failed to spawn chown for {}: {e}", dir.display())))?;
        if !output.status.success() {
            return Err(Error::Other(format!(
                "chown {owner}: {} failed: {}",
                dir.display(),
                String::from_utf8_lossy(&output.stderr)
            )));
        }
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
        // A SID is not a meaningful Linux account; nothing sensible to chown to.
        User::Id(AccountId::Sid(_)) => None,
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
