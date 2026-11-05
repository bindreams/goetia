//! The effectful launchd backend: [`Launchd`].
//!
//! The plist's *directory* is the enrollment bit, never a plist key:
//!
//! - `install` writes the plist to [`STAGING_DIR`] (or, for an update, where the existing artifact
//!   already lives; see [`locate`]). launchd scans neither directory on its own, so nothing loads
//!   and nothing runs. A plist that would change what a loaded job reads is refused and nothing is
//!   written (see [`refuse`]). A `Create` over a loaded job of the same label boots that job out
//!   first.
//! - `start` `launchctl bootstrap`s the plist from wherever it is (if not already loaded), then
//!   `launchctl kickstart`s it, which is what launches a job with no `KeepAlive`/`RunAtLoad` (a
//!   `restart: never` daemon). Under a budget that waits, the kickstart asks for the pid (`-p`)
//!   and so blocks on launchd's respawn throttle (measured ~10.02s, 10.01-10.03s) whenever the job
//!   has respawn history, as a crash-looping `restart: always` job does. [`Budget::DEFAULT`] is
//!   10s, just below that, so such a job essentially always reports [`Error::WaitTimeout`]. The
//!   request stands (the `launchctl` carrying it is left running, never killed), but launchd will
//!   not report the job running within the budget. The budget bounds that wait and nothing else:
//!   every `launchctl` call that issues the request runs to completion. The throttle is keyed to
//!   the job, so `bootout` plus `bootstrap` does not clear it and there is no remedy to apply.
//! - `stop` `launchctl bootout`s the job, from whichever directory it was loaded.
//! - `enable` moves the plist from [`STAGING_DIR`] into [`ENABLED_DIR`] (`/Library/LaunchDaemons`).
//!   Does not start it.
//! - `disable` moves the plist back to [`STAGING_DIR`]. It does not `bootout` the job:
//!   [`ServiceManager::disable`](crate::manager::ServiceManager::disable) promises not to stop a
//!   running one, and launchd holds a bootstrapped job by label, whichever directory its plist is
//!   in, so moving the file leaves it running.
//! - Reading (`list`, and every verb's ownership check) scans both directories, never `launchctl
//!   print`, whose format Apple does not guarantee. `status`'s live run state is the one exception,
//!   since no file says whether a job is running. It reads `launchctl print` leniently: a shape it
//!   does not recognize degrades to [`State::Unknown`].
//!
//! Only the modern verbs (`bootstrap`/`bootout`/`kickstart`/`print`) are used, never legacy
//! `load`/`unload`. `launchctl enable`/`disable` and the plist's `Disabled` key are never touched:
//! the override entries the former creates cannot be removed, and `bootstrap` refuses a `Disabled`
//! plist.

use std::collections::BTreeMap;
use std::io::{Read as _, Write as _};
use std::os::unix::fs::{DirBuilderExt as _, PermissionsExt as _};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::{fs, io};

use crate::backend::bounded::{self, Finished, Reservation, Role};
use crate::backend::client::{Client, ClientError, Real, Wait};
use crate::backend::launchd::{generate, refuse, state};
use crate::backend::{
    self, Decoded, Identity, InstalledDefinition, InstalledService, ReadOutcome, Scan, ScanEntry, Unnamed, WriteOutcome,
};
use crate::blob::Blob;
use crate::decide::{ExternalConfig, Outcome};
use crate::error::{Error, Occupant, PutBackFound, Reason, Result};
use crate::manager::budget::{self, Deadline};
use crate::manager::{Budget, Installed, State, Status, Step, flow};
use crate::spec::{AccountId, DaemonSpec, Id, Restart, User};

/// Where `install` writes a plist that is not enabled at boot. launchd never scans this directory,
/// so a job here is inert until `start` bootstraps it or `enable` moves it into [`ENABLED_DIR`].
pub const STAGING_DIR: &str = "/Library/Application Support/Goetia/daemons";

/// The system LaunchDaemons directory. A plist here is loaded at boot: being here is the
/// enrollment bit.
pub const ENABLED_DIR: &str = "/Library/LaunchDaemons";

pub struct Launchd {
    launchctl: Launchctl,
}

impl Launchd {
    pub fn new() -> Self {
        Self {
            launchctl: Launchctl::new(Box::new(Real::new(vec!["launchctl".to_string()], |_| {}))),
        }
    }
}

