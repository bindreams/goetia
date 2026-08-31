//! The effectful launchd backend: [`LaunchdManager`].
//!
//! Every verb is described in full in the crate-level design notes on
//! launchd enablement; the short version is that the plist's *directory* is
//! the enrollment bit, never a plist key:
//!
//! - `install` writes/updates the plist in [`STAGING_DIR`] (or, for an
//!   update, wherever the existing artifact already lives — see
//!   [`discover`]). launchd never scans either directory on its own, so
//!   nothing loads and nothing runs.
//! - `start` `launchctl bootstrap`s the plist from wherever it currently is
//!   (if not already loaded), then `launchctl kickstart`s it — the second
//!   step is what actually launches a job with no `KeepAlive`/`RunAtLoad`
//!   (a `restart: never` daemon).
//! - `stop` `launchctl bootout`s the job. Works regardless of which
//!   directory it was loaded from.
//! - `enable` moves the plist from [`STAGING_DIR`] into [`ENABLED_DIR`]
//!   (`/Library/LaunchDaemons`). Does not start it.
//! - `disable` `bootout`s the job if loaded, then moves the plist back to
//!   [`STAGING_DIR`].
//! - Discovery (`list`, and every verb's ownership check) scans both
//!   directories directly — never `launchctl print`, whose textual format
//!   Apple does not guarantee. `status`'s live run state is the one
//!   exception: there is no filesystem signal for "is this job currently
//!   running", so it does read `launchctl print`'s output, but leniently —
//!   an unrecognized shape degrades to [`State::Unknown`] rather than
//!   panicking or misreporting, so a future macOS changing that format
//!   cannot turn into a wrong answer, only a vaguer one.
//!
//! Only the modern verbs (`bootstrap`/`bootout`/`kickstart`/`print`) are
//! used — never legacy `load`/`unload` — and neither `launchctl enable`/
//! `disable` nor the plist's `Disabled` key is ever touched: the override
//! database entries the former creates cannot be removed, and `bootstrap`
//! refuses a `Disabled` plist outright (see the crate-level design notes).

use std::io::Write as _;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::{fs, io};

use crate::backend::Identity;
use crate::backend::launchd::generate;
use crate::decide::{self, Outcome, Ownership};
use crate::error::{Error, Result};
use crate::manager::{Installed, ServiceManager, State, Status};
use crate::spec::{AccountId, DaemonSpec, Id, User};

/// Where `install` writes a plist that is not (yet, or any longer) enabled
/// at boot. launchd never scans this directory, so a job living here is
/// inert until `start` bootstraps it explicitly or `enable` moves it into
/// [`ENABLED_DIR`].
pub const STAGING_DIR: &str = "/Library/Application Support/Goetia/daemons";

/// The standard system LaunchDaemons directory. A plist here is loaded
/// automatically at boot; whether it currently *is* the enrollment bit this
/// whole backend is built around.
pub const ENABLED_DIR: &str = "/Library/LaunchDaemons";

pub struct LaunchdManager;

impl LaunchdManager {
    pub fn new() -> Self {
        Self
    }
}

impl Default for LaunchdManager {
    fn default() -> Self {
        Self::new()
    }
}

// Paths and discovery =================================================================================================

fn staging_path(id: &str) -> PathBuf {
    Path::new(STAGING_DIR).join(format!("{id}.plist"))
}

fn enabled_path(id: &str) -> PathBuf {
    Path::new(ENABLED_DIR).join(format!("{id}.plist"))
}

fn target(id: &str) -> String {
    format!("system/{id}")
}

/// Where `id`'s plist currently lives, and whether that location is the
/// enabled one. `Ok(None)` if it lives in neither directory.
///
/// `Err` if a plist exists at **both** paths — a state none of this
/// backend's own operations can produce (`enable`/`disable` always move the
/// file, never copy it), but one that a hand-edit, a crash mid-move across
/// a filesystem boundary the two directories do not normally share, or
/// direct tampering could still leave behind. Silently preferring one over
/// the other would make the loser's content simply disappear from every
/// command that reads it; refusing and naming both paths is the honest
/// answer.
struct Location {
    path: PathBuf,
    enabled: bool,
}

