//! The effectful launchd backend: [`Launchd`].
//!
//! The plist's *directory* is the enrollment bit, never a plist key:
//!
//! - `install` writes the plist to [`STAGING_DIR`] (or, for an update, where the existing artifact
//!   already lives; see `locate`). launchd scans neither directory on its own, so nothing loads
//!   and nothing runs. A plist that would change what a loaded job reads is refused and nothing is
//!   written (see `refuse`). A `Create` over a loaded job of the same label boots that job out
//!   first.
//! - `start` `launchctl bootstrap`s the plist from wherever it is (if not already loaded), then
//!   `launchctl kickstart`s it, which is what launches a job with no `KeepAlive`/`RunAtLoad` (a
//!   `restart: never` daemon). Under a budget that waits, the kickstart asks for the pid (`-p`)
//!   and so blocks on launchd's respawn throttle whenever the job has respawn history, as a
//!   crash-looping `restart: always` job has once launchd has run it. The throttle is launchd's
//!   `ThrottleInterval`: goetia writes it from `spec.restart_delay` when `restart` is not `never`,
//!   and launchd's default is about 10s. The throttle runs from the
//!   job's last fork, not from the kick, and the budget from the verb's entry. `WaitTimeout` occurs
//!   when the throttle left at the kick outlasts the budget left then: under the default throttle
//!   and [`Budget::DEFAULT`] (10s), only for a verb entered within about 20ms of the last fork.
//!   Otherwise the verify and corrective path can give `Ok`,
//!   `WaitTimeout`, `StartUnconfirmed` or `NotStarted`. `bootstrap` returns at load, before the
//!   `RunAtLoad` run is forked, so a `start` straight after it may meet no throttle and give any of
//!   those four. On `WaitTimeout` the request stands. Its `launchctl` is ended only once one
//!   `launchctl print` shows launchd holding the start (`Evidence`): ended before launchd has the
//!   request, the start is lost. On any doubt, including a `print` that fails, is cut or outlasts
//!   `HELD_READ_BOUND`, it is left running until the throttle ends: up to `restart_delay`. The budget
//!   bounds the wait for the pid and nothing else: every `launchctl` call that issues the request
//!   runs to completion. The throttle is keyed to the job, so `bootout` plus `bootstrap` does not
//!   clear it and there is no remedy to apply.
//! - `stop` `launchctl bootout`s the job, from whichever directory it was loaded, and waits for it to
//!   be removed (see "Waits for a job's removal").
//! - `enable` moves the plist from [`STAGING_DIR`] into [`ENABLED_DIR`] (`/Library/LaunchDaemons`).
//!   Does not start it.
//! - `disable` moves the plist back to [`STAGING_DIR`]. It does not `bootout` the job:
//!   [`ServiceManager::disable`](crate::manager::ServiceManager::disable) promises not to stop a
//!   running one, and launchd holds a bootstrapped job by label, whichever directory its plist is
//!   in, so moving the file leaves it running.
//! - A task is two plists, the job `<id>.plist` and its timer `<id>@timer.plist`; reading and
//!   `status` treat them as one id (see `task`), and `install`, `uninstall`, `enable` and `disable`
//!   change both, in the order the plans of `launchd::task` give (see `write`). A task's `start` loads
//!   its job and kicks it, and its `stop` sends SIGTERM to the run in flight and never boots the job
//!   out (see `verbs`). Its `run` holds the job's next launch with `launchctl debug --start-suspended`
//!   until goetia is watching the process it names, then resumes it (see `run`).
//! - Reading (`list`, and every verb's ownership check) scans both directories, never `launchctl
//!   print`, whose format Apple does not guarantee. `status`'s live run state is the one exception,
//!   since no file says whether a job is running. It reads `launchctl print` leniently: a shape it
//!   does not recognize degrades to [`State::Unknown`].
//!
//! # Waits for a job's removal
//!
//! `launchctl bootout` returns within milliseconds, but launchd goes on answering for the label
//! (`print` shows `state = SIGTERMed`) until the job has exited. A step that acts on the label
//! afterwards must not meet the old job, so it waits until `print` reports the label not found (see
//! `Launchctl::bootout`). Which steps wait, under which bound, and when a job may never go, is
//! listed once, in the README's "Waiting for a start or stop".
//!
//! A start's wait that has no bound is part of sending a request: bounded by a deadline, it could
//! end with the old job still loaded, the `bootstrap` unsent and the daemon left unloaded. `install`
//! and `uninstall` wait without bound because they have no `--timeout`.
//!
//! Only the modern verbs (`bootstrap`/`bootout`/`kickstart`/`print`) are used, never legacy
//! `load`/`unload`. `launchctl enable`/`disable` and the plist's `Disabled` key are never touched:
//! the override entries the former creates cannot be removed, and `bootstrap` refuses a `Disabled`
//! plist.

use std::collections::BTreeMap;
use std::io::{Read as _, Write as _};
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::Duration;
use std::{fs, io};

use cosca::Process;

use crate::backend::account::{Account, resolve_account};
use crate::backend::bounded::{self, Finished, Kept, Left, Reservation, Role};
use crate::backend::client::{Client, ClientError, Real, Wait};
use crate::backend::created_dirs::{CreatedDirs, Planned, refuse_writable_log_dir};
use crate::backend::kill::{KillOutcome, kill_failed, kill_service, process};
use crate::backend::launchd::generate::Plists;
use crate::backend::launchd::state::Evidence;
use crate::backend::launchd::task::enabled_in;
use crate::backend::launchd::{generate, refuse, state};
use crate::backend::{
    self, Decoded, Identity, InstalledDefinition, InstalledService, ReadOutcome, Scan, ScanEntry, Standing,
    WriteOutcome,
};
use crate::blob::Blob;
use crate::decide::{ExternalConfig, Outcome};
use crate::error::{Error, Occupant, PutBackFound, Reason, Result, carried};
use crate::manager::budget::{self, Deadline};
use crate::manager::{Budget, DaemonStatus, Installed, State, Status, Step, flow};
use crate::spec::{DaemonKind, DaemonSpec, Id, Restart, StopAction};

pub use super::{ENABLED_DIR, STAGING_DIR};
use task::{Artifact, Assembled, Body};

mod interrupts;
mod kill;
mod proc;
mod run;
mod task;
mod verbs;
mod watch;
mod write;

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

/// `<stem>.plist` in `dir`: the one place a plist's file name is built.
fn plist_in(dir: &Path, stem: &str) -> PathBuf {
    dir.join(format!("{stem}.plist"))
}

fn staging_path(id: &str) -> PathBuf {
    plist_in(Path::new(STAGING_DIR), id)
}

fn enabled_path(id: &str) -> PathBuf {
    plist_in(Path::new(ENABLED_DIR), id)
}

fn target(id: &str) -> String {
    format!("system/{id}")
}

/// Where a plist lives, and whether that is the enabled directory.
struct Location {
    path: PathBuf,
    enabled: bool,
}

/// The two directories a plist can be in, each with whether it is the enabled one: staging first.
type Dirs<'a> = [(&'a str, bool); 2];

/// The directories goetia's plists are in.
const DIRS: Dirs<'static> = [(STAGING_DIR, false), (ENABLED_DIR, true)];

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

