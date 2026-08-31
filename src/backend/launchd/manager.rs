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
//! - `disable` moves the plist back to [`STAGING_DIR`]. Does **not**
//!   `bootout` it —
//!   [`ServiceManager::disable`](crate::manager::ServiceManager::disable)'s
//!   contract is "does not stop it if running", which boot-enrollment and
//!   current run state being genuinely orthogonal here makes free: launchd
//!   holds a bootstrapped job by label regardless of which directory its
//!   plist currently lives in, so moving the file has no effect on a job
//!   already loaded.
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

/// Whether *anything* occupies `path` — any directory entry at all, not
/// just a regular file. `Path::is_file` is `false` for a directory, a
/// symlink, a socket, or any other non-regular entry, which would let
/// `locate` classify an occupied path as `Absent`; `write_new`'s
/// underlying `link`(2) refuses to create over *any* of those the same as
/// over a regular file (`EEXIST`), so that mismatch would send
/// `install` into `Create` -> `write_new` -> `Raced` -> re-`install` ->
/// the identical classification, forever. `symlink_metadata` (not
/// `metadata`, which follows symlinks and would report a *dangling* one
/// as absent) catches every case uniformly.
fn occupied(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok()
}

fn locate(id: &str) -> Result<Option<Location>> {
    let staging = staging_path(id);
    let enabled = enabled_path(id);
    match (occupied(&staging), occupied(&enabled)) {
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

/// Shared preamble for every verb but `install`: locate `id`, read its
/// artifact, and confirm it's ours. Factored out so the five call sites
/// (`uninstall`/`enable`/`disable`/`start`/`stop`) cannot independently
/// drift on what "found and ours" means.
fn located_and_ours(id: &Id) -> Result<(Location, String)> {
    let location = locate(id.as_str())?.ok_or_else(|| not_installed(id))?;
    let text = read_to_string(&location.path)?;
    require_ours(&text, id)?;
    Ok((location, text))
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
            .map_err(|e| Error::AccountLookup {
                detail: format!("look up user `{name}`: {e}"),
            })?
            .map(Account::from)
            .ok_or_else(|| Error::AccountLookup {
                detail: format!("no such user `{name}` on this host"),
            }),
        User::Id(AccountId::Uid(uid)) => account_from_uid(*uid),
        User::Id(AccountId::Sid(sid)) => Err(Error::AccountLookup {
            detail: format!("user `{{id: {sid}}}` names a Windows SID, which is not meaningful on macOS"),
        }),
    }
}

fn account_from_uid(uid: u32) -> Result<Account> {
    nix::unistd::User::from_uid(nix::unistd::Uid::from_raw(uid))
        .map_err(|e| Error::AccountLookup {
            detail: format!("look up uid {uid}: {e}"),
        })?
        .map(Account::from)
        .ok_or_else(|| Error::AccountLookup {
            detail: format!("no such uid {uid} on this host"),
        })
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

/// Only touches a directory this call itself creates. `dir` is an
/// arbitrary absolute path a spec author supplied (`spec::resolve`
/// guarantees only that it's absolute) — `chown`ing and `chmod 0755`ing it
/// unconditionally would hand ownership of, and strip the mode from,
/// whatever already happened to be there (`/var/log`, a user's home
/// directory, ...) on every single `install`. A directory that already
/// exists is therefore left completely untouched.
fn ensure_dir_owned_by(dir: &Path, account: &Account) -> Result<()> {
    if dir.exists() {
        return Ok(());
    }
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
    require_success("plutil", &["-lint", path_str])
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

/// Move `src` to `dest` without ever replacing something already at
/// `dest` — used by `enable`/`disable` to move the plist between
/// [`STAGING_DIR`] and [`ENABLED_DIR`]. A plain `fs::rename` silently
/// replaces an existing destination on POSIX, which would make a
/// same-shape TOCTOU clobber possible here exactly as it is for `install`'s
/// create path (see `write_new`'s doc comment) — a foreign plist, or a
/// concurrent operation's, landing at `dest` between the caller's own
/// checks and this call would otherwise be silently destroyed. Hard-link
/// then unlink, the same portable non-clobbering primitive `write_new`
/// uses via `persist_noclobber`: the link is the atomic step, so
/// `AlreadyExists` means `dest` genuinely was already occupied.
fn move_no_clobber(src: &Path, dest: &Path) -> Result<()> {
    match fs::hard_link(src, dest) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
            return Err(Error::AlreadyExists {
                path: dest.to_path_buf(),
            });
        }
        Err(e) => {
            return Err(Error::Io {
                path: dest.to_path_buf(),
                source: e,
            });
        }
    }
    fs::remove_file(src).map_err(io_err(src))
}

// launchctl ===========================================================================================================

/// Spawn `command` with `args`, giving back the full `Output` for a caller
/// that needs to inspect the exit code or streams itself (`is_loaded`,
/// `bootout`, `query_live_state`). Only the spawn failure — the process
/// could not be started at all — is an `Err` here; a non-zero exit is
/// reported through the returned `Output`.
fn run(command: &str, args: &[&str]) -> Result<std::process::Output> {
    Command::new(command)
        .args(args)
        .output()
        .map_err(|e| Error::CommandFailed {
            command: format!("{command} {}", args.join(" ")),
            stderr: e.to_string(),
        })
}

/// [`run`], but for a caller that only cares whether it succeeded
/// (`bootstrap`, `kickstart`, `plutil -lint`) — spawn failure and a
/// non-zero exit both become the same [`Error::CommandFailed`].
fn require_success(command: &str, args: &[&str]) -> Result<()> {
    let out = run(command, args)?;
    if out.status.success() {
        Ok(())
    } else {
        Err(Error::CommandFailed {
            command: format!("{command} {}", args.join(" ")),
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        })
    }
}

fn launchctl(args: &[&str]) -> Result<std::process::Output> {
    run("launchctl", args)
}

fn is_loaded(id: &str) -> Result<bool> {
    // Not "structured data" parsing (see the module doc comment): this only
    // ever looks at the exit code, never the body of `print`'s output.
    Ok(launchctl(&["print", &target(id)])?.status.success())
}

fn bootstrap(path: &Path) -> Result<()> {
    let path_str = path.to_str().expect("plist path is UTF-8 (see generate::path_str)");
    require_success("launchctl", &["bootstrap", "system", path_str])
}

fn kickstart(id: &str) -> Result<()> {
    require_success("launchctl", &["kickstart", &target(id)])
}

/// Exit codes `launchctl` uses for "no such job" — established empirically
/// in `tests/support/service_guard.rs`'s cleanup, which needs the same
/// distinction for the same underlying label lookup `bootout`/`print`
/// perform: 3 is "no such process", 113 is "could not find specified
/// service".
fn is_not_found(code: Option<i32>) -> bool {
    matches!(code, Some(3 | 113))
}

/// `bootout`, unconditionally — no `is_loaded` pre-check. A separate check
/// then act would leave a window for the job to unload between the two
/// (a concurrent `stop`, an operator's own `launchctl bootout`, ...), which
/// would fail this call for a service that is, in fact, already stopped —
/// breaking the idempotency [`ServiceManager::stop`] declares mandatory.
/// Calling `bootout` directly and classifying *its own* result — success,
/// or the "no such job" codes [`is_not_found`] names, both `Ok` — covers
/// the already-gone case by construction instead of by timing.
///
/// [`ServiceManager::stop`]: crate::manager::ServiceManager::stop
fn bootout(id: &str) -> Result<()> {
    let out = launchctl(&["bootout", &target(id)])?;
    if out.status.success() || is_not_found(out.status.code()) {
        Ok(())
    } else {
        Err(Error::CommandFailed {
            command: format!("launchctl bootout {}", target(id)),
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        })
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
        // `is_not_found` (established for `bootout`'s use of the same
        // label lookup): a clean "not running", the state a service left
        // staged-but-never-started, or bootout'd, is expected to report.
        // Anything else — most importantly an unprivileged caller's
        // permission failure querying the system domain — must not be
        // read as "stopped": `list`/`status` are required to work
        // unelevated (the design spec's §4), and reporting a daemon that
        // is in fact running as `Stopped` because the caller merely
        // couldn't ask is exactly the confidently-wrong answer the lenient
        // parse below exists to avoid.
        return if is_not_found(out.status.code()) {
            (State::Stopped, None)
        } else {
            (State::Unknown, None)
        };
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
            // launchd has no drop-in or override mechanism that alters a job
            // without touching its plist. Enablement lives in the plist's
            // *directory*, which is deliberately outside the compared surface.
            false,
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
                let target = discovery
                    .location
                    .as_ref()
                    .expect("Ownership::Ours implies discovery found a location")
                    .path
                    .clone();
                // `target` was captured by `discover`, before `decide` ran.
                // If a concurrent `enable`/`disable`/`uninstall` has since
                // moved or removed the artifact, writing here would
                // recreate a plist at the now-vacated path — landing it in
                // *both* directories, the exact ambiguous state `locate`
                // hard-errors on. Re-checking immediately before the write
                // narrows the race to the few instructions between this
                // check and `write_existing`'s own open, and on a detected
                // vanish, re-running `install` re-derives the correct
                // outcome against whatever is actually there now (the same
                // reclassify-via-recursion the `Create`/`Raced` case above
                // uses).
                if !occupied(&target) {
                    return self.install(spec, force);
                }
                write_existing(&target, &desired)?;
                if matches!(outcome, Outcome::Update { .. }) && is_loaded(spec.id.as_str())? {
                    // launchd holds the plist content it read at bootstrap
                    // time in memory; rewriting the file on disk does not
                    // reach an already-loaded job. Without this, a job
                    // that happens to still be loaded (its own process may
                    // long since have exited, for a `restart: never`
                    // daemon) would have `start` silently kickstart the
                    // *pre-update* command/env/user/cwd/logs forever,
                    // reporting `Ok` the whole time. Bootout is skipped for
                    // `Stale`: that outcome only ever changes the embedded
                    // metadata comment's `Version` field, never anything
                    // `generate::plist` derives from `spec` itself, so a
                    // loaded job is not stale in any way that affects it.
                    bootout(spec.id.as_str())?;
                }
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
            // launchd has no drop-in mechanism; see the `install` call site.
            false,
        ))
    }

    fn uninstall(&self, id: &Id) -> Result<()> {
        let (location, _text) = located_and_ours(id)?;

        bootout(id.as_str())?;
        fs::remove_file(&location.path).map_err(io_err(&location.path))
    }

    fn enable(&self, id: &Id) -> Result<()> {
        let (location, _text) = located_and_ours(id)?;

        if location.enabled {
            return Ok(());
        }
        let dest = enabled_path(id.as_str());
        move_no_clobber(&location.path, &dest)
    }

    /// Does not stop the job if it is loaded — see
    /// [`ServiceManager::disable`]'s doc comment ("Does not stop it if
    /// running"), which [`crate::manager::fake::Fake::disable`] already
    /// honours. launchd holds a bootstrapped job by label, independent of
    /// which directory its plist currently lives in (`stop`'s own doc
    /// comment: "works regardless of which directory it was loaded from"),
    /// so moving the file back to staging has no effect on a job already
    /// loaded — boot-enrollment and current run state are genuinely
    /// orthogonal here, exactly as the trait contract requires.
    ///
    /// [`ServiceManager::disable`]: crate::manager::ServiceManager::disable
    fn disable(&self, id: &Id) -> Result<()> {
        let (location, _text) = located_and_ours(id)?;

        if !location.enabled {
            return Ok(());
        }
        let dest = staging_path(id.as_str());
        ensure_dir(Path::new(STAGING_DIR))?;
        move_no_clobber(&location.path, &dest)
    }

    fn start(&self, id: &Id) -> Result<()> {
        let (location, text) = located_and_ours(id)?;
        let blob = generate::extract(&text)?.ok_or_else(|| foreign(id))?;

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

        // Verify the outcome, do not assume it.
        //
        // `is_loaded` and `query_live_state` both key on the *label*
        // (`system/<id>`) and cannot tell whose job answers to it. A job
        // loaded from a different plist — one we just replaced, or one an
        // external actor boot'ed out that has not finished tearing down —
        // satisfies both, so each check above can be skipped on the strength
        // of a job that is about to cease existing. That is not theoretical:
        // it silently no-op'd `install --start` on a real migration, leaving
        // the daemon stopped while `start` returned `Ok`, because the
        // predecessor job with the same label was still shutting down.
        //
        // Only `KeepAlive: true` (`restart: always`) licenses this check:
        // launchd guarantees such a job is running whenever it is loaded.
        // Under `on-failure` or `never` a job may legitimately have run and
        // exited by now, and demanding `Running` would fail a correct start.
        if blob.spec.restart == Restart::Always && query_live_state(id.as_str()).0 != State::Running {
            // One corrective cycle, not a retry loop: tear the stale job
            // down by label and load ours from the path we just confirmed.
            bootout(id.as_str())?;
            bootstrap(&location.path)?;
            if query_live_state(id.as_str()).0 != State::Running {
                kickstart(id.as_str())?;
            }
            if query_live_state(id.as_str()).0 != State::Running {
                return Err(Error::Other(format!(
                    "`{id}` did not start: its plist is loaded but launchd reports no running process, and `restart: always` means it should have one"
                )));
            }
        }
        Ok(())
    }

    fn stop(&self, id: &Id) -> Result<()> {
        located_and_ours(id)?;

        bootout(id.as_str())
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
            // A read failure (permission denied, the entry vanishing
            // between the scan above and here, ...) leaves zero evidence
            // of whether this plist ever carried a Goetia marker at all —
            // unlike a decode failure below, which only happens *after*
            // confirming the marker is present. Claiming `OursUnreadable`
            // without that evidence would misreport an unreadable
            // *foreign* plist (routine on `/Library/LaunchDaemons`, which
            // holds every vendor's daemons, not just Goetia's) as one of
            // ours; `list`'s contract is that a foreign entry is never
            // included at all, so this is skipped exactly like one.
            let Ok(text) = fs::read_to_string(path) else {
                continue;
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