impl Default for Launchd {
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

/// Where a plist lives, and whether that is the enabled directory.
struct Location {
    path: PathBuf,
    enabled: bool,
}

/// What a stat established about a path: absence, presence, or neither.
///
/// [`Presence::Undetermined`] carries the [`io::Error`] itself, not a rendered reason, because
/// [`undetermined`]'s `recovery` is keyed on the errno.
#[derive(Debug)]
enum Presence {
    Absent,
    Present,
    Undetermined { source: io::Error },
}

/// Whether *anything* occupies `path`: any directory entry, not just a regular file.
///
/// `persist_new`'s `link`(2) refuses to create over a directory, socket or symlink just as over a
/// file (`EEXIST`), so `Path::is_file` would let `locate` call such a path `Absent`, and `install`
/// would classify `Create` and fail with `Raced` on every run. `symlink_metadata` also counts a
/// dangling symlink, which `metadata` would report as absent.
fn occupied(path: &Path) -> Presence {
    match fs::symlink_metadata(path) {
        Ok(_) => Presence::Present,
        Err(e) if e.kind() == io::ErrorKind::NotFound => Presence::Absent,
        // `EACCES`, `ENOTDIR` on a parent, `EIO`: the stat never completed, so it established
        // nothing about absence.
        Err(source) => Presence::Undetermined { source },
    }
}

/// Where `id`'s plist lives, and whether that is the enabled directory. `Ok(None)` if it lives in
/// neither.
///
/// `Err` if a plist exists at both paths. No operation of this backend leaves that (`enable` and
/// `disable` move, never copy), but a hand-edit or a crash mid-move could; preferring one would
/// make the other's content vanish from every command, so both paths are named instead.
fn locate(id: &str) -> Result<Option<Location>> {
    let staging = staging_path(id);
    let enabled = enabled_path(id);
    match (occupied(&staging), occupied(&enabled)) {
        // One unresolved probe leaves the pair unresolved: each remaining answer claims something
        // about the directory that was never read.
        (Presence::Undetermined { source }, _) => Err(undetermined(id, "stat", &staging, source)),
        (_, Presence::Undetermined { source }) => Err(undetermined(id, "stat", &enabled, source)),
        (Presence::Absent, Presence::Absent) => Ok(None),
        (Presence::Present, Presence::Absent) => Ok(Some(Location {
            path: staging,
            enabled: false,
        })),
        (Presence::Absent, Presence::Present) => Ok(Some(Location {
            path: enabled,
            enabled: true,
        })),
        (Presence::Present, Presence::Present) => Err(Error::AmbiguousInstall {
            id: id.to_string(),
            paths: [staging, enabled],
        }),
    }
}

// Reading a plist =====================================================================================================

/// This backend's single [`Error::Undetermined`] constructor, worded as
/// `backend::systemd::manager::discover::undetermined` is. Never [`Error::Os`]: the failing read is
/// routinely of a stranger's service, since `/Library/LaunchDaemons` holds every vendor's daemons.
///
/// Every failure but `NotFound` lands here: an `EIO` leaves goetia as ignorant of the id as an
/// `EACCES`. The errno chooses only `recovery`: re-running elevated is advice only a permission
/// boundary earns.
fn undetermined(id: &str, op: &str, path: &Path, source: io::Error) -> Error {
    let recovery = if source.kind() == io::ErrorKind::PermissionDenied {
        "re-run as root (or under sudo): that read is what tells goetia whether anything is \
         installed at this id"
    } else {
        "resolve that failure and re-run: that read is what tells goetia whether anything is \
         installed at this id"
    };
    Error::Undetermined {
        id: id.to_string(),
        reason: Box::new(Error::os_at(op, path, source)),
        recovery: recovery.to_string(),
    }
}

/// The magic every binary property list begins with. A `/Library/LaunchDaemons` full of them is
/// normal on a macOS host.
const BINARY_PLIST_MAGIC: &[u8] = b"bplist00";

/// What a plist's bytes turned out to be, once obtained. Reaching it means the file was opened,
/// `fstat`'d as regular and read to the end, so presence is established whichever variant comes out
/// (unlike [`Error::Undetermined`]).
enum ClassifyOutcome {
    /// Valid UTF-8: the only form `generate::plist` writes, and the only one in which
    /// `generate::extract` can look for the `goetia:begin` comment.
    Text(String),
    /// Bytes goetia did not write, identified positively (a binary or UTF-16 plist is as surely not
    /// goetia's as an XML plist with no marker) and answered like one: foreign, omitted.
    ///
    /// Every non-UTF-8 plist gets this one answer, not `undetermined`: a vendor's UTF-16 plist or
    /// one with a stray Latin-1 byte would otherwise be a permanent, unclearable `undetermined`
    /// entry and exit `4`.
    ///
    /// Cost: a plist goetia wrote and that was corrupted since reads as a stranger's, not as
    /// ours-but-broken, because the marker is in the bytes that would not decode.
    NotOurs,
}

/// What trying to obtain a plist's bytes ran into.
enum Obtained {
    Bytes(Vec<u8>),
    /// Nothing is at the path.
    Absent,
    /// Something is at the path and it is positively not a plist: a FIFO, a directory, a socket, a
    /// device node, a dangling symlink. Treated like a binary plist: foreign, omitted.
    NonRegular,
    /// The read did not complete.
    Failed(io::Error),
}

/// Classify `path` before opening it, and again on the descriptor actually opened.
///
/// `open(2)` on a FIFO blocks until a writer arrives, and `list` reads every `*.plist` in
/// `/Library/LaunchDaemons`, so one `mkfifo` there would wedge the listing for the host and
/// `install`/`status` for that id. Step 1, `fs::metadata`, opens nothing, so it cannot block, and
/// it keeps a device node present at step 1 from being opened at all; one swapped in after step 1
/// is opened, then rejected by step 2.
///
/// Step 1 follows symlinks, unlike `systemd::manager::discover::classify_and_read`'s `O_NOFOLLOW`:
/// `systemctl mask` points a fragment at `/dev/null`, so there the symlink itself is the artifact.
/// launchd has no masking, and a symlink at a plist path is just an indirection to the plist. Do
/// not unify the two.
///
/// Step 2 re-checks the descriptor, not the name, so the verdict is about the object whose bytes
/// are read: the file step 1 classified need not be the one a later `open` resolves the name to.
/// No `(st_dev, st_ino)` comparison against step 1 is needed, since an `fstat` on the open
/// descriptor is stronger than two name lookups agreeing. It does not rule out reading a
/// different regular file than step 1 stat'd, the benign content race a plain `fs::read` has too,
/// and one no verdict here depends on.
/// `O_NONBLOCK` covers the window between the steps: a FIFO swapped in after step 1 cannot block
/// the open, so step 2 rejects it.
fn obtain(path: &Path) -> Obtained {
    use std::os::unix::fs::OpenOptionsExt as _;

    match fs::metadata(path) {
        Ok(meta) if !meta.is_file() => return Obtained::NonRegular,
        Ok(_) => {}
        // `metadata` follows, so it reports a dangling symlink as absent, yet `link`(2) refuses to
        // create over one (`EEXIST`). Left `Absent`, every verb would say "not installed" while no
        // write could ever succeed. The link is positively not a plist, like a FIFO.
        Err(e) if e.kind() == io::ErrorKind::NotFound && path.symlink_metadata().is_ok() => {
            return Obtained::NonRegular;
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Obtained::Absent,
        Err(e) => return Obtained::Failed(e),
    }

    let mut file = match fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(path)
    {
        Ok(file) => file,
        // Gone between the two steps: the uninstall race step 1 tolerates too. A dangling symlink
        // cannot reach here; step 1 made it `NonRegular`.
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Obtained::Absent,
        Err(e) => return Obtained::Failed(e),
    };
    match file.metadata() {
        Ok(meta) if !meta.is_file() => return Obtained::NonRegular,
        Ok(_) => {}
        Err(e) => return Obtained::Failed(e),
    }

    let mut bytes = Vec::new();
    match file.read_to_end(&mut bytes) {
        Ok(_) => Obtained::Bytes(bytes),
        Err(e) => Obtained::Failed(e),
    }
}

/// The one place bytes obtained from a plist become a verdict, so `list` and [`read_artifact`]
/// cannot classify the same file differently.
///
/// The magic is checked before the UTF-8 decode because a small binary plist can be valid UTF-8
/// (its trailer is mostly NUL bytes).
fn classify(bytes: Vec<u8>) -> ClassifyOutcome {
    if bytes.starts_with(BINARY_PLIST_MAGIC) {
        return ClassifyOutcome::NotOurs;
    }
    match String::from_utf8(bytes) {
        Ok(text) => ClassifyOutcome::Text(text),
        Err(_) => ClassifyOutcome::NotOurs,
    }
}

/// Read the artifact `locate` found for `id`, for the callers that look for a marker in it
/// ([`read_at`]).
///
/// An absent path is [`Error::NotInstalled`]: the plist was there when `locate` stat'd it, so an
/// uninstall completed in between, and absence is the truth. Every read that did not complete is
/// [`undetermined`].
///
/// Bytes that are not goetia's ([`ClassifyOutcome::NotOurs`]) and anything not a regular file read
/// as the empty string: no marker, so `Foreign`, and `install` reaches `RefuseForeign` with a
/// remedy instead of a bare `Err`.
fn read_artifact(path: &Path, id: &str) -> Result<String> {
    let bytes = match obtain(path) {
        Obtained::Bytes(bytes) => bytes,
        Obtained::Absent => return Err(Error::NotInstalled { id: id.to_string() }),
        Obtained::NonRegular => return Ok(String::new()),
        Obtained::Failed(e) => return Err(undetermined(id, "read", path, e)),
    };
    match classify(bytes) {
        ClassifyOutcome::Text(text) => Ok(text),
        ClassifyOutcome::NotOurs => Ok(String::new()),
    }
}

/// A marked plist, as `read` saw it: where it is, and its text.
pub(crate) struct Plist {
    id: Id,
    blob: Decoded,
    location: Location,
    text: String,
}

impl Plist {
    fn new(id: Id, blob: Decoded, (location, text): (Location, String)) -> Self {
        Self {
            id,
            blob,
            location,
            text,
        }
    }
}

impl InstalledService for Plist {
    fn id(&self) -> &Id {
        &self.id
    }

    fn blob(&self) -> std::result::Result<&Blob, &Arc<Error>> {
        self.blob.as_ref()
    }
}

/// What is at `location`: no marker is `Foreign`, a marker is `Marked` whether or not its blob
/// decodes, since the marker alone is proof of ownership. Whether the spec can still be rendered
/// here is `flow`'s question, answered with `render`.
fn read_at(id: &str, location: Location) -> Result<ReadOutcome<Unnamed<(Location, String)>>> {
    let text = read_artifact(&location.path, id)?;
    let Some(blob) = generate::extract(&text).transpose() else {
        return Ok(ReadOutcome::Foreign { recovery: None });
    };
    let blob = blob.map_err(Arc::new);
    Ok(ReadOutcome::Marked(Unnamed {
        blob,
        body: (location, text),
    }))
}

// Account resolution ==================================================================================================

/// A platform account resolved from [`crate::spec::User`]: the [`Identity`] a generator needs, plus
/// the uid/gid `install` needs to hand a `cwd`/`logs` directory to its owner.
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

/// Resolve a [`User`] to a real macOS account, always by *name*: `launchd.plist(5)` does not say
/// whether `UserName` accepts a numeric string, so every case, `User::Id(AccountId::Uid(_))`
/// included, goes through a passwd lookup.
fn resolve_account(user: &User) -> Result<Account> {
    match user {
        User::Root => account_from_uid(0),
        User::Name(name) => nix::unistd::User::from_name(name)
            .map_err(|e| Error::AccountLookup {
                detail: format!("look up user `{name}`"),
                source: Some(e.into()),
            })?
            .map(Account::from)
            .ok_or_else(|| Error::AccountLookup {
                detail: format!("no such user `{name}` on this host"),
                source: None,
            }),
        User::Id(AccountId::Uid(uid)) => account_from_uid(*uid),
        User::Id(AccountId::Sid(sid)) => Err(Error::AccountLookup {
            detail: format!("user `{{id: {sid}}}` names a Windows SID, which is not meaningful on macOS"),
            source: None,
        }),
    }
}

fn account_from_uid(uid: u32) -> Result<Account> {
    nix::unistd::User::from_uid(nix::unistd::Uid::from_raw(uid))
        .map_err(|e| Error::AccountLookup {
            detail: format!("look up uid {uid}"),
            source: Some(e.into()),
        })?
        .map(Account::from)
        .ok_or_else(|| Error::AccountLookup {
            detail: format!("no such uid {uid} on this host"),
            source: None,
        })
}

// Writing the plist ===================================================================================================

/// `create_dir_all` the directory a plist is about to be written into, then make it `0755`
/// regardless of the caller's umask: `list`/`status`/`show` must work unelevated, which needs every
/// ancestor world-readable and searchable.
fn ensure_dir(dir: &Path) -> Result<()> {
    fs::create_dir_all(dir).map_err(|e| Error::os_at("create directory", dir, e))?;
    fs::set_permissions(dir, fs::Permissions::from_mode(0o755)).map_err(|e| Error::os_at("chmod", dir, e))
}

/// Create `cwd` itself and the parent of `logs` (a file path; launchd creates the log file but not
/// its directory), both world-readable and searchable. Each component created is handed to
/// `account`, so a non-root daemon can write there. Otherwise a non-root daemon whose
/// `StandardOutPath` is in a root-only directory fails at launch with an opaque status, and a
/// missing `WorkingDirectory` refuses the job outright.
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

/// Create `dir` and every missing ancestor, `chmod`ing `0755` and `chown`ing to `account` only the
/// components this call itself creates. `dir` is an arbitrary absolute path a spec author supplied
/// (`spec::resolve` guarantees only that it's absolute), so doing that to a path that already
/// exists would hand ownership of, and strip the mode from, whatever happened to be there
/// (`/var/log`, a user's home directory, ...). A path that already exists — of any kind, a dangling
/// symlink included — is left untouched, and so is one a concurrent install creates first.
fn ensure_dir_owned_by(dir: &Path, account: &Account) -> Result<()> {
    create_owned(&missing_components(dir)?, account)
}

/// The components of `dir` that do not exist, outermost first. A dangling symlink exists.
fn missing_components(dir: &Path) -> Result<Vec<&Path>> {
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
            Err(e) => return Err(Error::os_at("stat", cursor, e)),
        }
    }
    missing.reverse();
    Ok(missing)
}