fn locate(id: &str) -> Result<Option<Location>> {
    let staging = staging_path(id);
    let enabled = enabled_path(id);
    match (staging.is_file(), enabled.is_file()) {
        (false, false) => Ok(None),
        (true, false) => Ok(Some(Location {
            path: staging,
            enabled: false,
        })),
        (false, true) => Ok(Some(Location {
            path: enabled,
            enabled: true,
        })),
        (true, true) => Err(Error::Other(format!(
            "daemon `{id}` has a plist in both {STAGING_DIR} and {ENABLED_DIR}; remove one by hand \
             (they should never both exist) before retrying"
        ))),
    }
}

fn read_to_string(path: &Path) -> Result<String> {
    fs::read_to_string(path).map_err(|source| Error::Io {
        path: path.to_path_buf(),
        source,
    })
}

/// What [`install`](ServiceManager::install) needs to decide anything:
/// [`decide::decide`]'s three inputs, plus the location discovery already
/// had to read to produce them (so a subsequent write does not have to
/// re-derive it).
struct Discovery {
    found: Ownership,
    on_disk: Option<String>,
    location: Option<Location>,
}

/// Classify what's at `id`, exactly as [`decide::decide`] needs it.
///
/// A marked artifact whose blob decodes is `Ownership::Ours` only if the
/// account *its own* `user` field names can still be resolved on this host
/// — `regenerated` has to be `generate(blob.spec)` under the identity that
/// spec's own user resolves to, not under whatever identity the caller is
/// currently installing, since those two can differ (see
/// `resolve_account`'s doc comment). An account that no longer exists (a
/// user deleted after install) is exactly the same kind of "ours, but we
/// cannot fully process it" situation an undecodable blob is — bricking
/// `list`/`status`/`uninstall` for a service the account gap doesn't stop
/// existing would be strictly worse than surfacing it as
/// `OursUnreadable`.
fn discover(id: &str) -> Result<Discovery> {
    let Some(location) = locate(id)? else {
        return Ok(Discovery {
            found: Ownership::Absent,
            on_disk: None,
            location: None,
        });
    };
    let text = read_to_string(&location.path)?;
    let found = match generate::extract(&text) {
        Ok(None) => Ownership::Foreign,
        Ok(Some(blob)) => match resolve_account(&blob.spec.user) {
            Ok(account) => Ownership::Ours {
                regenerated: generate::plist(&blob.spec, &account.identity()),
                blob,
            },
            Err(e) => Ownership::OursUnreadable { reason: e.to_string() },
        },
        Err(e) => Ownership::OursUnreadable { reason: e.to_string() },
    };
    Ok(Discovery {
        found,
        on_disk: Some(text),
        location: Some(location),
    })
}

/// The narrower "is this even ours" gate every verb but `install` needs —
/// a marked-but-undecodable artifact still passes, since the marker alone
/// is proof of ownership (and `uninstall` in particular has to be able to
/// operate on one; see `decide::Outcome::RefuseUnreadable`'s recovery
/// text). Only a plist with no Goetia marker at all is refused.
fn require_ours(text: &str, id: &Id) -> Result<()> {
    match generate::extract(text) {
        Ok(None) => Err(foreign(id)),
        Ok(Some(_)) | Err(_) => Ok(()),
    }
}

fn foreign(id: &Id) -> Error {
    Error::Foreign {
        id: id.as_str().to_string(),
        recovery: decide::foreign_recovery(id.as_str()),
    }
}

fn not_installed(id: &Id) -> Error {
    Error::NotInstalled {
        id: id.as_str().to_string(),
    }
}

// Account resolution ==================================================================================================

/// A platform account, resolved from [`crate::spec::User`]: the
/// [`Identity`] a generator needs, plus the uid/gid `install` needs to hand
/// a `cwd`/`logs` directory to its owner.
#[derive(Debug)]
struct Account {
    name: String,
    uid: nix::unistd::Uid,
    gid: nix::unistd::Gid,
}

impl Account {
    fn identity(&self) -> Identity {
        Identity {
            user: self.name.clone(),
        }
    }
}

impl From<nix::unistd::User> for Account {
    fn from(u: nix::unistd::User) -> Self {
        Account {
            name: u.name,
            uid: u.uid,
            gid: u.gid,
        }
    }
}