/// Where the plist `<stem>.plist` of `id` lives in `dirs`, and whether that is the enabled directory.
/// `Ok(None)` if it lives in neither. `stem` is `id` for the job and [`generate::timer_label`] of it
/// for a task's timer: one mechanism for both slots.
///
/// `Err` if a plist exists at both paths. No operation of this backend leaves that (`enable` and
/// `disable` move, never copy), but a hand-edit or a crash mid-move could; preferring one would
/// make the other's content vanish from every command, so both paths are named instead.
fn locate(dirs: Dirs, id: &str, stem: &str) -> Result<Option<Location>> {
    let path = |(dir, _): (&str, bool)| plist_in(Path::new(dir), stem);
    let (staging, enabled) = (path(dirs[0]), path(dirs[1]));
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
    /// Valid UTF-8: the only form `generate::plists` writes, and the only one in which
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
/// different regular file than step 1 stat'd: the benign content race a plain `fs::read` has,
/// which no verdict depends on.
///
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
/// (`task::read_at`).
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

/// A marked id, as `read` saw it: the job's plist and, for a task, its timer's, each with where it is
/// and its text. `blob` is the job's metadata, or why there is none: it will not decode, or the job is
/// missing (a timer alone at the id, which is goetia's and unusable). So a snapshot whose blob decoded
/// has a job.
pub(crate) struct Plist {
    id: Id,
    blob: Decoded,
    job: Option<Artifact>,
    timer: Option<Artifact>,
}

impl Plist {
    fn new(id: Id, blob: Decoded, Body { job, timer }: Body) -> Self {
        Self { id, blob, job, timer }
    }

    /// The job's plist. For an id whose job is missing, the error that says so, which the verbs that
    /// act on a job's file return: its blob is that error.
    fn job(&self) -> Result<&Artifact> {
        self.job.as_ref().ok_or_else(|| match &self.blob {
            Err(e) => Error::Kept(Arc::clone(e)),
            Ok(_) => Error::blob("the job's plist is missing"),
        })
    }

    fn is_task(&self) -> bool {
        matches!(&self.blob, Ok(blob) if blob.spec.kind() == DaemonKind::Task)
    }

    /// The `stop-action` the job's metadata carries, or `None` where it does not decode.
    fn stop_action(&self) -> Option<StopAction> {
        self.blob.as_ref().ok().map(|blob| *blob.spec.stop_action())
    }

    /// Where a task is enabled: see [`enabled_in`].
    fn enabled(&self) -> bool {
        enabled_in(
            self.timer.as_ref().map(|artifact| artifact.location.enabled),
            self.job.as_ref().map(|artifact| artifact.location.enabled),
        )
    }

    /// The kind the verbs that start, stop, kill, enable and disable treat the id as: what the job's
    /// metadata names, and, where it will not decode, a task if a goetia timer sits beside the job. A
    /// service whose metadata is damaged must keep working as one, and a task's timer is what says it
    /// is not.
    fn kind(&self) -> DaemonKind {
        match &self.blob {
            Ok(blob) => blob.spec.kind(),
            Err(_) if self.timer.is_some() => DaemonKind::Task,
            Err(_) => DaemonKind::Service,
        }
    }

    /// Whether `install` and `uninstall` act on a job and a timer: a task, and any id with a timer of
    /// goetia's beside it, which one of them would otherwise leave behind. Where it is false the id is
    /// a service, and its verbs are the service's.
    fn has_two_plists(&self) -> bool {
        self.is_task() || self.timer.is_some()
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

// Account resolution ==================================================================================================

/// The [`Identity`] a generator needs of an account. Every account here is resolved by name: `launchd.plist(5)`
/// does not say whether `UserName` accepts a numeric string, so a uid goes through a passwd lookup too, and
/// one with no entry is an error.
impl Account {
    fn identity(&self) -> Identity {
        Identity {
            user: self.name.clone(),
        }
    }
}

// Writing the plist ===================================================================================================

/// `create_dir_all` the directory a plist is about to be written into, then make it `0755`
/// regardless of the caller's umask: `list`/`status`/`show` must work unelevated, which needs every
/// ancestor world-readable and searchable.
fn ensure_dir(dir: &Path) -> Result<()> {
    fs::create_dir_all(dir).map_err(|e| Error::os_at("create directory", dir, e))?;
    fs::set_permissions(dir, fs::Permissions::from_mode(0o755)).map_err(|e| Error::os_at("chmod", dir, e))
}

/// What `prepare_parent_dirs` would make, read and judged, nothing made: `cwd` itself and the parent of
/// `logs` (a file path; launchd creates the log file but not its directory), each with the account it
/// is to be handed to, which root is not.
/// launchd opens the log and enters `cwd` as the job's user, so a non-root daemon whose
/// `StandardOutPath` is in a root-only directory fails at launch with an opaque status, and a missing
/// `WorkingDirectory` refuses the job outright. A job that runs as root opens its log as root, which
/// follows a symlink there, so its log's directory is judged for what an account can do with it
/// (`refuse_writable_log_dir`); a job that runs as another user opens it as that user and is not.
/// Every refusal is here, before anything is made or anything else is changed.
fn plan_parent_dirs(spec: &DaemonSpec, account: &Account, created: &CreatedDirs) -> Result<Planned> {
    let owner = account.owner()?;
    let common = spec.common();
    let parent_of_logs = common
        .logs
        .as_deref()
        .and_then(Path::parent)
        .filter(|parent| !parent.as_os_str().is_empty());
    let cwd = common.cwd.as_deref().map(|cwd| created.plan(cwd)).transpose()?;
    let log = parent_of_logs.map(|parent| created.plan(parent)).transpose()?;
    if let (true, Some(log), Some(logs)) = (account.uid.is_root(), &log, &common.logs) {
        refuse_writable_log_dir(logs, log, common.id.as_str(), created)?;
    }
    Ok(cwd.into_iter().chain(log).map(|plan| (plan, owner)).collect())
}

/// Make `cwd` itself and the parent of `logs` where they are missing, world-readable and searchable,
/// and hand each to `account` before this returns: the job can run as soon as the write that follows
/// loads it. Both are judged before either is made: see `plan_parent_dirs`.
fn prepare_parent_dirs(spec: &DaemonSpec, account: &Account, created: &mut CreatedDirs) -> Result<()> {
    let plans = plan_parent_dirs(spec, account, created)?;
    created.make(plans)
}

/// What a read of the id says of whether `desired`'s job is the one that stands: the plist is what
/// names the directories, so a task's timer is not compared.
fn plist_standing(read: Result<ReadOutcome<Plist>>, desired: &Plists) -> Standing {
    match read {
        Ok(ReadOutcome::Marked(plist)) => match &plist.job {
            Some(job) if job.text == desired.job => Standing::Yes,
            _ => Standing::No,
        },
        Ok(_) => Standing::No,
        Err(e) => Standing::Unknown(format!("could not read the plist: {}", e.report())),
    }
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

/// [`persist_new`] for a `Create`, where a `Raced` is an error: something now exists where `read` saw
/// nothing, and a `launchctl` call has been made since, so another pass would draw more than the verb
/// reserved. `booted_out` is whether a loaded job was booted out first, which the error then says.
fn persist_new_unraced(staged: tempfile::NamedTempFile, target: &Path, id: &Id, booted_out: bool) -> Result<()> {
    match persist_new(staged, target)? {
        PersistOutcome::Written => Ok(()),
        PersistOutcome::Raced if booted_out => Err(refuse::slot_taken_after_bootout(id, target, describe_at(target))),
        PersistOutcome::Raced => Err(refuse::changed_during_install(id, target)),
    }
}

/// Overwrite the plist already at `target`, for `Update`/`Stale` only: `read` has established the
/// id is marked.
fn write_existing(target: &Path, content: &str) -> Result<()> {
    persist_over(
        staged_tempfile(target.parent().expect("plist path always has a parent"), content)?,
        target,
    )
}

/// Replace the plist at `target` with `staged`, which is already linted: a task stages both its plists
/// before its first `launchctl` request, so a malformed one stops the install before anything changed.
fn persist_over(staged: tempfile::NamedTempFile, target: &Path) -> Result<()> {
    staged
        .persist(target)
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
        State::Stopped | State::Failed | State::Running | State::Idle => Error::NotStarted {
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

/// [`run()`] for a caller that only needs success: a spawn failure is an [`Error::Spawn`], a non-zero
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
/// A budget that does not wait gets an unbounded deadline, not `budget.start()`: a non-waiting
/// `stop` still waits until launchd no longer has the job (see [`Launchctl::bootout`]). What a
/// non-waiting budget declines to wait for is the pid, and the plain `kickstart` is what declines
/// it.
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
    /// launchd forked the job and named this pid on a whole capture. That a pid parsed is the
    /// confirmation.
    Confirmed(u32),
    /// `kickstart` succeeded but named no pid this code will trust: the plain form, which never
    /// prints one, or a `-p` whose stdout is not the whole `<digits>\n` line.
    NoPid,
}

/// How long the `print` after an expired `kickstart` may take before it counts as doubt. A failure
/// bound for an external process that might never exit, never taken as proof of state: expiry only
/// leaves the `kickstart` running. `print` answers in milliseconds.
const HELD_READ_BOUND: Duration = Duration::from_secs(5);

/// How long [`Launchctl::bootout`]'s wait rests between two reads of the label. Pacing, never proof
/// of state: each read asks launchd, and the wait ends on its answer.
const UNLOAD_POLL: Duration = Duration::from_millis(50);

/// Exit codes `launchctl` uses for "no such job": 3 ("no such process") and 113 ("could not find
/// specified service").
fn is_not_found(code: Option<i32>) -> bool {
    matches!(code, Some(3 | 113))
}

/// What `launchctl print` answered for a label that did not fail.
enum Printed {
    /// Not found: launchd has no job under it.
    Absent,
    Present(bounded::Capture),
}

/// What [`Launchctl::wait_print`] waits for.
#[derive(Clone, Copy)]
enum Until<'a> {
    /// launchd no longer has the label.
    Unloaded,
    /// A task's job has a process, or no longer shows `state = spawn scheduled`: a start launchd has
    /// accepted has forked, or has not happened and will not. `kickstart` returns before the fork, and
    /// `launchctl kill` of a job in that state signals nothing, so a `stop` waits for this first.
    ProcessOrNotScheduled,
    /// The end of the process `held` is recorded: `print` shows another pid, or none (a job launchd
    /// holds that has exited, or is throttled and `spawn scheduled`), or the label is not held, or it
    /// shows `pid` on a process other than the held one that `procinfo` names as the job's (launchd
    /// started the job again and the number was reused). A `pid` that `print` shows over a process
    /// `procinfo` does not name is a stranger's: `print` is stale, and the wait goes on. `held` is
    /// `None` for a process that was gone before it could be held.
    Recorded { pid: u32, held: Option<&'a Process> },
}

impl Until<'_> {
    /// Whether `printed` reaches `self`. `names` is [`Launchctl::names_launch`] of a pid, for the one
    /// case that needs the process behind a pid shown by `print`.
    fn reached(self, printed: &Printed, names: impl FnOnce(u32) -> Result<bool>) -> Result<bool> {
        Ok(match (self, printed) {
            // Not found: not loaded, so no process and no start.
            (_, Printed::Absent) => true,
            (Until::Unloaded, Printed::Present(_)) => false,
            // A body that is not whole says nothing: the read is made again.
            (Until::ProcessOrNotScheduled, Printed::Present(capture)) => {
                capture.complete && {
                    let body = String::from_utf8_lossy(&capture.stdout);
                    state::has_process(&body).is_some() || !state::spawn_scheduled(&body)
                }
            }
            (Until::Recorded { pid, held }, Printed::Present(capture)) => {
                capture.complete
                    && process::recorded(
                        state::has_process(&String::from_utf8_lossy(&capture.stdout)),
                        held,
                        pid,
                        || names(pid).map_err(io::Error::other),
                    )
                    .map_err(|e| carried(e).unwrap_or_else(|e| Error::os(format!("read what has process {pid}"), e)))?
            }
        })
    }
}

/// What `launchctl kill` answered.
#[derive(Debug, PartialEq, Eq)]
enum SignalOutcome {
    Sent,
    /// The job has no process to signal: it is not loaded (113), or loaded with none (3).
    NoProcess,
}

/// What `launchctl debug` exits with for a hold that is already pending: 37, "Operation already in
/// progress" (measured on macOS CI).
const DEBUG_IN_PROGRESS: i32 = 37;

/// The files of one more read over `kept`, for a read repeated as often as it takes. A failure is a
/// read that could not be made, never a job that is gone.
fn reread(id: &str, kept: &Kept) -> Result<Reservation> {
    kept.again()
        .map_err(|e| failed(&["print", target(id).as_str()], false, ClientError::NotRun(e.into())))
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
    /// expires is left running ([`Finished::Left`]), and killed only by a caller that knows launchd
    /// has the request, since `launchctl` never says so itself. Its reaper (see [`request`]) and the
    /// temp files its output goes to are drawn from `reservation`.
    ///
    /// [`Finished::Expired`], or for a request [`Finished::Left`], iff the deadline expired first.
    /// For a request that is not a failure of the command: the request is out, or on its way in a
    /// `launchctl` left running. What it means differs per verb, so the verb decides. A failure
    /// before `launchctl` ran is an [`Error::Spawn`]. One after it may have (cosca failing past
    /// `exec`, or the wait failing) is the same for a read, and [`Error::RequestInDoubt`] for a
    /// request, which may have reached launchd.
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
    /// an `Err`, since callers act on "not loaded" by writing or bootstrapping. Only the exit code is
    /// read, never the body of `print`'s output.
    fn is_loaded(&self, id: &str, deadline: Deadline, reservation: &mut Reservation) -> Result<Option<bool>> {
        Ok(self
            .print(id, deadline, Role::Query, reservation)?
            .map(|printed| matches!(printed, Printed::Present(_))))
    }

    /// `launchctl print` for `id`. `Ok(None)` iff `deadline` expired. Any failure but the not-found codes
    /// is an `Err`.
    fn print(
        &self,
        id: &str,
        deadline: Deadline,
        role: Role,
        reservation: &mut Reservation,
    ) -> Result<Option<Printed>> {
        let target = target(id);
        let args = ["print", target.as_str()];
        let Finished::Exited { status, capture } = self.run(&args, deadline, role, reservation)? else {
            return Ok(None);
        };
        if status.success() {
            Ok(Some(Printed::Present(capture)))
        } else if is_not_found(status.code()) {
            Ok(Some(Printed::Absent))
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
    ///
    /// On an expiry the `kickstart` is left running unless a `print` of the job shows `evidence`: see
    /// [`Self::end_if_held`].
    fn kickstart(
        &self,
        id: &str,
        budget: Budget,
        deadline: Deadline,
        evidence: Evidence,
        reservation: &mut Reservation,
    ) -> Result<Option<Kickstarted>> {
        let target = target(id);
        let args: Vec<&str> = if budget.waits() {
            vec!["kickstart", "-p", &target]
        } else {
            vec!["kickstart", &target]
        };
        let role = request(reservation)?;
        let (status, capture) = match self.run(&args, deadline, role, reservation)? {
            Finished::Exited { status, capture } => (status, capture),
            Finished::Left(left) => {
                self.end_if_held(id, left, evidence, reservation);
                return Ok(None);
            }
            Finished::Expired => return Ok(None),
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
            Some(pid) => Kickstarted::Confirmed(pid),
            None => Kickstarted::NoPid,
        }))
    }

    /// Ends the `kickstart` that `left` holds if one `launchctl print` shows `evidence`. A `print`
    /// that failed, is not whole or outlasts [`HELD_READ_BOUND`] is doubt, and doubt leaves it running.
    fn end_if_held(&self, id: &str, left: Left, evidence: Evidence, reservation: &mut Reservation) {
        let target = target(id);
        let args = ["print", target.as_str()];
        let deadline = Budget::Bounded(HELD_READ_BOUND).start();
        let held = match self.run(&args, deadline, Role::Query, reservation) {
            Ok(Finished::Exited { status, capture }) => {
                status.success() && capture.complete && evidence.shown_by(&String::from_utf8_lossy(&capture.stdout))
            }
            Ok(Finished::Left(_)) => unreachable!("a query is never left running"),
            Ok(Finished::Expired) | Err(_) => false,
        };
        if held {
            left.kill();
        }
    }

    /// `bootout`, then the wait until launchd no longer has `id` loaded. `Ok(None)` iff `deadline`
    /// expired, in either.
    ///
    /// `launchctl bootout` returns within milliseconds, not when the job's process has exited: until
    /// then `print` still answers for the label (`state = SIGTERMed`). That lasts until the job has
    /// exited; the README's "Waiting for a start or stop" says when it may not. A caller that acts on
    /// the label after a `bootout` (`bootstrap`, a read, a plist removed) must not meet the old
    /// instance, so this waits for it, under `deadline`: a caller that cannot be cut short passes an
    /// unbounded one. The wait is over when `print` reports the label not found, whatever job answered
    /// for it before: a label someone else loads again meanwhile keeps the wait going until `deadline`.
    ///
    /// No `is_loaded` pre-check: the job could unload between check and act (a concurrent `stop`, an
    /// operator's `launchctl bootout`), failing this call for an already stopped service and
    /// breaking the idempotency [`ServiceManager::stop`] requires. Success and the
    /// [`is_not_found`] codes are both `Ok`.
    ///
    /// What the wait needs is taken from `reservation` before anything is sent, so an
    /// [`Error::Reservation`] means nothing was.
    ///
    /// [`ServiceManager::stop`]: crate::manager::ServiceManager::stop
    fn bootout(&self, id: &str, deadline: Deadline, reservation: &mut Reservation) -> Result<Option<()>> {
        let kept = reservation
            .keep(Role::COUNTED_FILES)
            .map_err(|source| Error::Reservation { id: None, source })?;
        let target = target(id);
        let args = ["bootout", target.as_str()];
        let role = request(reservation)?;
        let Finished::Exited { status, capture } = self.run(&args, deadline, role, reservation)? else {
            return Ok(None);
        };
        if !(status.success() || is_not_found(status.code())) {
            return Err(command_failed(&args, status, &capture.stderr));
        }
        self.wait_print(id, deadline, &kept, Until::Unloaded)
    }

    /// Reads `print` of `id` until it shows `until`. `Ok(None)` iff `deadline` expired first.
    ///
    /// Every read re-checks the real condition; [`UNLOAD_POLL`] only paces them. How many reads the
    /// wait takes is not known when the verb reserves (a job's exit, a start launchd has accepted), so
    /// the wait is one call, its files taken once by the caller as `kept`: no read can fail for want of
    /// a file once the request is out. A read that fails is an `Err`, never a condition reached.
    ///
    /// A deadline already spent still gets one last read, under [`HELD_READ_BOUND`] (`print` answers
    /// in milliseconds), so a condition reached in the last interval is reported; no read is made
    /// under a spent deadline, which would be killed at once.
    fn wait_print(&self, id: &str, deadline: Deadline, kept: &Kept, until: Until<'_>) -> Result<Option<()>> {
        loop {
            let spent = deadline.expired();
            let read_deadline = if spent {
                Budget::Bounded(HELD_READ_BOUND).start()
            } else {
                deadline
            };
            match self.print(id, read_deadline, Role::Query, &mut reread(id, kept)?)? {
                Some(printed) if until.reached(&printed, |pid| self.names_launch_again(id, pid, kept))? => {
                    return Ok(Some(()));
                }
                Some(_) if !spent => {}
                Some(_) | None => return Ok(None),
            }
            let pause = deadline.remaining().map_or(UNLOAD_POLL, |left| left.min(UNLOAD_POLL));
            std::thread::sleep(pause);
        }
    }

    /// `launchctl kill <signal>` on the job `id`, never a pid: launchd signals what runs under the label
    /// now, so a pid that was reused cannot be hit. What happens next is the plist's `KeepAlive`.
    /// Unbounded: it returns as soon as launchd has the signal sent. [`SignalOutcome::NoProcess`]
    /// is the [`is_not_found`] codes, which say the job has no process to signal; what that means is the
    /// verb's to say. `kill` does not use it: see [`kill::JobKill`].
    fn signal(&self, id: &str, signal: &str, reservation: &mut Reservation) -> Result<SignalOutcome> {
        let target = target(id);
        let args = ["kill", signal, target.as_str()];
        let (status, capture) = self.request_to_end(&args, reservation)?;
        if status.success() {
            Ok(SignalOutcome::Sent)
        } else if is_not_found(status.code()) {
            Ok(SignalOutcome::NoProcess)
        } else {
            Err(command_failed(&args, status, &capture.stderr))
        }
    }

    /// The pid `print` shows for the job `id`, in any state: `None` for a label launchd does not hold
    /// and for a job it holds with no process (`spawn scheduled` included). A read that fails is an
    /// `Err`, never no process. Unbounded, and counted in no verb's reservation: the files of the read
    /// are made on the spot, as for `status`.
    fn job_pid(&self, id: &str) -> Result<Option<u32>> {
        let printed = self.print(
            id,
            Budget::Unbounded.start(),
            Role::UncountedQuery,
            &mut Reservation::default(),
        )?;
        match printed.expect("a call under an unbounded deadline never expires") {
            Printed::Absent => Ok(None),
            Printed::Present(capture) => {
                debug_assert!(capture.complete, "a read under an unbounded deadline is whole");
                Ok(state::has_process(&String::from_utf8_lossy(&capture.stdout)))
            }
        }
    }

    /// [`Self::names_launch`] drawing its files from `kept`, the one counted call a wait or a hold reads
    /// through. A failure to take them is an [`Error::Reservation`].
    fn names_launch_again(&self, id: &str, pid: u32, kept: &Kept) -> Result<bool> {
        let mut files = kept.again().map_err(|source| Error::Reservation { id: None, source })?;
        self.names_launch(id, pid, &mut files)
    }

    /// Whether `launchctl procinfo <pid>` names `pid` as the launch of the job `id`: its output has the
    /// job's block, the line `system/<id> = {`. launchd's own pid-to-job lookup answers it, from the
    /// `xpcproxy` stage on and with no lag, for the launch and never for a descendant, a stranger or a
    /// dead pid (KB, "macOS - launchctl procinfo names a launch's job from the xpcproxy stage on, in every
    /// call"). Its exit code is 0 for all of those and says nothing, and its stderr is noise: only the
    /// text is read. A `pid` of a process that is not the launch, or is gone, is `false`; `launchctl`
    /// that cannot be run is an `Err`. Unbounded, as [`Self::signal`] is; a read, with no reaper.
    fn names_launch(&self, id: &str, pid: u32, reservation: &mut Reservation) -> Result<bool> {
        let pid = pid.to_string();
        let args = ["procinfo", pid.as_str()];
        let Finished::Exited { capture, .. } = self.run(&args, Budget::Unbounded.start(), Role::Query, reservation)?
        else {
            unreachable!("a call under an unbounded deadline never expires")
        };
        debug_assert!(capture.complete, "a read under an unbounded deadline is whole");
        let block = format!("system/{id} = {{");
        Ok(String::from_utf8_lossy(&capture.stdout)
            .lines()
            .any(|line| line == block))
    }

    /// `launchctl debug <id> --start-suspended`: the next launch of the job starts stopped, whoever starts
    /// it, and stays so until it is sent SIGCONT. One launch only, and a hold already pending (exit 37) is
    /// not replaced, so what holds the next launch is one hold either way, and both answer `Ok`. Unbounded,
    /// as [`Self::signal`] is.
    ///
    /// The answer is `ta`, [`proc::now`] read after the `launchctl debug` process was reaped: a launch of
    /// the job that started after `ta` began after this call.
    fn debug_start_suspended(&self, id: &str, reservation: &mut Reservation) -> Result<u64> {
        let target = target(id);
        let args = ["debug", target.as_str(), "--start-suspended"];
        let (status, capture) = self.request_to_end(&args, reservation)?;
        let ta = proc::now();
        if status.success() || status.code() == Some(DEBUG_IN_PROGRESS) {
            Ok(ta)
        } else {
            Err(command_failed(&args, status, &capture.stderr))
        }
    }

    /// `args` as a request that runs to its end: the deadline is unbounded, so it never expires and what
    /// it answered is whole.
    fn request_to_end(
        &self,
        args: &[&str],
        reservation: &mut Reservation,
    ) -> Result<(std::process::ExitStatus, bounded::Capture)> {
        let role = request(reservation)?;
        match self.run(args, Budget::Unbounded.start(), role, reservation)? {
            Finished::Exited { status, capture } => Ok((status, capture)),
            Finished::Left(_) | Finished::Expired => unreachable!("a call under an unbounded deadline never expires"),
        }
    }

    /// Loads the job `id` from `path`, unless launchd holds it already: what `start` of a service and of
    /// a task do first. A job being torn down (after a `stop` that timed out, or another program's
    /// `launchctl bootout`) answers for the label and would pass for a loaded job, so `bootstrap` would
    /// be skipped and the job gone once it exits: it is waited out, then loaded. That wait is part of
    /// sending the request, so it is under no budget.
    ///
    /// Draws a kept call's files (the read and the wait share them), the `bootstrap`, and, where that is
    /// refused, one more read.
    fn load_job(&self, id: &str, path: &Path, reservation: &mut Reservation) -> Result<()> {
        // Under a deadline that never expires, so the capture is whole.
        let issuing = Budget::Unbounded.start();
        // Taken before anything is sent, so a `Reservation` error means nothing was.
        let kept = reservation
            .keep(Role::COUNTED_FILES)
            .map_err(|source| Error::Reservation { id: None, source })?;
        // The one read of the label that parses `print`'s body, for the one question its exit code
        // cannot answer: whether the job is being torn down.
        let mut held = reread(id, &kept)?;
        let loaded = match never_expires(self.print(id, issuing, Role::Query, &mut held)?) {
            Printed::Absent => false,
            Printed::Present(capture) => {
                debug_assert!(capture.complete, "a read under an unbounded deadline is whole");
                if state::tearing_down(&String::from_utf8_lossy(&capture.stdout)) {
                    never_expires(self.wait_print(id, issuing, &kept, Until::Unloaded)?);
                    false
                } else {
                    true
                }
            }
        };
        if !loaded && let Err(e) = self.bootstrap(path, issuing, reservation).map(never_expires) {
            // A concurrent `start` may have loaded it since the check; propagate the error only
            // if the job is not loaded now either.
            if !never_expires(self.is_loaded(id, issuing, reservation)?) {
                return Err(e);
            }
        }
        Ok(())
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
        let Some((status, capture)) = self.read_print(id, deadline, role, reservation)? else {
            return Ok((State::Unknown, None));
        };
        Ok(classified(status, &capture, DaemonKind::Service))
    }

    /// One `print` of `id` as `role`, whatever it answered. `Ok(None)` iff `deadline` expired.
    fn read_print(
        &self,
        id: &str,
        deadline: Deadline,
        role: Role,
        reservation: &mut Reservation,
    ) -> Result<Option<(std::process::ExitStatus, bounded::Capture)>> {
        Ok(match self.run(&["print", &target(id)], deadline, role, reservation)? {
            Finished::Exited { status, capture } => Some((status, capture)),
            Finished::Left(_) | Finished::Expired => None,
        })
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
///
/// `install`, `uninstall` and `kill` have a service's count and a task's (see `write`); [`needs`] takes the
/// larger, since a reservation is made before the kind is known. A task's `run` draws within `start`'s
/// count: a pass is the `debug`, the `kickstart -p`, the `procinfo` that names the launch's pid (a read,
/// with no reaper) and one `SIGCONT` (the resume, or the release of an older launch that took a hold), 4
/// calls and 3 reapers, and the first pass is preceded by the load (a kept read and wait, the `bootstrap`,
/// one more read), 7 calls and 4 reapers. Each pass after the first makes its own reservation, of a
/// start's, before its `debug`.
const SERVICE_INSTALL_REQUESTS: usize = 1;
/// A task's longest `install`, an update of an armed task whose job changes: the timer's `bootout`,
/// the job's, and both `bootstrap`s. A stray timer label's `bootout` is the timer's: a plan boots out
/// one timer label, so it adds nothing.
const TASK_INSTALL_REQUESTS: usize = 4;
const INSTALL_REQUESTS: usize = max(SERVICE_INSTALL_REQUESTS, TASK_INSTALL_REQUESTS);
const START_REQUESTS: usize = 5;
/// A service's `stop` is a `bootout`.
const SERVICE_STOP_REQUESTS: usize = 1;
/// A task's `stop` passes make no request: they signal through a hold.
const TASK_STOP_REQUESTS: usize = 0;
const STOP_REQUESTS: usize = max(SERVICE_STOP_REQUESTS, TASK_STOP_REQUESTS);
/// A task's `uninstall`: the timer's `bootout` and the job's.
const TASK_UNINSTALL_REQUESTS: usize = 2;
const UNINSTALL_REQUESTS: usize = max(SERVICE_STOP_REQUESTS, TASK_UNINSTALL_REQUESTS);
/// `kill` sends no `launchctl` request: the process is signalled through a hold, and launchd is only read.
const KILL_REQUESTS: usize = 0;
/// A task's `enable` makes its own reservation: the `bootout` of a timer label launchd holds with no
/// timer file, the `bootstrap` of the job, and the `bootstrap` of the timer.
const TASK_ENABLE_REQUESTS: usize = 3;
/// A task's `disable` makes its own: the timer's `bootout`.
const TASK_DISABLE_REQUESTS: usize = 1;

/// Most `launchctl` calls per verb, reads included: each draws its role's temp files from the same
/// reservation. A service's `install` adds two `is_loaded`s; `start` one `is_loaded` and five
/// `live_state`s. The `print` after an expired `kickstart` is not counted: the verb ends there, in
/// place of its last read. A wait ([`Launchctl::wait_print`]) is one call however many reads it
/// makes: one after each `bootout`. `start` has one more: its first read of the label and the wait
/// that may follow it share one set of files.
const SERVICE_INSTALL_CALLS: usize = SERVICE_INSTALL_REQUESTS + 2 + 1;
/// A task's longest `install`: the two reads of which labels launchd holds, the timer's `bootout` and
/// its wait, the read that checks the job is idle, the job's `bootout` and its wait, and both
/// `bootstrap`s. A `Create` is shorter: the same two reads, and the two `bootout`s with their waits.
const TASK_INSTALL_CALLS: usize = 2 + (1 + 1) + 1 + (1 + 1) + 2;
const INSTALL_CALLS: usize = max(SERVICE_INSTALL_CALLS, TASK_INSTALL_CALLS);
const START_CALLS: usize = START_REQUESTS + 6 + 2;
/// A service's `stop`: the `bootout` and its wait.
const SERVICE_STOP_CALLS: usize = SERVICE_STOP_REQUESTS + 1;
/// One pass of a task's `stop`, the longest: the read of the job, and the confirmation of the process held
/// (its `procinfo`, with the reads that go with it, one call's files). A pass that waits for a queued
/// start's pid is shorter: the read, and the wait.
const TASK_STOP_CALLS: usize = 2;
const STOP_CALLS: usize = max(SERVICE_STOP_CALLS, TASK_STOP_CALLS);
/// A task's `uninstall`: the two reads of which labels launchd holds, then both `bootout`s and their
/// waits.
const TASK_UNINSTALL_CALLS: usize = 2 + (1 + 1) + (1 + 1);
const UNINSTALL_CALLS: usize = max(SERVICE_STOP_CALLS, TASK_UNINSTALL_CALLS);
/// A service's `kill`: the confirmation's `procinfo` and the wait for launchd to stop showing the killed
/// pid, one call's files however many reads they make. Its other reads are of no reservation, as
/// `status`'s are not.
const SERVICE_KILL_CALLS: usize = 1;
/// A task's longest `kill`: the wait for the killed pid, the read that finds no process, and the wait
/// for the start launchd has accepted to fork.
const TASK_KILL_CALLS: usize = SERVICE_KILL_CALLS + 1 + 1;
const KILL_CALLS: usize = max(SERVICE_KILL_CALLS, TASK_KILL_CALLS);
/// A task's longest `enable`: the two reads of which labels launchd holds, both `bootstrap`s, and the
/// `bootout` of a stray timer label with its wait.
const TASK_ENABLE_CALLS: usize = 2 + 2 + (1 + 1);
/// A task's `disable`: the same two reads, the timer's `bootout` and its wait.
const TASK_DISABLE_CALLS: usize = 2 + (1 + 1);

const fn max(a: usize, b: usize) -> usize {
    if a > b { a } else { b }
}

/// The most `step` draws, as `(requests, calls)`: for `install`, `uninstall`, `stop` and `kill`, the larger of a
/// service's and a task's (a task's `stop` is counted per pass: a later pass reserves again). A draw
/// past it is a miscount, caught by the assertion in [`Reservation`] in every test that runs the verb.
///
/// `request_start_after_stop` is `start` under a budget that does not wait: it draws no more than
/// `start` does, and so is counted as it is. A task's `start` is a `bootstrap` and a `kickstart`, which
/// the service's longest path covers; a task's `request_start_after_stop` reads `print` first, which
/// makes its longest path 5 calls (the read, the load's 3, the `kickstart`) and 2 requests, inside it.
fn needs(step: Step) -> (usize, usize) {
    match step {
        Step::Install => (INSTALL_REQUESTS, INSTALL_CALLS),
        Step::Start | Step::RequestStartAfterStop => (START_REQUESTS, START_CALLS),
        Step::Stop => (STOP_REQUESTS, STOP_CALLS),
        Step::Uninstall => (UNINSTALL_REQUESTS, UNINSTALL_CALLS),
        Step::Kill => (KILL_REQUESTS, KILL_CALLS),
    }
}

/// What a verb that makes its own reservation (a task's `enable` and `disable`, which are handed none
/// that counts them) draws: `requests` reapers and the files of `calls` calls. All or none, so a failure
/// here sends nothing.
fn own_reservation(id: &Id, requests: usize, calls: usize) -> Result<Reservation> {
    Reservation::make(requests, calls * Role::COUNTED_FILES).map_err(|source| Error::Reservation {
        id: Some(id.to_string()),
        source,
    })
}

/// The reapers and temp files `steps` draw, summed: what `Launchd::reserve` makes.
fn counted(steps: &[Step]) -> (usize, usize) {
    let (requests, calls) = steps
        .iter()
        .map(|&step| needs(step))
        .fold((0, 0), |(requests, calls), (r, c)| (requests + r, calls + c));
    (requests, calls * Role::COUNTED_FILES)
}

/// What a `launchctl print` that ran says about the live state of the job of a daemon of `kind`: a
/// task's job is read by [`state::classify_task`], and one launchd does not hold is `Idle`.
fn classified(status: std::process::ExitStatus, capture: &bounded::Capture, kind: DaemonKind) -> (State, Option<u32>) {
    if !capture.complete {
        return (State::Unknown, None);
    }
    if !status.success() {
        // Not found is a clean "not running", what a staged-never-started or booted-out service
        // reports. Anything else, above all an unprivileged caller's permission failure querying
        // the system domain, must not read as "stopped": `list`/`status` work unelevated, and a
        // running daemon reported `Stopped` because the caller could not ask is a wrong answer.
        return if is_not_found(status.code()) {
            (
                match kind {
                    DaemonKind::Service => State::Stopped,
                    DaemonKind::Task => State::Idle,
                },
                None,
            )
        } else {
            (State::Unknown, None)
        };
    }
    let text = String::from_utf8_lossy(&capture.stdout);
    match kind {
        DaemonKind::Service => state::classify(&text),
        DaemonKind::Task => state::classify_task(&text),
    }
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
    type Definition = Plists;
    type Snapshot = Plist;
    type InstallScope = CreatedDirs;
    type Reservation = Reservation;
    type Run = run::Run;

    fn begin_install(
        &self,
        _spec: &DaemonSpec,
        _desired: &Plists,
        _reservation: &mut Reservation,
    ) -> Result<CreatedDirs> {
        Ok(CreatedDirs::default())
    }

    /// Makes every reaper and temp file `steps` can draw now, so that no step fails for want of one
    /// after an earlier step has sent something: `restart`'s start after its stop, `install
    /// --start`'s start after the install's `bootout`.
    fn reserve(&self, steps: &[Step], _budget: Budget) -> Result<Reservation> {
        let (reapers, files) = counted(steps);
        Reservation::make(reapers, files).map_err(|source| Error::Reservation { id: None, source })
    }

    fn render(&self, spec: &DaemonSpec) -> Result<Plists> {
        let account = resolve_account(&spec.common().user)?;
        generate::plists(spec, &account.identity())
    }

    /// `spec` as `render` would write it: the job from `spec`, and the timer from its own metadata
    /// where that decodes (else from `spec`), so that a job and a timer left by two different installs
    /// are each unmodified. A job beside no timer regenerates without one, so a missing timer is not
    /// drift. Each spec goes through `render`, which refuses what cannot be written, so a timer is
    /// never generated for a spec its job could not be.
    fn regenerate(&self, spec: &DaemonSpec, snapshot: &Plist) -> Result<Plists> {
        let mut plists = self.render(spec)?;
        plists.timer = match &snapshot.timer {
            None => None,
            Some(timer) => match timer.blob() {
                Ok(blob) => self.render(&blob.spec)?.timer,
                Err(_) => plists.timer,
            },
        };
        Ok(plists)
    }

    fn read(&self, id: &Id) -> Result<ReadOutcome<Plist>> {
        task::read_in(DIRS, id)
    }

    fn installed(&self, snapshot: &Plist) -> Result<InstalledDefinition<Plists>> {
        Ok(InstalledDefinition {
            definition: Plists {
                job: snapshot.job()?.text.clone(),
                timer: snapshot.timer.as_ref().map(|timer| timer.text.clone()),
            },
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
    ///
    /// A task, and a daemon that becomes or stops being one, is written as two plists (see `write`);
    /// what follows is a service's.
    fn write(
        &self,
        spec: &DaemonSpec,
        desired: &Plists,
        outcome: &Outcome,
        snapshot: Option<&Plist>,
        created: &mut CreatedDirs,
        reservation: &mut Reservation,
    ) -> Result<WriteOutcome> {
        if spec.kind() == DaemonKind::Task || snapshot.is_some_and(Plist::has_two_plists) {
            return self.write_task(
                DIRS,
                spec,
                desired,
                outcome,
                snapshot,
                write::Scope { created, reservation },
            );
        }
        // Resolved again: `render` kept only the plist, which names the account, and launchd
        // resolves that name at launch too.
        let account = resolve_account(&spec.common().user)?;
        let desired = desired.job.as_str();
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
                let target = staging_path(spec.common().id.as_str());
                prepare_parent_dirs(spec, &account, created)?;
                let staged = staged_tempfile(target.parent().expect("plist path always has a parent"), desired)?;
                let loaded = never_expires(self.launchctl.is_loaded(
                    spec.common().id.as_str(),
                    deadline,
                    reservation,
                )?);
                if loaded {
                    never_expires(
                        self.launchctl
                            .bootout(spec.common().id.as_str(), deadline, reservation)?,
                    );
                }
                persist_new_unraced(staged, &target, &spec.common().id, loaded)?;
                self.recheck_after_write(&spec.common().id, deadline, &target, desired, None, reservation)?;
            }
            Outcome::Update { .. } | Outcome::Stale { .. } => {
                let snapshot = snapshot.expect("an id `decide` chose to update was read as marked");
                let job = snapshot.job()?;
                let previous = job.text.as_str();
                // `Stale` is checked too: a release can change what the generator renders.
                let changes_job = refuse::changes_loaded_job(previous, desired);
                if changes_job
                    && never_expires(
                        self.launchctl
                            .is_loaded(spec.common().id.as_str(), deadline, reservation)?,
                    )
                {
                    return Err(refuse::loaded_refusal(&spec.common().id, DaemonKind::Service));
                }
                prepare_parent_dirs(spec, &account, created)?;
                let target = job.location.path.as_path();
                // `target` was captured by `read`, before `decide` ran. A concurrent `enable` or
                // `disable` may have moved it since, and writing here would put a plist in both
                // directories, which `locate` errors on; a concurrent `uninstall` would report
                // success and leave the write's plist installed. This check only narrows the race to
                // the window between it and `write_existing`'s rename onto `target`; it does not
                // close it.
                if !still_present(target)? {
                    return if changes_job {
                        // `is_loaded` has been called.
                        Err(refuse::changed_during_install(&spec.common().id, target))
                    } else {
                        Ok(WriteOutcome::Raced)
                    };
                }
                write_existing(target, desired)?;
                if changes_job {
                    self.recheck_after_write(
                        &spec.common().id,
                        deadline,
                        target,
                        desired,
                        Some(previous),
                        reservation,
                    )?;
                }
            }
            other => unreachable!("`flow::install` writes only `Create`, `Update` and `Stale`, got {other:?}"),
        }
        Ok(WriteOutcome::Done)
    }

    /// Whether the job's plist, read as `read` reads it, is `desired`'s: the plist is what names the
    /// directories. A timer's names none.
    fn standing(&self, spec: &DaemonSpec, desired: &Plists) -> Standing {
        plist_standing(self.read(&spec.common().id), desired)
    }

    fn live_status(&self, snapshot: &Plist) -> Result<Status> {
        if snapshot.is_task() {
            return self.task_status(snapshot);
        }
        // `status` and `list` have no budget of their own: unbounded.
        // Read as `UncountedQuery`: neither verb counted this read, so it draws on no reservation
        // and is handed an empty one.
        let (state, pid) = self.launchctl.read_live_state(
            snapshot.id.as_str(),
            Budget::Unbounded.start(),
            Role::UncountedQuery,
            &mut Reservation::default(),
        )?;
        Ok(Status::Service(DaemonStatus {
            state,
            pid,
            enabled: snapshot.job()?.location.enabled,
        }))
    }

    fn task_state(&self, snapshot: &Plist) -> Result<State> {
        self.task_run_state(snapshot)
    }

    fn scan(&self) -> Scan<Plist> {
        scan_in(DIRS)
    }

    /// A task is uninstalled as two plists (see `write`); a service as one.
    fn uninstall(&self, snapshot: Plist, reservation: &mut Reservation) -> Result<()> {
        if snapshot.has_two_plists() {
            return self.uninstall_task(snapshot, reservation);
        }
        let location = &snapshot.job()?.location;

        // Unbounded, like `install`'s: `uninstall` has no `--timeout`, and bounding this would turn a
        // slow-stopping job into a failed uninstall.
        never_expires(
            self.launchctl
                .bootout(snapshot.id.as_str(), Budget::Unbounded.start(), reservation)?,
        );
        fs::remove_file(&location.path).map_err(|e| Error::os_at("remove", &location.path, e))
    }

    /// A task is enabled as two plists, loaded (see `write`); a service's is the move of its plist.
    /// A task's `enable` makes its own reservation, since the one it is handed counts nothing.
    fn enable(&self, snapshot: Plist, _reservation: &mut Reservation) -> Result<()> {
        if snapshot.kind() == DaemonKind::Task {
            let mut own = own_reservation(&snapshot.id, TASK_ENABLE_REQUESTS, TASK_ENABLE_CALLS)?;
            return self.enable_task(DIRS, snapshot, &mut own);
        }
        let location = &snapshot.job()?.location;

        if location.enabled {
            return Ok(());
        }
        let dest = enabled_path(snapshot.id.as_str());
        move_no_clobber(&location.path, &dest)
    }

    /// Does not stop the job if it is loaded, as [`ServiceManager::disable`] requires: launchd
    /// holds a bootstrapped job by label, whichever directory its plist is in.
    ///
    /// A task's `disable` boots out the timer and moves both plists to staging, and leaves the job
    /// loaded (see `write`); it makes its own reservation, being handed none.
    ///
    /// [`ServiceManager::disable`]: crate::manager::ServiceManager::disable
    fn disable(&self, snapshot: Plist) -> Result<()> {
        if snapshot.kind() == DaemonKind::Task {
            let mut own = own_reservation(&snapshot.id, TASK_DISABLE_REQUESTS, TASK_DISABLE_CALLS)?;
            return self.disable_task(DIRS, snapshot, &mut own);
        }
        let location = &snapshot.job()?.location;

        if !location.enabled {
            return Ok(());
        }
        let dest = staging_path(snapshot.id.as_str());
        ensure_dir(Path::new(STAGING_DIR))?;
        move_no_clobber(&location.path, &dest)
    }

    fn start(&self, snapshot: Plist, budget: Budget, reservation: &mut Reservation) -> Result<()> {
        // A task's start requests a run and returns: no budget, and no wait for the pid.
        if snapshot.kind() == DaemonKind::Task {
            return self.start_task(snapshot, reservation);
        }
        // First, so the wait is counted from the verb's entry (see `verb_deadline`).
        let deadline = verb_deadline(budget);
        let path = snapshot.job()?.location.path.clone();
        let Plist { id, blob, .. } = snapshot;
        let id = &id;
        let blob = blob.map_err(Error::Kept)?;
        let DaemonSpec::Service(service) = &blob.spec else {
            unreachable!("a daemon whose metadata decodes as a task is started by `start_task`");
        };
        let timed_out = || budget::timed_out(id.as_str(), "running", budget);
        // Issuing the request is unbounded: the budget bounds waiting for launchd's answer, never
        // whether the request is sent, so a spent budget still reaches launchd.
        let issuing = Budget::Unbounded.start();

        // Waiting out a job being torn down is part of sending the request, like the corrective
        // cycle's: under no budget, so none leaves the old job loaded with `bootstrap` unsent.
        self.launchctl.load_job(id.as_str(), &path, reservation)?;
        // `kickstart` launches a job with no `KeepAlive`/`RunAtLoad` (`restart: never`); `bootstrap`
        // alone only loads it. Other policies are already started by `RunAtLoad`. `start` on a
        // running service must be `Ok` without restarting it, and launchd does not document that a
        // plain `kickstart` leaves a running instance alone, so check first.
        let before = self.launchctl.live_state(id.as_str(), issuing, reservation)?.0;
        if before != State::Running {
            // A later `running` ends the `kickstart` only after a clean `Stopped` read: see `Evidence`.
            let evidence = if before == State::Stopped {
                Evidence::QueuedOrRunning
            } else {
                Evidence::Queued
            };
            // Bounded by `deadline`: `-p` is the wait for the pid. An expiry leaves the `launchctl`
            // running unless launchd is shown to hold the start, so the request still goes out.
            match self
                .launchctl
                .kickstart(id.as_str(), budget, deadline, evidence, reservation)?
                .ok_or_else(timed_out)?
            {
                // launchd forked the job and named the pid: the confirmation.
                Kickstarted::Confirmed(_) => {}
                // `-p` returned no pid this code trusts. Not expected from launchd, and not
                // guessed at: one bounded read decides it.
                Kickstarted::NoPid if budget.waits() => {
                    let state = self.launchctl.live_state(id.as_str(), deadline, reservation)?.0;
                    if state != State::Running {
                        // A read the deadline cut short is `Unknown`: an expiry, not a job that
                        // failed to start.
                        return Err(if deadline.expired() {
                            timed_out()
                        } else {
                            not_started(id, state, service.restart == Restart::Always)
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
        // `install --start` returned `Ok`. A job tearing down here is another program's or the
        // operator's `launchctl bootout`.
        //
        // Only `KeepAlive: true` (`restart: always`) licenses this check: launchd keeps such a job
        // running whenever it is loaded. Under `on-failure` or `never` a job may have run and
        // exited, and demanding `Running` would fail a correct start.
        //
        // Skipped when the budget does not wait: the block asserts state, and a budget that waits
        // for none (`--timeout 0`, whose `Ok(())` means only that the request was accepted) sees
        // `spawn scheduled`, which `state::classify` folds into `Unknown`.
        if budget.waits()
            && service.restart == Restart::Always
            && self.launchctl.live_state(id.as_str(), issuing, reservation)?.0 != State::Running
        {
            // One corrective cycle, not a retry loop: tear the stale job down by label, wait until
            // it is gone so that the `bootstrap` does not meet it, and load ours.
            // Issued whatever is left of the budget, like the request above: when a stale job
            // answered to the label, this cycle is the request, and skipping it would report a start
            // never sent as one that may still arrive. The `bootout`'s wait for the old process is
            // part of sending it, so it runs under `issuing` on purpose and can outlast the budget,
            // for as long as launchd takes to remove the job: bounded by `deadline`, it could end
            // with the old job still loaded, `bootstrap` unsent and the daemon left unloaded.
            never_expires(self.launchctl.bootout(id.as_str(), issuing, reservation)?);
            never_expires(self.launchctl.bootstrap(&path, issuing, reservation)?);
            if self.launchctl.live_state(id.as_str(), issuing, reservation)?.0 != State::Running {
                // Queued only: see `Evidence`.
                self.launchctl
                    .kickstart(id.as_str(), budget, deadline, Evidence::Queued, reservation)?
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

    /// The plain request-only start.
    ///
    /// A service's stop before it waited until launchd no longer had the job (see
    /// [`Launchctl::bootout`]), even under [`Budget::Immediate`], so the instance it stopped cannot be
    /// what answers for the label here.
    ///
    /// A task's stop before it, under a budget that does not wait, only sent SIGTERM, and `kickstart` of
    /// a job that still has a process starts nothing and answers as a start does (measured: exit 0,
    /// nothing written). So one `print` reads whether the run is still there, and if it is, no start is
    /// issued ([`Error::StillRunning`]). If the run exits after the read the refusal is still true of
    /// what was read; if a fire starts a run in the gap, a fresh run is up. A read that failed or was not
    /// whole concludes nothing, so no start is issued either, and neither is a state with no pid that is
    /// neither idle nor a start launchd has accepted: the result is [`Error::StopUnconfirmed`], as
    /// `stop`'s is ([`verbs::quiet`] decides both), and indeterminate.
    fn request_start_after_stop(&self, snapshot: Plist, reservation: &mut Reservation) -> Result<()> {
        if snapshot.kind() == DaemonKind::Task {
            let print = self.launchctl.read_task_print(
                snapshot.id().as_str(),
                Budget::Unbounded.start(),
                Role::Query,
                reservation,
            )?;
            if let Some(pid) = print.pid {
                return Err(Error::StillRunning {
                    id: snapshot.id().to_string(),
                    pid,
                });
            }
            verbs::quiet(snapshot.id(), &print)?;
        }
        backend::Backend::start(self, snapshot, Budget::Immediate, reservation)
    }

    /// A task is never booted out: its `stop` sends SIGTERM to the run in flight and waits for it (see
    /// `stop_task`). A service is booted out, whatever the label holds.
    fn stop(&self, snapshot: Plist, budget: Budget, reservation: &mut Reservation) -> Result<()> {
        if snapshot.kind() == DaemonKind::Task {
            return self.stop_task(snapshot.id(), snapshot.stop_action(), budget, reservation);
        }
        // First, so the wait is counted from the verb's entry. A budget that does not wait gets an
        // unbounded deadline (see `verb_deadline`); under a bounded one an expiry leaves the
        // `bootout` running, so the request still goes out.
        let deadline = verb_deadline(budget);
        let id = snapshot.id();
        self.launchctl
            .bootout(id.as_str(), deadline, reservation)?
            .ok_or_else(|| budget::timed_out(id.as_str(), "stopped", budget))
    }

    /// SIGKILL to the job's process, for a service and a task alike, and the wait for launchd to stop
    /// showing it: a job with no process is [`Error::NotRunning`]. A task's `kill` that finds no process
    /// while launchd has accepted a start and not yet forked it (`spawn scheduled`, which `status` calls
    /// running) waits for the pid and then kills (see `kill_task`); a service has no such state. See
    /// [`crate::backend::kill`] and [`kill::JobKill`].
    fn kill(&self, snapshot: Plist, reservation: &mut Reservation) -> Result<()> {
        if snapshot.kind() == DaemonKind::Task {
            return self.kill_task(snapshot.id(), reservation);
        }
        let id = snapshot.id();
        let mut target = kill::JobKill::new(&self.launchctl, id.as_str(), reservation)?;
        match kill_service(&mut target, id.as_str()).map_err(|e| kill_failed(id.as_str(), e))? {
            KillOutcome::Ended => Ok(()),
            KillOutcome::NotRunning => Err(Error::NotRunning { id: id.to_string() }),
        }
    }

    /// Holds the job's next launch, starts it, and follows the process it names (see `run`).
    fn begin_run(
        &self,
        snapshot: Plist,
        reservation: &mut Reservation,
        notes: &mut dyn std::io::Write,
    ) -> Result<run::Run> {
        self.begin_task_run(snapshot, reservation, notes)
    }
}

// Enumeration =========================================================================================================

/// Every plist in `dirs` (each with whether it is the enabled one), as `read` would classify it.
///
/// A pass that stopped in one directory does not stop the other: scanning on only adds ids goetia
/// has established, and the aggregate in `reported` forbids concluding absence for anything either
/// pass missed. Both stopping gives two aggregates.
fn scan_in(dirs: Dirs) -> Scan<Plist> {
    let mut by_stem: BTreeMap<String, Vec<Location>> = BTreeMap::new();
    let mut incomplete = Vec::new();
    for (dir, enabled) in dirs {
        let pass = scan_plists(dir);
        for (stem, path) in pass.plists {
            by_stem.entry(stem).or_default().push(Location { path, enabled });
        }
        incomplete.extend(pass.incomplete);
    }

    // A stem `id_of_timer_stem` answers for is that id's timer, which no listing names as an id of
    // its own; any other stem is a job.
    let mut by_id: BTreeMap<String, Names> = BTreeMap::new();
    for (stem, locations) in by_stem {
        match generate::id_of_timer_stem(&stem) {
            Some(id) => by_id.entry(id.to_string()).or_default().timer = locations,
            None => by_id.entry(stem).or_default().job = locations,
        }
    }

    let mut entries = Vec::new();
    let mut reported = Vec::new();
    // A partial scan can narrow a positive claim but never supports absence: `reported` forbids
    // that. Untested: needs a mid-`readdir` fault on macOS.
    for (id, names) in by_id {
        // Each location is read, because `/Library/LaunchDaemons` holds every vendor's daemons: a
        // name found in both directories is goetia's only if one of the two carries the marker.
        match task::assemble(&id, names.job, || Ok(names.timer)) {
            // Undetermined, not absence or ownership: the failed read established neither. The
            // read is its reason; the `status` recovery is left out.
            Err(failures) => {
                let failures = failures.into_iter().map(|e| match e.into_undetermined() {
                    Ok((read, _recovery)) => *read,
                    Err(e) => e,
                });
                reported.push(Installed::Undetermined {
                    name: Some(id),
                    reason: Reason::failed(Error::both_all(failures).expect("a failed read has a failure")),
                });
            }
            // Gone since the scan: an uninstall completed. Absence is established, so nothing.
            Ok(Assembled::Read(ReadOutcome::Absent)) => {}
            Ok(Assembled::Read(found)) => {
                let entry = ScanEntry::of(&id, found, Plist::new);
                entries.push((id, entry));
            }
            Ok(Assembled::Unusable { reason, .. }) => reported.push(Installed::OursUnusable {
                name: id,
                reason: Reason::said(reason),
            }),
            Ok(Assembled::ForeignTimer { recovery }) => reported.push(Installed::OursUnusable {
                name: id,
                reason: Reason::said(recovery),
            }),
        }
    }
    // Last, so a pass that got some way in still reports what it named before what it could
    // not — the order `cli::support::partition_installed` imposes on the rendered output too.
    reported.extend(incomplete);
    Scan { entries, reported }
}

/// Where an id's two file names were found: its job's and its timer's.
#[derive(Default)]
struct Names {
    job: Vec<Location>,
    timer: Vec<Location>,
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