/// `mkdir` each of `missing` (outermost first), then `chmod` and `chown` it. `AlreadyExists` means
/// another install created it after the walk: not ours, so left alone.
fn create_owned(missing: &[&Path], account: &Account) -> Result<()> {
    for path in missing {
        match fs::DirBuilder::new().mode(0o755).create(path) {
            Ok(()) => {
                fs::set_permissions(path, fs::Permissions::from_mode(0o755))
                    .map_err(|e| Error::os_at("chmod", *path, e))?;
                nix::unistd::chown(*path, Some(account.uid), Some(account.gid))
                    .map_err(|e| Error::os_at(format!("chown to {}", account.name), *path, e))?;
            }
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(Error::os_at("create directory", *path, e)),
        }
    }
    Ok(())
}

/// Build a `0644` named temp file in `dir` (so a later hard-link persist stays on one filesystem)
/// carrying `content`, and check it with `plutil -lint` before handing it back. `install` does not
/// bootstrap the plist, so this catches malformed XML at install rather than at the first `start`.
fn staged_tempfile(dir: &Path, content: &str) -> Result<tempfile::NamedTempFile> {
    ensure_dir(dir)?;
    let mut tmp = tempfile::Builder::new()
        .prefix(".goetia-")
        .suffix(".plist.tmp")
        .tempfile_in(dir)
        .map_err(|e| Error::os_at("create a temp file in", dir, e))?;
    tmp.write_all(content.as_bytes())
        .map_err(|e| Error::os_at("write", tmp.path(), e))?;
    fs::set_permissions(tmp.path(), fs::Permissions::from_mode(0o644))
        .map_err(|e| Error::os_at("chmod", tmp.path(), e))?;
    lint(tmp.path())?;
    Ok(tmp)
}

fn lint(path: &Path) -> Result<()> {
    let path_str = path
        .to_str()
        .expect("temp plist path is UTF-8 (see generate::path_str)");
    require_success("plutil", &["-lint", path_str])
}

enum PersistOutcome {
    Written,
    /// Something appeared at the target path between discovery and this write.
    Raced,
}

/// Create-only write of a staged plist for a `Create`: refuses to replace anything at `target`, so
/// a foreign plist (or a concurrent installer's) dropped in after discovery is not destroyed.
/// Hard-link-then-unlink (`NamedTempFile::persist_noclobber`), the portable equivalent of Linux's
/// `RENAME_NOREPLACE`: the link is the atomic step, so `Raced` means the destination did not exist a
/// moment ago and does now.
fn persist_new(staged: tempfile::NamedTempFile, target: &Path) -> Result<PersistOutcome> {
    match staged.persist_noclobber(target) {
        Ok(_file) => Ok(PersistOutcome::Written),
        Err(e) if e.error.kind() == io::ErrorKind::AlreadyExists => Ok(PersistOutcome::Raced),
        Err(e) => Err(Error::os_at("create", target, e.error)),
    }
}