/// Resolve a [`User`] to a real macOS account. Always a *name* —
/// `launchd.plist(5)`'s `UserName` key is undocumented as to whether a
/// numeric string is accepted at all, so rather than gamble on it this
/// resolves every case (including `User::Id(AccountId::Uid(_))`) to the
/// account's actual name via a passwd lookup. That sidesteps the question
/// entirely instead of answering it, and it is what identity resolution
/// being effectful (see the crate-level design notes) exists to allow.
fn resolve_account(user: &User) -> Result<Account> {
    match user {
        User::Root => account_from_uid(0),
        User::Name(name) => nix::unistd::User::from_name(name)
            .map_err(|e| Error::Other(format!("look up user `{name}`: {e}")))?
            .map(Account::from)
            .ok_or_else(|| Error::Other(format!("no such user `{name}` on this host"))),
        User::Id(AccountId::Uid(uid)) => account_from_uid(*uid),
        User::Id(AccountId::Sid(sid)) => Err(Error::Other(format!(
            "user `{{id: {sid}}}` names a Windows SID, which is not meaningful on macOS"
        ))),
    }
}

fn account_from_uid(uid: u32) -> Result<Account> {
    nix::unistd::User::from_uid(nix::unistd::Uid::from_raw(uid))
        .map_err(|e| Error::Other(format!("look up uid {uid}: {e}")))?
        .map(Account::from)
        .ok_or_else(|| Error::Other(format!("no such uid {uid} on this host")))
}

// Writing the plist ===================================================================================================

fn io_err(path: impl Into<PathBuf>) -> impl FnOnce(io::Error) -> Error {
    let path = path.into();
    move |source| Error::Io { path, source }
}

/// `create_dir_all` the directory a plist is about to be written into, then
/// make sure it is `0755` regardless of the caller's umask — `list`/
/// `status`/`show` must work unelevated (see the design spec's §4), which
/// needs every ancestor to stay world-readable+searchable.
fn ensure_dir(dir: &Path) -> Result<()> {
    fs::create_dir_all(dir).map_err(io_err(dir))?;
    fs::set_permissions(dir, fs::Permissions::from_mode(0o755)).map_err(io_err(dir))
}

/// `create_dir_all` `cwd` itself (its own contents are what `WorkingDirectory`
/// needs to exist) and the *parent* of `logs` (a file path; launchd creates
/// the leaf log file itself but not its containing directory) — both
/// world-readable-and-searchable, then handed to `account` so a non-root
/// daemon can actually write there. Without this, a `user: someuser` daemon
/// with `StandardOutPath` pointed at a directory only root can write into
/// fails at launch with an opaque status, and `WorkingDirectory` naming a
/// directory that does not exist refuses the job outright.
fn prepare_parent_dirs(spec: &DaemonSpec, account: &Account) -> Result<()> {
    if let Some(cwd) = &spec.cwd {
        ensure_dir_owned_by(cwd, account)?;
    }
    if let Some(logs) = &spec.logs
        && let Some(parent) = logs.parent()
        && !parent.as_os_str().is_empty()
    {
        ensure_dir_owned_by(parent, account)?;
    }
    Ok(())
}

fn ensure_dir_owned_by(dir: &Path, account: &Account) -> Result<()> {
    ensure_dir(dir)?;
    nix::unistd::chown(dir, Some(account.uid), Some(account.gid))
        .map_err(|e| Error::Other(format!("chown {} to {}: {e}", dir.display(), account.name)))
}

/// Build a `0644` named temp file in `dir` (so a later hard-link-based
/// persist stays on the same filesystem) carrying `content`, and check it
/// with `plutil -lint` before handing it back. Because `install` no longer
/// bootstraps the plist (see the module doc comment), it loses the
/// validation `launchctl bootstrap` used to provide for free; this is what
/// stands in for it, catching malformed XML here instead of at the first
/// `start`.
fn staged_tempfile(dir: &Path, content: &str) -> Result<tempfile::NamedTempFile> {
    ensure_dir(dir)?;
    let mut tmp = tempfile::Builder::new()
        .prefix(".goetia-")
        .suffix(".plist.tmp")
        .tempfile_in(dir)
        .map_err(io_err(dir))?;
    tmp.write_all(content.as_bytes()).map_err(io_err(tmp.path()))?;
    fs::set_permissions(tmp.path(), fs::Permissions::from_mode(0o644)).map_err(io_err(tmp.path()))?;
    lint(tmp.path())?;
    Ok(tmp)
}

fn lint(path: &Path) -> Result<()> {
    let path_str = path
        .to_str()
        .expect("temp plist path is UTF-8 (see generate::path_str)");
    let out = Command::new("plutil")
        .args(["-lint", path_str])
        .output()
        .map_err(|e| Error::Other(format!("spawn `plutil -lint`: {e}")))?;
    if out.status.success() {
        Ok(())
    } else {
        Err(Error::Other(format!(
            "generated plist failed `plutil -lint`: {}",
            String::from_utf8_lossy(&out.stderr)
        )))
    }
}

enum WriteNew {
    Written,
    /// Something appeared at the target path between discovery and this
    /// write (see `install`'s doc comment on non-clobbering create).
    Raced,
}

/// Create-only write for the `Ownership::Absent` case: refuses to replace
/// anything already at `target`, so a foreign plist (or a concurrent
/// installer's) dropped into the gap between discovery and this write
/// cannot be destroyed by an install whose entire stated purpose is
/// refusing exactly that. Implemented as hard-link-then-unlink
/// (`NamedTempFile::persist_noclobber`), the portable equivalent of Linux's
/// `RENAME_NOREPLACE`: the link is the atomic step, so a `Raced` result
/// means the destination genuinely did not exist a moment ago and does
/// now, not that this code lost a filesystem-level race it should have
/// won.
fn write_new(target: &Path, content: &str) -> Result<WriteNew> {
    let tmp = staged_tempfile(target.parent().expect("plist path always has a parent"), content)?;
    match tmp.persist_noclobber(target) {
        Ok(_file) => Ok(WriteNew::Written),
        Err(e) if e.error.kind() == io::ErrorKind::AlreadyExists => Ok(WriteNew::Raced),
        Err(e) => Err(Error::Io {
            path: target.to_path_buf(),
            source: e.error,
        }),
    }
}

/// Overwrite the plist already at `target` — used only for `Update`/`Stale`,
/// where `discover` has already established the id is `Ownership::Ours`, so
/// replacing it is exactly what was asked for.
fn write_existing(target: &Path, content: &str) -> Result<()> {
    let tmp = staged_tempfile(target.parent().expect("plist path always has a parent"), content)?;
    tmp.persist(target).map(drop).map_err(|e| Error::Io {
        path: target.to_path_buf(),
        source: e.error,
    })
}

// launchctl ===========================================================================================================

fn launchctl(args: &[&str]) -> Result<std::process::Output> {
    Command::new("launchctl")
        .args(args)
        .output()
        .map_err(|e| Error::Other(format!("spawn `launchctl {}`: {e}", args.join(" "))))
}

fn is_loaded(id: &str) -> Result<bool> {
    // Not "structured data" parsing (see the module doc comment): this only
    // ever looks at the exit code, never the body of `print`'s output.
    Ok(launchctl(&["print", &target(id)])?.status.success())
}

fn bootstrap(path: &Path) -> Result<()> {
    let path_str = path.to_str().expect("plist path is UTF-8 (see generate::path_str)");
    let out = launchctl(&["bootstrap", "system", path_str])?;
    if out.status.success() {
        Ok(())
    } else {
        Err(Error::Other(format!(
            "launchctl bootstrap system {}: {}",
            path.display(),
            String::from_utf8_lossy(&out.stderr)
        )))
    }
}

fn kickstart(id: &str) -> Result<()> {
    let out = launchctl(&["kickstart", &target(id)])?;
    if out.status.success() {
        Ok(())
    } else {
        Err(Error::Other(format!(
            "launchctl kickstart {}: {}",
            target(id),
            String::from_utf8_lossy(&out.stderr)
        )))
    }
}

/// Idempotent: a job that is not currently loaded is not a failure to stop.
/// See `ServiceManager::stop`'s doc comment for why every backend must
/// agree on this.
fn bootout_if_loaded(id: &str) -> Result<()> {
    if !is_loaded(id)? {
        return Ok(());
    }
    let out = launchctl(&["bootout", &target(id)])?;
    if out.status.success() {
        Ok(())
    } else {
        Err(Error::Other(format!(
            "launchctl bootout {}: {}",
            target(id),
            String::from_utf8_lossy(&out.stderr)
        )))
    }
}