/// Overwrite the plist already at `target`, for `Update`/`Stale` only: `read` has established the
/// id is marked.
fn write_existing(target: &Path, content: &str) -> Result<()> {
    let tmp = staged_tempfile(target.parent().expect("plist path always has a parent"), content)?;
    tmp.persist(target)
        .map(drop)
        .map_err(|e| Error::os_at("replace", target, e.error))
}

/// Whether a plist is still at `target`. [`Error::Os`] if the stat could not tell, not
/// [`undetermined`]: the callers have already read an artifact at this id, so "is anything installed
/// here" is not the question the stat left open.
fn still_present(target: &Path) -> Result<bool> {
    match occupied(target) {
        Presence::Present => Ok(true),
        Presence::Absent => Ok(false),
        Presence::Undetermined { source } => Err(Error::os_at("stat", target, source)),
    }
}

/// A waited start that launchd accepted, read back as `state` (not `Running`) before the deadline:
/// a stopped or failed job did not start, and `Unknown` (`spawn scheduled`, or a read that failed)
/// establishes nothing either way.
fn not_started(id: &Id, state: State, keep_alive: bool) -> Error {
    match state {
        State::Unknown => Error::StartUnconfirmed { id: id.to_string() },
        State::Stopped | State::Failed | State::Running => Error::NotStarted {
            id: id.to_string(),
            keep_alive,
        },
    }
}

/// Undo `install`'s write to `target`: put `previous` back, or remove the plist if there was none.
/// Only over the plist `install` wrote (`wrote`). The path can have changed since: a concurrent
/// `enable` or `uninstall` moved or removed it, or a concurrent install wrote after ours, and a write
/// here would put a plist in both directories, resurrect a removed one, or destroy the other's.
/// The `Err` says what is on disk.
fn undo_write(target: &Path, wrote: &str, previous: Option<&str>) -> Result<()> {
    let blocked = |found| Error::PutBackBlocked {
        path: target.to_path_buf(),
        found,
    };
    // `install` writes a regular file, so anything else found here (a symlink, dangling or not, a
    // directory) is someone else's. This reports what the check found; the read below looks the
    // path up again.
    match fs::symlink_metadata(target) {
        Ok(meta) if !meta.file_type().is_file() => return Err(blocked(PutBackFound::Replaced)),
        Ok(_) => {}
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Err(blocked(PutBackFound::Gone)),
        Err(e) => return Err(Error::os_at("stat", target, e)),
    }
    match fs::read_to_string(target) {
        Ok(on_disk) if on_disk == wrote => {}
        Ok(_) => return Err(blocked(PutBackFound::Replaced)),
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Err(blocked(PutBackFound::Gone)),
        Err(e) if e.kind() == io::ErrorKind::InvalidData => return Err(blocked(PutBackFound::Replaced)),
        Err(e) => return Err(Error::os_at("read", target, e)),
    }
    let undone = match previous {
        Some(previous) => write_existing(target, previous),
        None => fs::remove_file(target).map_err(|e| Error::os_at("remove", target, e)),
    };
    undone.map_err(|e| Error::Step {
        what: format!("{} still holds what install wrote", target.display()),
        source: Box::new(e),
    })
}

/// What is at `path`, for a message.
fn describe_at(path: &Path) -> Occupant {
    match fs::symlink_metadata(path) {
        Ok(m) if m.is_file() => Occupant::File,
        Ok(m) if m.is_dir() => Occupant::Directory,
        Ok(m) if m.is_symlink() => Occupant::Symlink,
        Ok(_) => Occupant::Special,
        Err(e) => Occupant::Unreadable(e),
    }
}

/// Move `src` to `dest` without replacing anything at `dest`, for `enable`/`disable`. `fs::rename`
/// silently replaces an existing destination on POSIX, so a foreign or concurrent plist landing at
/// `dest` after the caller's checks would be destroyed. Hard-link then unlink, as `persist_new`
/// does: `AlreadyExists` means `dest` was occupied.
fn move_no_clobber(src: &Path, dest: &Path) -> Result<()> {
    match fs::hard_link(src, dest) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
            return Err(Error::AlreadyExists {
                path: dest.to_path_buf(),
            });
        }
        Err(e) => return Err(Error::os_at("move to", dest, e)),
    }
    fs::remove_file(src).map_err(|e| Error::os_at("remove", src, e))
}

// launchctl ===========================================================================================================

/// Spawn `command` with `args` (a tool other than `launchctl`, which goes through [`Launchctl`]),
/// giving back the full `Output`. Only a failure to start the process is an `Err`; a non-zero exit
/// is in the `Output`.
fn run(command: &str, args: &[&str]) -> Result<std::process::Output> {
    Command::new(command)
        .args(args)
        .output()
        .map_err(|e| Error::spawn(format!("{command} {}", args.join(" ")), e))
}

/// [`run`] for a caller that only needs success: a spawn failure is an [`Error::Spawn`], a non-zero
/// exit an [`Error::Command`].
fn require_success(command: &str, args: &[&str]) -> Result<()> {
    let out = run(command, args)?;
    if out.status.success() {
        Ok(())
    } else {
        Err(Error::command(
            format!("{command} {}", args.join(" ")),
            out.status,
            &out.stderr,
        ))
    }
}

/// The failure of a `launchctl` call that did not complete: a [`Error::RequestInDoubt`] for a
/// `request` whose `launchctl` may have started, an [`Error::Spawn`] for anything else.
fn failed(args: &[&str], request: bool, error: ClientError) -> Error {
    let command = format!("launchctl {}", args.join(" "));
    let (source, may_have_run) = match error {
        ClientError::NotRun(source) => (source, false),
        ClientError::MayHaveRun(source) => (source, true),
        ClientError::Lost { error, announced } => {
            debug_assert!(!announced, "launchd sends no announced requests");
            (error, true)
        }
    };
    if request && may_have_run {
        Error::RequestInDoubt {
            request: command,
            // `launchctl` never says when launchd has the request.
            reached: false,
            note: None,
            source: source.into(),
        }
    } else {
        Error::spawn(command, source)
    }
}

/// The failure for a `launchctl` call that ran and exited non-zero.
fn command_failed(args: &[&str], status: std::process::ExitStatus, stderr: &[u8]) -> Error {
    Error::command(format!("launchctl {}", args.join(" ")), status, stderr)
}

/// The one [`Deadline`] a budgeted verb derives, first thing at entry.
///
/// A budget that does not wait gets an unbounded deadline, not `budget.start()`: `launchctl` has no
/// non-blocking `bootout`, so issuing that request is the blocking call, and an already-expired
/// deadline would stop waiting at once and reach `budget::timed_out(.., Budget::Immediate)`,
/// tripping its `debug_assert!`. What a non-waiting budget declines to wait for is the pid, and the
/// plain `kickstart` is what declines it.
fn verb_deadline(budget: Budget) -> Deadline {
    if budget.waits() {
        budget.start()
    } else {
        Budget::Unbounded.start()
    }
}

/// `Deadline(None)` never expires, so no step driven by an unbounded deadline reports an expiry.
/// `expect`, never `let _ =`, which would turn an unreachable case into a silent one.
fn never_expires<T>(step: Option<T>) -> T {
    step.expect("this call passed an unbounded deadline, which never expires")
}

/// What a `kickstart` established about the job.
enum Kickstarted {
    /// launchd forked the job and named a pid on a whole capture. That a pid parsed is the
    /// confirmation, so the number is not carried.
    Confirmed,
    /// `kickstart` succeeded but named no pid this code will trust: the plain form, which never
    /// prints one, or a `-p` whose stdout is not the whole `<digits>\n` line.
    NoPid,
}