/// Best-effort live state, read from `launchctl print`'s body — the one
/// place this backend does read it, since there is no filesystem signal
/// for "is this job currently running" (see the module doc comment). Never
/// fails: a `launchctl` that cannot be run at all, or output in a shape
/// this does not recognize, degrades to `(State::Unknown, None)` rather
/// than taking down `status`/`list` for every other entry.
fn query_live_state(id: &str) -> (State, Option<u32>) {
    let Ok(out) = launchctl(&["print", &target(id)]) else {
        return (State::Unknown, None);
    };
    if !out.status.success() {
        // Not loaded at all: a clean "not running", not "unknown" — this is
        // the state a service left staged-but-never-started, or bootout'd,
        // is expected to report.
        return (State::Stopped, None);
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let pid = find_field(&text, "pid").and_then(|s| s.trim().parse().ok());
    let state = match find_field(&text, "state").map(str::trim) {
        Some("running") => State::Running,
        Some(_) => State::Stopped,
        None => State::Unknown,
    };
    (state, pid)
}

fn find_field<'a>(text: &'a str, key: &str) -> Option<&'a str> {
    let prefix = format!("{key} = ");
    text.lines()
        .find_map(|line| line.trim_start().strip_prefix(prefix.as_str()))
}

// ServiceManager ======================================================================================================

impl ServiceManager for LaunchdManager {
    fn install(&self, spec: &DaemonSpec, force: bool) -> Result<Outcome> {
        let account = resolve_account(&spec.user)?;
        let desired = generate::plist(spec, &account.identity());
        let discovery = discover(spec.id.as_str())?;
        let outcome = decide::decide(
            &discovery.found,
            discovery.on_disk.as_deref(),
            &desired,
            spec,
            crate::version(),
            force,
        );

        match &outcome {
            Outcome::Create => {
                prepare_parent_dirs(spec, &account)?;
                let target = staging_path(spec.id.as_str());
                match write_new(&target, &desired)? {
                    WriteNew::Written => {}
                    WriteNew::Raced => {
                        // Something now exists where discovery saw nothing.
                        // Re-running `install` from scratch re-derives
                        // `Ownership` against what is actually there and
                        // reaches whatever `decide` says about *that* —
                        // reusing the exact same policy path every other
                        // call goes through, rather than a bespoke
                        // reclassify branch that could drift from it.
                        return self.install(spec, force);
                    }
                }
            }
            Outcome::Update { .. } | Outcome::Stale { .. } => {
                prepare_parent_dirs(spec, &account)?;
                let target = &discovery
                    .location
                    .as_ref()
                    .expect("Ownership::Ours implies discovery found a location")
                    .path;
                write_existing(target, &desired)?;
            }
            Outcome::UpToDate
            | Outcome::Conflict { .. }
            | Outcome::RefuseForeign { .. }
            | Outcome::RefuseUnreadable { .. } => {}
        }

        Ok(outcome)
    }

    fn preview_install(&self, spec: &DaemonSpec) -> Result<Outcome> {
        let account = resolve_account(&spec.user)?;
        let desired = generate::plist(spec, &account.identity());
        let discovery = discover(spec.id.as_str())?;
        Ok(decide::decide(
            &discovery.found,
            discovery.on_disk.as_deref(),
            &desired,
            spec,
            crate::version(),
            false,
        ))
    }

    fn uninstall(&self, id: &Id) -> Result<()> {
        let location = locate(id.as_str())?.ok_or_else(|| not_installed(id))?;
        let text = read_to_string(&location.path)?;
        require_ours(&text, id)?;

        bootout_if_loaded(id.as_str())?;
        fs::remove_file(&location.path).map_err(io_err(&location.path))
    }

    fn enable(&self, id: &Id) -> Result<()> {
        let location = locate(id.as_str())?.ok_or_else(|| not_installed(id))?;
        let text = read_to_string(&location.path)?;
        require_ours(&text, id)?;

        if location.enabled {
            return Ok(());
        }
        let dest = enabled_path(id.as_str());
        if dest.exists() {
            return Err(Error::Other(format!(
                "cannot enable `{id}`: {} already exists (this should never happen; remove it by hand)",
                dest.display()
            )));
        }
        fs::rename(&location.path, &dest).map_err(io_err(&dest))
    }

    fn disable(&self, id: &Id) -> Result<()> {
        let location = locate(id.as_str())?.ok_or_else(|| not_installed(id))?;
        let text = read_to_string(&location.path)?;
        require_ours(&text, id)?;

        if !location.enabled {
            return Ok(());
        }
        bootout_if_loaded(id.as_str())?;
        let dest = staging_path(id.as_str());
        if dest.exists() {
            return Err(Error::Other(format!(
                "cannot disable `{id}`: {} already exists (this should never happen; remove it by hand)",
                dest.display()
            )));
        }
        ensure_dir(Path::new(STAGING_DIR))?;
        fs::rename(&location.path, &dest).map_err(io_err(&dest))
    }

    fn start(&self, id: &Id) -> Result<()> {
        let location = locate(id.as_str())?.ok_or_else(|| not_installed(id))?;
        let text = read_to_string(&location.path)?;
        require_ours(&text, id)?;

        if !is_loaded(id.as_str())? {
            if let Err(e) = bootstrap(&location.path) {
                // A concurrent `start` may have loaded it between the check
                // above and this call; only propagate the error if the job
                // genuinely is not loaded now either.
                if !is_loaded(id.as_str())? {
                    return Err(e);
                }
            }
        }
        // `kickstart` is what actually launches a job with no
        // `KeepAlive`/`RunAtLoad` (`restart: never`) — `bootstrap` alone
        // only loads it. For every other `restart` policy, `bootstrap`
        // already started it via `RunAtLoad`, so this only calls
        // `kickstart` when the job is not already running: `start` on an
        // already-running service must be `Ok`, not restart it, and rather
        // than lean on an undocumented guarantee that a plain (non-`-k`)
        // `kickstart` never touches a running instance, checking first
        // makes that guarantee this code's own, not launchd's.
        let (state, _pid) = query_live_state(id.as_str());
        if state != State::Running {
            kickstart(id.as_str())?;
        }
        Ok(())
    }

    fn stop(&self, id: &Id) -> Result<()> {
        let location = locate(id.as_str())?.ok_or_else(|| not_installed(id))?;
        let text = read_to_string(&location.path)?;
        require_ours(&text, id)?;

        bootout_if_loaded(id.as_str())
    }

    fn status(&self, id: &Id) -> Result<Status> {
        let location = locate(id.as_str())?.ok_or_else(|| not_installed(id))?;
        let text = read_to_string(&location.path)?;
        match generate::extract(&text) {
            Ok(None) => Err(foreign(id)),
            Err(e) => Err(e),
            Ok(Some(_)) => {
                let (state, pid) = query_live_state(id.as_str());
                Ok(Status {
                    state,
                    pid,
                    enabled: location.enabled,
                })
            }
        }
    }

    fn list(&self) -> Result<Vec<Installed>> {
        let mut by_id: std::collections::BTreeMap<String, Vec<(PathBuf, bool)>> = std::collections::BTreeMap::new();
        for (dir, enabled) in [(STAGING_DIR, false), (ENABLED_DIR, true)] {
            let entries = match fs::read_dir(dir) {
                Ok(e) => e,
                // The staging directory does not exist until the first
                // `install` ever creates it.
                Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
                Err(e) => {
                    return Err(Error::Io {
                        path: PathBuf::from(dir),
                        source: e,
                    });
                }
            };
            for entry in entries {
                let entry = entry.map_err(io_err(dir))?;
                let path = entry.path();
                if path.extension().and_then(|e| e.to_str()) != Some("plist") {
                    continue;
                }
                let Some(id) = path.file_stem().and_then(|s| s.to_str()) else {
                    continue;
                };
                by_id.entry(id.to_string()).or_default().push((path, enabled));
            }
        }

        let mut out = Vec::new();
        for (id, locations) in by_id {
            if locations.len() > 1 {
                out.push(Installed::OursUnreadable {
                    name: id,
                    reason: format!("present in both {STAGING_DIR} and {ENABLED_DIR}"),
                });
                continue;
            }
            let (path, enabled) = &locations[0];
            let text = match fs::read_to_string(path) {
                Ok(t) => t,
                Err(e) => {
                    out.push(Installed::OursUnreadable {
                        name: id,
                        reason: format!("read {}: {e}", path.display()),
                    });
                    continue;
                }
            };
            match generate::extract(&text) {
                Ok(None) => {} // foreign: not Goetia-managed, omitted per the trait doc comment
                Ok(Some(blob)) => {
                    let (state, _pid) = query_live_state(&id);
                    out.push(Installed::Ours {
                        spec: blob.spec,
                        state,
                        enabled: *enabled,
                    });
                }
                Err(e) => out.push(Installed::OursUnreadable {
                    name: id,
                    reason: e.to_string(),
                }),
            }
        }
        Ok(out)
    }
}

#[cfg(test)]
#[path = "manager_tests.rs"]
mod manager_tests;