/// Exit codes `launchctl` uses for "no such job": 3 ("no such process") and 113 ("could not find
/// specified service").
fn is_not_found(code: Option<i32>) -> bool {
    matches!(code, Some(3 | 113))
}

/// `launchctl`, reached through a [`Client`]. Every call the verbs make goes through [`Launchctl::run`].
struct Launchctl {
    client: Box<dyn Client>,
}

impl Launchctl {
    fn new(client: Box<dyn Client>) -> Launchctl {
        Launchctl { client }
    }

    /// `launchctl` with `args`, spawned and waited under `deadline` in `role`.
    ///
    /// `role` keeps the budget from deciding whether a request is sent: a [`Role::Request`] that
    /// expires is left running, never killed, since `launchctl` never says when launchd has the
    /// request. Its reaper (see [`request`]) and the temp files its output goes to are drawn from
    /// `reservation`.
    ///
    /// [`Finished::Expired`] iff the deadline expired first. For a request that is not a failure
    /// of the command: the request is out, or on its way in a `launchctl` left running. What it
    /// means differs per verb, so the verb decides. A failure before `launchctl` ran is an
    /// [`Error::Spawn`]. One after it may have (cosca failing past `exec`, or the wait failing) is
    /// the same for a read, and [`Error::RequestInDoubt`] for a request, which may have reached
    /// launchd.
    fn run(&self, args: &[&str], deadline: Deadline, role: Role, reservation: &mut Reservation) -> Result<Finished> {
        // A request whose `launchctl` may have started is in doubt, not failed: it may have reached
        // launchd. A read changes nothing either way.
        let request = match role {
            Role::Query | Role::UncountedQuery => false,
            Role::Request(_) => true,
            Role::AnnouncedRequest(_) => unreachable!("launchd sends no announced requests"),
        };
        self.client
            .run(args, Wait::Bounded { role, deadline }, reservation)
            .map_err(|error| failed(args, request, error))
    }

    /// Whether launchd has `id` loaded. `Ok(None)` iff `deadline` expired.
    ///
    /// Only success and the not-found codes in [`is_not_found`] answer the question; any other failure is
    /// an `Err`, since callers act on "not loaded" by writing or bootstrapping.
    fn is_loaded(&self, id: &str, deadline: Deadline, reservation: &mut Reservation) -> Result<Option<bool>> {
        // Only the exit code is read, never the body of `print`'s output.
        let target = target(id);
        let args = ["print", target.as_str()];
        let Finished::Exited { status, capture } = self.run(&args, deadline, Role::Query, reservation)? else {
            return Ok(None);
        };
        if status.success() {
            Ok(Some(true))
        } else if is_not_found(status.code()) {
            Ok(Some(false))
        } else {
            Err(command_failed(&args, status, &capture.stderr))
        }
    }

    /// `Ok(None)` iff `deadline` expired.
    fn bootstrap(&self, path: &Path, deadline: Deadline, reservation: &mut Reservation) -> Result<Option<()>> {
        let path_str = path.to_str().expect("plist path is UTF-8 (see generate::path_str)");
        let args = ["bootstrap", "system", path_str];
        let role = request(reservation)?;
        let Finished::Exited { status, capture } = self.run(&args, deadline, role, reservation)? else {
            return Ok(None);
        };
        if status.success() {
            Ok(Some(()))
        } else {
            Err(command_failed(&args, status, &capture.stderr))
        }
    }

    /// `launchctl kickstart` for `id`, under `deadline`. `Ok(None)` iff the deadline expired.
    ///
    /// Never `-k`: it force-restarts the job, which is not what `start` means, and blocks on
    /// launchd's respawn throttle every time.
    ///
    /// `budget.waits()` picks the form. A budget that waits uses `-p`, which returns once launchd
    /// has forked the job and writes the bare pid on stdout. That is launchd's whole answer, so a
    /// pid parsed from a successful `-p` is the confirmation and no follow-up `print` is needed. A
    /// budget that does not wait uses the plain form, which only requests the spawn.
    fn kickstart(
        &self,
        id: &str,
        budget: Budget,
        deadline: Deadline,
        reservation: &mut Reservation,
    ) -> Result<Option<Kickstarted>> {
        let target = target(id);
        let args: Vec<&str> = if budget.waits() {
            vec!["kickstart", "-p", &target]
        } else {
            vec!["kickstart", &target]
        };
        let role = request(reservation)?;
        let Finished::Exited { status, capture } = self.run(&args, deadline, role, reservation)? else {
            return Ok(None);
        };
        if !status.success() {
            return Err(command_failed(&args, status, &capture.stderr));
        }
        // Read the pid only from a complete capture: a partial `"4766\n"` parses as `"47"`, a wrong
        // and possibly live pid. `state::kickstart_pid` guards the same way.
        let pid = capture
            .complete
            .then(|| state::kickstart_pid(&String::from_utf8_lossy(&capture.stdout)))
            .flatten();
        Ok(Some(match pid {
            Some(_) => Kickstarted::Confirmed,
            None => Kickstarted::NoPid,
        }))
    }

    /// `bootout`, with no `is_loaded` pre-check: the job could unload between check and act (a
    /// concurrent `stop`, an operator's `launchctl bootout`), failing this call for an already
    /// stopped service and breaking the idempotency [`ServiceManager::stop`] requires. Success and
    /// the [`is_not_found`] codes are both `Ok`.
    ///
    /// [`ServiceManager::stop`]: crate::manager::ServiceManager::stop
    fn bootout(&self, id: &str, deadline: Deadline, reservation: &mut Reservation) -> Result<Option<()>> {
        let target = target(id);
        let args = ["bootout", target.as_str()];
        let role = request(reservation)?;
        let Finished::Exited { status, capture } = self.run(&args, deadline, role, reservation)? else {
            return Ok(None);
        };
        if status.success() || is_not_found(status.code()) {
            Ok(Some(()))
        } else {
            Err(command_failed(&args, status, &capture.stderr))
        }
    }

    /// `launchctl kill SIGKILL` on the job, never a pid; what happens next is the plist's `KeepAlive`.
    /// The [`is_not_found`] codes mean the job has no process: [`Error::NotRunning`]. `Ok(None)` iff
    /// the deadline expired.
    fn kill_job(&self, id: &str, deadline: Deadline, reservation: &mut Reservation) -> Result<Option<()>> {
        let target = target(id);
        let args = ["kill", "SIGKILL", target.as_str()];
        let role = request(reservation)?;
        let Finished::Exited { status, capture } = self.run(&args, deadline, role, reservation)? else {
            return Ok(None);
        };
        if status.success() {
            Ok(Some(()))
        } else if is_not_found(status.code()) {
            Err(Error::NotRunning { id: id.to_string() })
        } else {
            Err(command_failed(&args, status, &capture.stderr))
        }
    }

    /// Live state, read from `launchctl print`'s body (see the module doc). An unrecognized shape
    /// degrades to `(State::Unknown, None)`. `Err` iff `launchctl` could not be run at all, which
    /// `start` must not mistake for a job that is not running.
    fn live_state(&self, id: &str, deadline: Deadline, reservation: &mut Reservation) -> Result<(State, Option<u32>)> {
        self.read_live_state(id, deadline, Role::Query, reservation)
    }

    /// [`Self::live_state`], read as `role`.
    fn read_live_state(
        &self,
        id: &str,
        deadline: Deadline,
        role: Role,
        reservation: &mut Reservation,
    ) -> Result<(State, Option<u32>)> {
        // An expiry degrades to `Unknown`, as a capture that is not whole does. The budgeted verbs
        // check the deadline wherever an `Unknown` would otherwise become a wrong error.
        let Finished::Exited { status, capture } = self.run(&["print", &target(id)], deadline, role, reservation)?
        else {
            return Ok((State::Unknown, None));
        };
        Ok(classified(status, &capture))
    }
}

/// The role of a request: a [`Role::Request`] with its reaper drawn from `reservation`.
fn request(reservation: &mut Reservation) -> Result<Role> {
    reservation
        .reaper()
        .map(Role::Request)
        .map_err(|source| Error::Reservation { id: None, source })
}

// Reservation =========================================================================================================

/// Most requests each verb sends; each draws a reaper from the verb's [`Reservation`].
/// `start`: bootstrap+kickstart, then the corrective cycle's bootout+bootstrap+kickstart.
const INSTALL_REQUESTS: usize = 1;
const START_REQUESTS: usize = 5;
const STOP_REQUESTS: usize = 1;
const KILL_REQUESTS: usize = 1;

/// Most `launchctl` calls per verb, reads included: each draws its role's temp files from the same
/// reservation. `install` adds two `is_loaded`s; `start` two `is_loaded`s and five `live_state`s.
const INSTALL_CALLS: usize = INSTALL_REQUESTS + 2;
const START_CALLS: usize = START_REQUESTS + 7;
const STOP_CALLS: usize = STOP_REQUESTS;
const KILL_CALLS: usize = KILL_REQUESTS;

/// The most `step` draws, as `(requests, calls)`. A draw past it is a miscount, caught by the
/// assertion in [`Reservation`] in every test that runs the verb.
///
/// `request_start_after_stop` is `start` under a budget that does not wait: it draws no more than
/// `start` does, and so is counted as it is.
fn needs(step: Step) -> (usize, usize) {
    match step {
        Step::Install => (INSTALL_REQUESTS, INSTALL_CALLS),
        Step::Start | Step::RequestStartAfterStop => (START_REQUESTS, START_CALLS),
        Step::Stop | Step::Uninstall => (STOP_REQUESTS, STOP_CALLS),
        Step::Kill => (KILL_REQUESTS, KILL_CALLS),
    }
}

/// The reapers and temp files `steps` draw, summed: what [`Launchd::reserve`] makes.
fn counted(steps: &[Step]) -> (usize, usize) {
    let (requests, calls) = steps
        .iter()
        .map(|&step| needs(step))
        .fold((0, 0), |(requests, calls), (r, c)| (requests + r, calls + c));
    (requests, calls * Role::COUNTED_FILES)
}

/// What a `launchctl print` that ran says about the job's live state.
fn classified(status: std::process::ExitStatus, capture: &bounded::Capture) -> (State, Option<u32>) {
    if !capture.complete {
        return (State::Unknown, None);
    }
    if !status.success() {
        // Not found is a clean "not running", what a staged-never-started or booted-out service
        // reports. Anything else, above all an unprivileged caller's permission failure querying
        // the system domain, must not read as "stopped": `list`/`status` work unelevated, and a
        // running daemon reported `Stopped` because the caller could not ask is a wrong answer.
        return if is_not_found(status.code()) {
            (State::Stopped, None)
        } else {
            (State::Unknown, None)
        };
    }
    let text = String::from_utf8_lossy(&capture.stdout);
    state::classify(&text)
}

// Backend =============================================================================================================

impl Launchd {
    /// The read after `install` wrote `wrote` to `target`, where `previous` was (`None` after a create).
    ///
    /// The check before the write and the write are not atomic, and launchd relaunches a loaded job from
    /// the definition it was loaded with, not the plist on disk. A job loaded in between may hold either,
    /// and goetia takes no lock, so it reports doubt rather than success. Put the plist back as it was
    /// first: with the new one on disk a rerun reads `UpToDate` and never reaches the loaded job.
    fn recheck_after_write(
        &self,
        id: &Id,
        deadline: Deadline,
        target: &Path,
        wrote: &str,
        previous: Option<&str>,
        reservation: &mut Reservation,
    ) -> Result<()> {
        match self.launchctl.is_loaded(id.as_str(), deadline, reservation) {
            Ok(loaded) if !never_expires(loaded) => Ok(()),
            Ok(_) => Err(refuse::loaded_after_write(id, undo_write(target, wrote, previous))),
            Err(read) => Err(refuse::state_unknown_after_write(
                id,
                read,
                undo_write(target, wrote, previous),
            )),
        }
    }
}

impl backend::Backend for Launchd {
    type Definition = String;
    type Snapshot = Plist;
    type InstallScope = ();
    type Reservation = Reservation;

    fn begin_install(&self, _spec: &DaemonSpec) -> Result<()> {
        Ok(())
    }

    /// Makes every reaper and temp file `steps` can draw now, so that no step fails for want of one
    /// after an earlier step has sent something: `restart`'s start after its stop, `install
    /// --start`'s start after the install's `bootout`.
    fn reserve(&self, steps: &[Step], _budget: Budget) -> Result<Reservation> {
        let (reapers, files) = counted(steps);
        Reservation::make(reapers, files).map_err(|source| Error::Reservation { id: None, source })
    }

    fn render(&self, spec: &DaemonSpec) -> Result<String> {
        let account = resolve_account(&spec.user)?;
        Ok(generate::plist(spec, &account.identity()))
    }

    fn read(&self, id: &Id) -> Result<ReadOutcome<Plist>> {
        match locate(id.as_str())? {
            None => Ok(ReadOutcome::Absent),
            Some(location) => read_at(id.as_str(), location).map(|found| found.named(id, Plist::new)),
        }
    }

    fn installed(&self, snapshot: &Plist) -> Result<InstalledDefinition<String>> {
        Ok(InstalledDefinition {
            definition: snapshot.text.clone(),
            // launchd has no drop-in or override that alters a job without touching its plist.
            // Enablement lives in the plist's directory, outside the compared surface.
            external: ExternalConfig::Absent,
        })
    }

    /// `install` has no `--timeout` of its own, so its own `launchctl` calls are deliberately
    /// unbounded.
    ///
    /// `Raced` only where no `launchctl` call has been made yet: another pass could then make more
    /// calls than the verb reserved ([`INSTALL_CALLS`]), so a race after one is an error instead.
    fn write(
        &self,
        spec: &DaemonSpec,
        desired: &String,
        outcome: &Outcome,
        snapshot: Option<&Plist>,
        _scope: &mut (),
        reservation: &mut Reservation,
    ) -> Result<WriteOutcome> {
        // Resolved again: `render` kept only the plist, which names the account, and launchd
        // resolves that name at launch too.
        let account = resolve_account(&spec.user)?;
        let deadline = Budget::Unbounded.start();

        match outcome {
            Outcome::Create => {
                // A job may already answer to this label with no plist of ours (a hand-installed
                // predecessor, or a booted-out one still tearing down). `start` would see it loaded and
                // running and return Ok while our daemon never ran, and label-scoped checks cannot tell
                // whose job it is, so boot it out here, where the artifact is known to be new.
                //
                // Staging comes first, so a failure there stops nothing. A read or `bootout` that fails
                // leaves no plist, so a rerun is a `Create` again and reaches the stale job; after a
                // plist is written the rerun would read `UpToDate` and never look.
                let target = staging_path(spec.id.as_str());
                prepare_parent_dirs(spec, &account)?;
                let staged = staged_tempfile(target.parent().expect("plist path always has a parent"), desired)?;
                let loaded = never_expires(self.launchctl.is_loaded(spec.id.as_str(), deadline, reservation)?);
                if loaded {
                    never_expires(self.launchctl.bootout(spec.id.as_str(), deadline, reservation)?);
                }
                match persist_new(staged, &target)? {
                    PersistOutcome::Written => {}
                    // Something now exists where `read` saw nothing, and `is_loaded` has been called.
                    PersistOutcome::Raced => {
                        return Err(if loaded {
                            refuse::slot_taken_after_bootout(&spec.id, &target, describe_at(&target))
                        } else {
                            refuse::changed_during_install(&spec.id, &target)
                        });
                    }
                }
                self.recheck_after_write(&spec.id, deadline, &target, desired, None, reservation)?;
            }
            Outcome::Update { .. } | Outcome::Stale { .. } => {
                let snapshot = snapshot.expect("an id `decide` chose to update was read as marked");
                let previous = snapshot.text.as_str();
                // `Stale` is checked too: a release can change what the generator renders.
                let changes_job = refuse::changes_loaded_job(previous, desired);
                if changes_job && never_expires(self.launchctl.is_loaded(spec.id.as_str(), deadline, reservation)?) {
                    return Err(refuse::loaded_refusal(&spec.id));
                }
                prepare_parent_dirs(spec, &account)?;
                let target = snapshot.location.path.as_path();
                // `target` was captured by `read`, before `decide` ran. A concurrent `enable` or
                // `disable` may have moved it since, and writing here would put a plist in both
                // directories, which `locate` errors on; a concurrent `uninstall` would report
                // success and leave the write's plist installed. This check only narrows the race to
                // the window between it and `write_existing`'s rename onto `target`; it does not
                // close it.
                if !still_present(target)? {
                    return if changes_job {
                        // `is_loaded` has been called.
                        Err(refuse::changed_during_install(&spec.id, target))
                    } else {
                        Ok(WriteOutcome::Raced)
                    };
                }
                write_existing(target, desired)?;
                if changes_job {
                    self.recheck_after_write(&spec.id, deadline, target, desired, Some(previous), reservation)?;
                }
            }
            other => unreachable!("`flow::install` writes only `Create`, `Update` and `Stale`, got {other:?}"),
        }
        Ok(WriteOutcome::Done)
    }

    fn live_status(&self, snapshot: &Plist) -> Result<Status> {
        // `status` and `list` have no budget of their own: unbounded.
        // Read as `UncountedQuery`: neither verb counted this read, so it draws on no reservation
        // and is handed an empty one.
        let (state, pid) = self.launchctl.read_live_state(
            snapshot.id.as_str(),
            Budget::Unbounded.start(),
            Role::UncountedQuery,
            &mut Reservation::default(),
        )?;
        Ok(Status {
            state,
            pid,
            enabled: snapshot.location.enabled,
        })
    }

    fn scan(&self) -> Scan<Plist> {
        scan_in([(STAGING_DIR, false), (ENABLED_DIR, true)])
    }

    fn uninstall(&self, snapshot: Plist, reservation: &mut Reservation) -> Result<()> {
        let Plist { id, location, .. } = snapshot;

        // Unbounded, like `install`'s: `uninstall` has no `--timeout`, and bounding this would turn a
        // slow-stopping job into a failed uninstall.
        never_expires(
            self.launchctl
                .bootout(id.as_str(), Budget::Unbounded.start(), reservation)?,
        );
        fs::remove_file(&location.path).map_err(|e| Error::os_at("remove", &location.path, e))
    }

    fn enable(&self, snapshot: Plist) -> Result<()> {
        let Plist { id, location, .. } = snapshot;

        if location.enabled {
            return Ok(());
        }
        let dest = enabled_path(id.as_str());
        move_no_clobber(&location.path, &dest)
    }

    /// Does not stop the job if it is loaded, as [`ServiceManager::disable`] requires: launchd
    /// holds a bootstrapped job by label, whichever directory its plist is in.
    ///
    /// [`ServiceManager::disable`]: crate::manager::ServiceManager::disable
    fn disable(&self, snapshot: Plist) -> Result<()> {
        let Plist { id, location, .. } = snapshot;

        if !location.enabled {
            return Ok(());
        }
        let dest = staging_path(id.as_str());
        ensure_dir(Path::new(STAGING_DIR))?;
        move_no_clobber(&location.path, &dest)
    }

    fn start(&self, snapshot: Plist, budget: Budget, reservation: &mut Reservation) -> Result<()> {
        // First, so the wait is counted from the verb's entry (see `verb_deadline`).
        let deadline = verb_deadline(budget);
        let Plist { id, blob, location, .. } = snapshot;
        let id = &id;
        let blob = blob.map_err(Error::Kept)?;
        let timed_out = || budget::timed_out(id.as_str(), "running", budget);
        // Issuing the request is unbounded: the budget bounds waiting for launchd's answer, never
        // whether the request is sent, so a spent budget still reaches launchd.
        let issuing = Budget::Unbounded.start();

        if !never_expires(self.launchctl.is_loaded(id.as_str(), issuing, reservation)?) {
            if let Err(e) = self
                .launchctl
                .bootstrap(&location.path, issuing, reservation)
                .map(never_expires)
            {
                // A concurrent `start` may have loaded it since the check; propagate the error only
                // if the job is not loaded now either.
                if !never_expires(self.launchctl.is_loaded(id.as_str(), issuing, reservation)?) {
                    return Err(e);
                }
            }
        }
        // `kickstart` launches a job with no `KeepAlive`/`RunAtLoad` (`restart: never`); `bootstrap`
        // alone only loads it. Other policies are already started by `RunAtLoad`. `start` on a
        // running service must be `Ok` without restarting it, and launchd does not document that a
        // plain `kickstart` leaves a running instance alone, so check first.
        if self.launchctl.live_state(id.as_str(), issuing, reservation)?.0 != State::Running {
            // Bounded by `deadline`: `-p` is the wait for the pid. An expiry leaves the `launchctl`
            // running, so the request still goes out.
            match self
                .launchctl
                .kickstart(id.as_str(), budget, deadline, reservation)?
                .ok_or_else(timed_out)?
            {
                // launchd forked the job and named the pid: the confirmation.
                Kickstarted::Confirmed => {}
                // `-p` returned no pid this code trusts. launchd named one in every run measured
                // (250/250), so this case was never observed and is not guessed at: one bounded
                // read decides it.
                Kickstarted::NoPid if budget.waits() => {
                    let state = self.launchctl.live_state(id.as_str(), deadline, reservation)?.0;
                    if state != State::Running {
                        // A read the deadline cut short is `Unknown`: an expiry, not a job that
                        // failed to start.
                        return Err(if deadline.expired() {
                            timed_out()
                        } else {
                            not_started(id, state, blob.spec.restart == Restart::Always)
                        });
                    }
                }
                // The plain form prints no pid; the request was issued and nothing more is asked.
                Kickstarted::NoPid => {}
            }
        }

        // Verify the outcome, do not assume it. `is_loaded` and `live_state` key on the label and
        // cannot tell whose job answers to it: a job from a plist we just replaced, or one that was
        // booted out and is still tearing down, satisfies both, so each check above can be skipped
        // on the strength of a job about to cease to exist. That left a daemon stopped after
        // `install --start` returned `Ok`.
        //
        // Only `KeepAlive: true` (`restart: always`) licenses this check: launchd keeps such a job
        // running whenever it is loaded. Under `on-failure` or `never` a job may have run and
        // exited, and demanding `Running` would fail a correct start.
        //
        // Skipped when the budget does not wait: the block asserts state, and a budget that waits
        // for none (`--timeout 0`, whose `Ok(())` means only that the request was accepted) sees
        // `spawn scheduled`, which `state::classify` folds into `Unknown`.
        if budget.waits()
            && blob.spec.restart == Restart::Always
            && self.launchctl.live_state(id.as_str(), issuing, reservation)?.0 != State::Running
        {
            // One corrective cycle, not a retry loop: tear the stale job down by label and load ours.
            // Issued whatever is left of the budget, like the request above: when a stale job
            // answered to the label, this cycle is the request, and skipping it would report a start
            // never sent as one that may still arrive.
            never_expires(self.launchctl.bootout(id.as_str(), issuing, reservation)?);
            never_expires(self.launchctl.bootstrap(&location.path, issuing, reservation)?);
            if self.launchctl.live_state(id.as_str(), issuing, reservation)?.0 != State::Running {
                self.launchctl
                    .kickstart(id.as_str(), budget, deadline, reservation)?
                    .ok_or_else(timed_out)?;
            }
            let state = self.launchctl.live_state(id.as_str(), deadline, reservation)?.0;
            if state != State::Running {
                if deadline.expired() {
                    return Err(timed_out());
                }
                return Err(not_started(id, state, true));
            }
        }
        Ok(())
    }

    /// The plain request-only start. The stop before it ran `bootout` to completion (it has no
    /// request-only form), so the instance it stopped cannot be what answers for the label here.
    fn request_start_after_stop(&self, snapshot: Plist, reservation: &mut Reservation) -> Result<()> {
        backend::Backend::start(self, snapshot, Budget::Immediate, reservation)
    }

    fn stop(&self, snapshot: Plist, budget: Budget, reservation: &mut Reservation) -> Result<()> {
        // First, so the wait is counted from the verb's entry. A budget that does not wait gets an
        // unbounded deadline (see `verb_deadline`); under a bounded one an expiry leaves the
        // `bootout` running, so the request still goes out.
        let deadline = verb_deadline(budget);
        let id = snapshot.id();
        self.launchctl
            .bootout(id.as_str(), deadline, reservation)?
            .ok_or_else(|| budget::timed_out(id.as_str(), "stopped", budget))
    }

    fn kill(&self, snapshot: Plist, reservation: &mut Reservation) -> Result<()> {
        let id = snapshot.id();
        never_expires(
            self.launchctl
                .kill_job(id.as_str(), Budget::Unbounded.start(), reservation)?,
        );
        Ok(())
    }
}

// Enumeration =========================================================================================================

/// Every plist in `dirs` (each with whether it is the enabled one), as `read` would classify it.
///
/// A pass that stopped in one directory does not stop the other: scanning on only adds ids goetia
/// has established, and the aggregate in `reported` forbids concluding absence for anything either
/// pass missed. Both stopping gives two aggregates.
fn scan_in(dirs: [(&str, bool); 2]) -> Scan<Plist> {
    let mut by_id: BTreeMap<String, Vec<Location>> = BTreeMap::new();
    let mut incomplete = Vec::new();
    for (dir, enabled) in dirs {
        let pass = scan_plists(dir);
        for (id, path) in pass.plists {
            by_id.entry(id).or_default().push(Location { path, enabled });
        }
        incomplete.extend(pass.incomplete);
    }

    let mut entries = Vec::new();
    let mut reported = Vec::new();
    // A partial scan can narrow a positive claim but never supports absence: `reported` forbids
    // that. Untested: needs a mid-`readdir` fault on macOS.
    for (id, locations) in by_id {
        // Each location is read, because `/Library/LaunchDaemons` holds every vendor's daemons: a
        // stem found in both is goetia's only if one of the two carries the marker.
        let mut found = Vec::new();
        let mut unestablished = None;
        for location in locations {
            match read_at(&id, location) {
                Ok(found_here) => found.push(found_here),
                // Gone since the scan: an uninstall completed. Absence is established, so skip;
                // reporting it would let a benign concurrent uninstall raise `list`'s exit code.
                Err(e) if matches!(e.cause(), Error::NotInstalled { .. }) => {}
                // Undetermined, not absence or ownership: the failed read established neither. The
                // read is its reason; the `status` recovery is left out.
                Err(e) => {
                    let failed = match e.into_undetermined() {
                        Ok((read, _recovery)) => *read,
                        Err(e) => e,
                    };
                    unestablished = Some(match unestablished {
                        None => failed,
                        Some(first) => Error::Both {
                            first: Box::new(first),
                            then: Box::new(failed),
                        },
                    });
                }
            }
        }
        if let Some(failed) = unestablished {
            reported.push(Installed::Undetermined {
                name: Some(id),
                reason: Reason::failed(failed),
            });
            continue;
        }
        match <[_; 1]>::try_from(found) {
            Ok([only]) => {
                let entry = ScanEntry::of(&id, only, Plist::new);
                entries.push((id, entry));
            }
            // Two plists, or none left: only a marker among them makes the id goetia's.
            Err(found) => {
                if found.iter().any(|f| matches!(f, ReadOutcome::Marked(_))) {
                    reported.push(Installed::OursUnusable {
                        name: id,
                        reason: Reason::said(format!("present in both {STAGING_DIR} and {ENABLED_DIR}")),
                    });
                }
            }
        }
    }
    // Last, so a pass that got some way in still reports what it named before what it could
    // not — the order `cli::support::partition_installed` imposes on the rendered output too.
    reported.extend(incomplete);
    Scan { entries, reported }
}

/// What one pass over one of `list`'s two directories found: the `<id>.plist` files it reached,
/// and — when the pass stopped early — the entry standing for whatever it never did.
#[derive(Debug)]
struct PlistScan {
    plists: Vec<(String, PathBuf)>,
    incomplete: Option<Installed>,
}

/// Enumerate `dir`. A pass that cannot start or finish is reported, not propagated: an `Err` out of
/// `list` discards everything already classified, including the other directory, and reaches the
/// CLI as an empty document on exit `1`. See [`flow::scan`].
///
/// A missing `dir` is a silent skip: the staging directory does not exist until the first
/// `install`, which means nothing is staged.
fn scan_plists(dir: &str) -> PlistScan {
    let mut plists = Vec::new();
    let incomplete = flow::scan(
        dir,
        fs::read_dir(dir),
        |entries| Box::new(entries.map(|entry| entry.map(|entry| entry.path()))),
        flow::directory_detail,
        |path: PathBuf| {
            if path.extension().and_then(|e| e.to_str()) != Some("plist") {
                return;
            }
            if let Some(id) = path.file_stem().and_then(|s| s.to_str()).map(str::to_string) {
                plists.push((id, path));
            }
        },
    );
    PlistScan { plists, incomplete }
}

#[cfg(test)]
#[path = "manager_tests.rs"]
mod manager_tests;
