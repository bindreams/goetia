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

use std::io::{Read as _, Write as _};
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::{fs, io};

use crate::backend::Identity;
use crate::backend::launchd::generate;
use crate::decide::{self, Outcome, Ownership};
use crate::error::{Error, Result};
use crate::manager::{Installed, ServiceManager, State, Status};
use crate::spec::{AccountId, DaemonSpec, Id, Restart, User};

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

/// What a stat established about a path — absence, presence, or neither.
///
/// The three are separated by what goetia *established*, never by what went
/// wrong. A `bool` can only say the first two, so it had to answer "I could
/// not look" as "nothing is there": a search-permission failure on either
/// directory made [`locate`] return `Ok(None)` and every verb report
/// [`Error::NotInstalled`] for a daemon that is right there.
///
/// [`Presence::Undetermined`] carries the [`io::Error`] itself rather than a
/// rendered reason, because [`undetermined`]'s `recovery` is keyed on the
/// errno and a `String` has already thrown that away.
#[derive(Debug)]
enum Presence {
    Absent,
    Present,
    Undetermined { source: io::Error },
}

/// Whether *anything* occupies `path` — any directory entry at all, not
/// just a regular file, and not a claim at all when the stat could not be
/// performed. `Path::is_file` is `false` for a directory, a
/// symlink, a socket, or any other non-regular entry, which would let
/// `locate` classify an occupied path as `Absent`; `write_new`'s
/// underlying `link`(2) refuses to create over *any* of those the same as
/// over a regular file (`EEXIST`), so that mismatch would send
/// `install` into `Create` -> `write_new` -> `Raced` -> re-`install` ->
/// the identical classification, forever. `symlink_metadata` (not
/// `metadata`, which follows symlinks and would report a *dangling* one
/// as absent) catches every case uniformly.
fn occupied(path: &Path) -> Presence {
    match fs::symlink_metadata(path) {
        Ok(_) => Presence::Present,
        Err(e) if e.kind() == io::ErrorKind::NotFound => Presence::Absent,
        // `EACCES` on a parent directory, `ENOTDIR` on a parent component,
        // `EIO`: the stat never completed, so absence is not one of the
        // things it established.
        Err(source) => Presence::Undetermined { source },
    }
}

/// What the pair of stats established about an id, before a caller collapses it into a `Result`.
///
/// [`locate`]'s own answer is this, and `list` needs the two `Err` cases as *entries* rather than
/// as an error that would discard every other id in the same listing.
enum Located {
    /// Neither directory holds a plist for this id — the only answer that establishes the id is
    /// absent, as opposed to one path being vacant.
    Nowhere,
    At(Location),
    /// Both directories hold one. See [`locate`] for how that state is reachable at all.
    Both,
    /// A stat did not complete, so the pair says nothing: neither location nor absence.
    Unsettled {
        path: PathBuf,
        source: io::Error,
    },
}

/// **Ask the id, not a path.** The one place this backend answers "what is at this id", and the
/// re-ask every caller that would otherwise conclude absence from a single vacant path owes it:
/// `enable`/`disable` *move* the plist between the two directories with [`move_no_clobber`]
/// (`hard_link` then `remove_file`), which keeps it linked at one of them at every instant, so a
/// path that is vacant now is a completed move exactly as readily as a completed uninstall. Only
/// [`Located::Nowhere`] is absence.
///
/// Two stats are not enough to conclude it, and that is why the staging one is repeated. The pair
/// is sequential, not simultaneous: a `disable` landing between them leaves staging vacant when
/// staging was read (the plist was still enabled) *and* enabled vacant when enabled was read (the
/// plist is now staged), and the id is installed the whole time. The third stat is what rules that
/// out — the move's `hard_link` precedes its `remove_file`, so a plist that left `ENABLED_DIR`
/// before the second stat was already in `STAGING_DIR` before it, and is still there at the third
/// unless a *second* move has since taken it back.
fn located(id: &str) -> Located {
    let staging = staging_path(id);
    let enabled = enabled_path(id);
    match (occupied(&staging), occupied(&enabled)) {
        // One unresolved probe leaves the *pair* unresolved: the id's
        // location is a fact about both directories at once, and the
        // remaining three answers — absent, staged, enabled — each claim
        // something about the directory that was never read.
        (Presence::Undetermined { source }, _) => Located::Unsettled { path: staging, source },
        (_, Presence::Undetermined { source }) => Located::Unsettled { path: enabled, source },
        (Presence::Present, Presence::Absent) => Located::At(Location {
            path: staging,
            enabled: false,
        }),
        (Presence::Absent, Presence::Present) => Located::At(Location {
            path: enabled,
            enabled: true,
        }),
        (Presence::Present, Presence::Present) => Located::Both,
        (Presence::Absent, Presence::Absent) => match occupied(&staging) {
            Presence::Absent => Located::Nowhere,
            Presence::Present => Located::At(Location {
                path: staging,
                enabled: false,
            }),
            Presence::Undetermined { source } => Located::Unsettled { path: staging, source },
        },
    }
}

fn locate(id: &str) -> Result<Option<Location>> {
    match located(id) {
        Located::Nowhere => Ok(None),
        Located::At(location) => Ok(Some(location)),
        Located::Both => Err(both_dirs_error(id)),
        Located::Unsettled { path, source } => Err(undetermined(id, "stat", &path, &source)),
    }
}

/// The refusal for a plist at both paths, so [`locate`] and the same discovery reached through a
/// re-ask cannot word one state two ways.
fn both_dirs_error(id: &str) -> Error {
    Error::Other(format!(
        "daemon `{id}` has a plist in both {STAGING_DIR} and {ENABLED_DIR}; remove one by hand \
         (they should never both exist) before retrying"
    ))
}

// Reading a plist =====================================================================================================

/// This backend's single [`Error::Undetermined`] constructor — the launchd
/// twin of `backend::systemd::manager::discover::undetermined`, and worded
/// identically, since the two describe one condition on two platforms.
///
/// [`Error::Undetermined`], never [`Error::Io`]: `Io` reaches
/// `cli::report::status_error`'s catch-all as `Kind::Unreadable`, which
/// *asserts* that goetia owns the id — the one thing a read that never
/// completed cannot establish. `/Library/LaunchDaemons` holds every vendor's
/// daemons, so that claim is routinely about a stranger's service.
///
/// Every failure but `NotFound` lands here, not `PermissionDenied` alone: an
/// `EIO` leaves goetia exactly as ignorant of the id as an `EACCES` does.
/// What the errno does choose is `recovery` — re-running elevated is advice
/// only a permission boundary earns, and offering it for a failing disk
/// sends the user somewhere useless.
fn undetermined(id: &str, op: &str, path: &Path, source: &io::Error) -> Error {
    undetermined_reason(
        id,
        read_detail(op, path, source),
        source.kind() == io::ErrorKind::PermissionDenied,
    )
}

/// The same error over a `reason` already rendered — for the one condition that has no errno of its
/// own to render, a path occupied when goetia stat'd it and vacant when it read it (see
/// [`vacated_detail`]).
fn undetermined_reason(id: &str, reason: String, access_denied: bool) -> Error {
    let recovery = if access_denied {
        "re-run as root (or under sudo): that read is what tells goetia whether anything is \
         installed at this id"
    } else {
        "resolve that failure and re-run: that read is what tells goetia whether anything is \
         installed at this id"
    };
    Error::Undetermined {
        id: id.to_string(),
        reason,
        recovery: recovery.to_string(),
    }
}

/// What did not complete, as facts — which operation, which path, which
/// failure — with no claim about the id attached. Shared by [`undetermined`]
/// and by `list`'s [`Installed::Undetermined`] entry, so the error and the
/// entry cannot describe one failure differently.
fn read_detail(op: &str, path: &Path, source: &dyn std::fmt::Display) -> String {
    format!("failed to {op} {path}: {source}", path = path.display())
}

/// The same, for the one failure with no errno behind it: a re-ask ([`located`]) found the plist at
/// `path` and the read that immediately followed found nothing there. A second move, or an
/// uninstall, landed between the two syscalls — and the pair establishes neither, which is the
/// whole reason it is reported rather than resolved by asking a third time. Asking again is only
/// worth anything against a *finite* number of moves, and nothing on this host promises that.
fn vacated_detail(path: &Path) -> String {
    format!(
        "{path} was occupied when goetia stat'd it and vacant when it read it",
        path = path.display()
    )
}

/// The magic every binary property list begins with. `plutil -convert
/// binary1` and `defaults write` emit that format by default, so a
/// `/Library/LaunchDaemons` full of them is the normal state of a macOS
/// host, not a defect.
const BINARY_PLIST_MAGIC: &[u8] = b"bplist00";

/// What a plist's bytes turned out to be, once they were obtained.
///
/// Obtaining bytes and understanding them are different questions, and this
/// enum answers only the second: reaching it at all means the file was opened,
/// `fstat`'d as regular and read to the end, so presence is established
/// whichever variant comes out. That is why neither of them is
/// [`Error::Undetermined`], which means presence was *not* established.
enum Classified {
    /// Valid UTF-8 — the only form `generate::plist` ever writes, and the
    /// only one `generate::extract`'s `goetia:begin` comment can be looked
    /// for in.
    Text(String),
    /// Bytes that are not something goetia wrote, as a **positive**
    /// identification rather than an inference from a failure: `generate::plist`
    /// emits UTF-8 XML and nothing else, so a binary plist and a UTF-16 one are
    /// each as certainly not goetia's as an XML plist carrying no marker — and
    /// they get that same answer, foreign and omitted.
    ///
    /// One negative, one answer. Splitting it — `bplist00` foreign, every other
    /// non-UTF-8 byte undetermined — gave a macOS host a permanent, unclearable
    /// `undetermined` entry and a permanent exit `4` for a vendor's service that
    /// is in no way goetia's business, over a plist saved as UTF-16 (legal, and
    /// its `FF FE` BOM is not UTF-8) or carrying one Latin-1 byte. The
    /// `bplist00` arm already refused that outcome; there was never a reason the
    /// other arm should accept it.
    ///
    /// The accepted cost: a plist goetia *did* write, corrupted after the fact,
    /// now reads as a stranger's rather than as ours-but-broken. Ownership
    /// cannot be established without reading the marker, and the marker is in
    /// the bytes that would not decode.
    NotOurs,
}

/// What trying to obtain a plist's bytes ran into.
enum Obtained {
    Bytes(Vec<u8>),
    /// Nothing is at the path.
    Absent,
    /// Something is at the path and it is positively **not** a plist: a
    /// FIFO, a directory, a socket, a device node. A different claim from
    /// [`Obtained::Failed`] — not "goetia could not read this", but "goetia
    /// read nothing because there is nothing here to read" — and so it is
    /// treated exactly like a binary plist: foreign, omitted.
    NonRegular,
    /// The read did not complete.
    Failed(io::Error),
}

/// Classify `path` before opening it, and again on the descriptor actually
/// opened.
///
/// `open(2)` on a FIFO with `O_RDONLY` blocks until a writer arrives, and
/// `list` reads every `*.plist` name in `/Library/LaunchDaemons` — so one
/// `mkfifo` there would otherwise wedge the listing for the whole host, and
/// wedge `install`/`status` for that id forever. The step-1 `fs::metadata`
/// never opens anything, so it cannot block, and it keeps a device node from
/// being opened at all.
///
/// [`fs::metadata`], which **follows symlinks**, deliberately unlike
/// `super::super::systemd::manager::discover::classify_and_read`'s
/// `O_PATH | O_NOFOLLOW` view of a unit fragment. The two differ because the
/// platforms do: `systemctl mask` points a fragment at `/dev/null`, so on
/// systemd the symlink itself is the meaningful artifact and reading through
/// it would look identical to "nothing here". launchd has no masking and no
/// equivalent — a symlink at a plist path is just an indirection to the
/// plist — and following it is what this backend has always done. Do not
/// unify the two.
///
/// Step 2 re-checks the *descriptor*, not the name, so the verdict is about
/// the exact object whose bytes are read: the file `fs::metadata` classified
/// need not be the file a later `open` resolves the same name to. That
/// leaves no residual for this hazard, and needs no `(st_dev, st_ino)`
/// comparison against step 1 to say so — an `fstat` on the open descriptor
/// is a stronger statement than agreement between two name lookups. What it
/// does not rule out is reading a *different regular file* than step 1
/// stat'd, which is the same benign content race a plain `fs::read` has and
/// no verdict here depends on.
///
/// `O_NONBLOCK` covers the window between the two steps: a FIFO swapped in
/// after step 1 cannot block the open, so step 2 gets to reject it.
fn obtain(path: &Path) -> Obtained {
    use std::os::unix::fs::OpenOptionsExt as _;

    match fs::metadata(path) {
        Ok(meta) if !meta.is_file() => return Obtained::NonRegular,
        Ok(_) => {}
        // `metadata` follows, so it reports a *dangling* symlink as absent —
        // but `link`(2) still refuses to create over one (`EEXIST`), and
        // `locate`'s `symlink_metadata` sees it. Left as `Absent`, the two
        // classifiers disagree permanently: every verb answers "not
        // installed" while no write can ever succeed, an unclearable dead
        // end whose only message says nothing is there. The link itself is
        // positively not a plist, which is the same presence fact a FIFO
        // is, so it gets the same answer.
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
        // Gone between the two steps: the same uninstall race step 1
        // tolerates, observed one syscall later. A dangling symlink cannot
        // reach here — step 1 already classified it `NonRegular`.
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

/// The one place bytes obtained from a plist become a verdict, so `list` and
/// [`read_artifact`] cannot classify the same file differently.
///
/// The magic is checked ahead of the UTF-8 decode rather than left to it: a
/// small binary plist can be valid UTF-8 (its trailer is mostly NUL bytes), and
/// identifying it by the eight bytes Apple's format begins with does not depend
/// on which of its object markers happen to fall outside ASCII.
fn classify(bytes: Vec<u8>) -> Classified {
    if bytes.starts_with(BINARY_PLIST_MAGIC) {
        return Classified::NotOurs;
    }
    match String::from_utf8(bytes) {
        Ok(text) => Classified::Text(text),
        Err(_) => Classified::NotOurs,
    }
}

/// Read the artifact `locate` found for `id`, for the three callers that go
/// on to look for a marker in it (`discover`, `located_and_ours`, `status`).
///
/// A vacant path is *re-asked*, never read as absence: the plist was there when `locate` stat'd it,
/// and an `enable`/`disable` moving it to the other directory in between leaves this exact path
/// vacant with the id still installed. [`located`] stats both, so it is what separates a move from
/// an uninstall — and [`Error::NotInstalled`] is claimed only for its `Nowhere`. Every read that
/// did not complete is [`undetermined`].
///
/// Bytes that are not goetia's ([`Classified::NotOurs`]), and anything that is
/// not a regular file at all, read as the empty string. That is a statement,
/// not a fallback: the marker lives in an XML comment, a binary plist has no
/// comments, bytes that are not UTF-8 are not the UTF-8 XML `generate::plist`
/// writes, a FIFO is not a plist in the first place — so "text carrying no
/// marker" is precisely what has been established in each case, and every
/// caller's `extract` turns it into the `Foreign` refusal that is the right
/// answer for all of them. It is also what keeps `install` over one of these
/// from erroring: `decide` reaches `Outcome::RefuseForeign`, which names the
/// remedy, instead of a bare `Err`.
fn read_artifact(path: &Path, id: &str) -> Result<String> {
    match obtain(path) {
        Obtained::Bytes(bytes) => Ok(marker_text(bytes)),
        Obtained::Absent => reread_after_vacancy(id),
        Obtained::NonRegular => Ok(String::new()),
        Obtained::Failed(e) => Err(undetermined(id, "read", path, &e)),
    }
}

/// The re-ask [`read_artifact`] owes a vacant path, and the error half of the one `list` performs:
/// ask the id over both directories, then read whatever that found, once.
fn reread_after_vacancy(id: &str) -> Result<String> {
    match located(id) {
        Located::Nowhere => Err(Error::NotInstalled { id: id.to_string() }),
        Located::Both => Err(both_dirs_error(id)),
        Located::Unsettled { path, source } => Err(undetermined(id, "stat", &path, &source)),
        Located::At(location) => match obtain(&location.path) {
            Obtained::Bytes(bytes) => Ok(marker_text(bytes)),
            Obtained::NonRegular => Ok(String::new()),
            Obtained::Failed(e) => Err(undetermined(id, "read", &location.path, &e)),
            Obtained::Absent => Err(undetermined_reason(id, vacated_detail(&location.path), false)),
        },
    }
}

/// Obtained bytes as the text every caller looks for a marker in: their own if they decode,
/// otherwise the empty string, which carries no marker and so is the [`Classified::NotOurs`]
/// verdict stated in the vocabulary `extract` speaks.
fn marker_text(bytes: Vec<u8>) -> String {
    match classify(bytes) {
        Classified::Text(text) => text,
        Classified::NotOurs => String::new(),
    }
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
    let text = read_artifact(&location.path, id)?;
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
    let text = read_artifact(&location.path, id.as_str())?;
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
            &decide::Overlay::default(),
        );

        match &outcome {
            Outcome::Create => {
                prepare_parent_dirs(spec, &account)?;
                let target = staging_path(spec.id.as_str());
                match write_new(&target, &desired)? {
                    WriteNew::Written => {
                        // A job may already answer to this label even though
                        // no artifact of ours existed a moment ago — a
                        // predecessor installed by hand, or one an external
                        // actor boot'ed out that has not finished tearing
                        // down. It cannot be from the plist we just created,
                        // so it is stale by construction.
                        //
                        // Leaving it loaded is what broke a real migration:
                        // `start` then saw the label loaded *and running*,
                        // concluded there was nothing to do, and returned
                        // `Ok` while the daemon never ran. Every label-scoped
                        // check has that blind spot — `is_loaded` and
                        // `query_live_state` cannot tell whose job answers
                        // to a label — so the fix has to be here, at the one
                        // moment we know the artifact is new.
                        if is_loaded(spec.id.as_str())? {
                            bootout(spec.id.as_str())?;
                        }
                    }
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
                match occupied(&target) {
                    Presence::Present => {}
                    Presence::Absent => return self.install(spec, force),
                    // The re-check established neither the vanish that would
                    // license re-deriving nor the presence that would
                    // license the write. [`Error::Io`], not [`undetermined`]:
                    // `discover` has already opened, read and decoded this
                    // artifact, so "is anything installed at this id" is not
                    // the question this stat left open.
                    Presence::Undetermined { source } => return Err(Error::Io { path: target, source }),
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
            &decide::Overlay::default(),
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
        let text = read_artifact(&location.path, id.as_str())?;
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
        let scan = scan_ids();

        let mut out = Vec::new();
        for id in &scan.ids {
            // The scan named this id; where it is, and whether it still is, is asked *now* — the
            // same `located` + `obtain` pair `status` goes through, so the two verbs cannot
            // describe one host differently, and no answer here rests on a path a pass recorded
            // before the other directory was even read.
            match located(id) {
                // Both directories are vacant now, which is what makes this an absent id rather
                // than a vacated path. Reporting it would let a benign concurrent uninstall raise
                // `list`'s host-wide exit code.
                Located::Nowhere => continue,
                // The same anomaly `locate` refuses for every other verb. `OursUnreadable`, not
                // `Undetermined`: presence is established at both paths, and it is which of them
                // enrolls the id at boot that this listing cannot state.
                Located::Both => out.push(Installed::OursUnreadable {
                    name: id.clone(),
                    reason: format!("present in both {STAGING_DIR} and {ENABLED_DIR}"),
                }),
                Located::Unsettled { path, source } => out.push(Installed::Undetermined {
                    name: Some(id.clone()),
                    reason: read_detail("stat", &path, &source),
                }),
                Located::At(location) => match obtain(&location.path) {
                    Obtained::Bytes(bytes) => out.extend(entry_from_bytes(id, bytes, location.enabled)),
                    // A FIFO, a directory, a socket: nothing goetia ever wrote, and established as
                    // such rather than merely unread — so it is omitted like any other foreign
                    // entry, not reported.
                    Obtained::NonRegular => continue,
                    // A failed read leaves zero evidence of whether this plist ever carried a
                    // Goetia marker at all — unlike a decode failure in `entry_from_bytes`, which
                    // only happens *after* confirming the marker is present. Claiming
                    // `OursUnreadable` without that evidence would misreport an unreadable
                    // *foreign* plist (routine on `/Library/LaunchDaemons`, which holds every
                    // vendor's daemons, not just Goetia's) as one of ours. Omitting it claims the
                    // other thing goetia does not know — that nothing is there.
                    Obtained::Failed(e) => out.push(Installed::Undetermined {
                        name: Some(id.clone()),
                        reason: read_detail("read", &location.path, &e),
                    }),
                    Obtained::Absent => out.push(Installed::Undetermined {
                        name: Some(id.clone()),
                        reason: vacated_detail(&location.path),
                    }),
                },
            }
        }
        // Last, so a pass that got some way in still reports what it named before what it could
        // not — the order `cli::support::partition_installed` imposes on the rendered output too.
        out.extend(scan.incomplete);
        Ok(out)
    }
}

// Classification ======================================================================================================

/// One id's entry, from bytes already obtained for it — `None` where the answer is to omit it.
/// Shared by every arm of `list` that has bytes, so a plist read from the staging directory and one
/// read from `/Library/LaunchDaemons` cannot be classified differently.
fn entry_from_bytes(id: &str, bytes: Vec<u8>, enabled: bool) -> Option<Installed> {
    // Foreign by positive identification, so omitted exactly as an unmarked XML plist is — see
    // `Classified::NotOurs`.
    let text = marker_text(bytes);
    match generate::extract(&text) {
        Ok(None) => None, // foreign: not Goetia-managed, omitted per the trait doc comment
        Ok(Some(blob)) => {
            let (state, pid) = query_live_state(id);
            Some(Installed::Ours {
                spec: blob.spec,
                state,
                pid,
                enabled,
            })
        }
        Err(e) => Some(Installed::OursUnreadable {
            name: id.to_string(),
            reason: e.to_string(),
        }),
    }
}

// Enumeration =========================================================================================================

/// Every id `LaunchdManager::list` has to answer for, and the entries standing for whatever the
/// passes that produced it never reached.
///
/// Names only. Where each id *is* — and whether it still is — is [`located`]'s answer at the moment
/// the id is classified, never the directory a pass happened to find it in: the passes are
/// sequential, so the pair of them is not a simultaneous observation of the two directories and
/// cannot be read as one.
///
/// # Why the staging directory is passed twice
///
/// A `disable` moves the plist `ENABLED_DIR` -> `STAGING_DIR`. One that completes *between* the
/// staging pass and the enabled pass is in neither of them — staged too late for the first,
/// unlinked too early for the second — and an id no pass names is an id `list` omits, which is
/// `list` saying it is not installed. [`move_no_clobber`] leaves it linked at one of the two at
/// every instant, so a third pass over the (small, goetia-only) staging directory after the enabled
/// one names it: a move in either direction is now seen by at least one pass whenever it happens.
/// What is left is two *complete* moves straddling the three passes, which is the same residual the
/// per-id re-ask carries and is stated there.
#[derive(Debug)]
struct IdScan {
    ids: std::collections::BTreeSet<String>,
    incomplete: Vec<Installed>,
}

fn scan_ids() -> IdScan {
    // A pass that stopped in one directory does not stop the other. They are two separate
    // locations, and what the staging directory refused to yield establishes nothing about
    // `/Library/LaunchDaemons` — so scanning on can only add ids goetia has actually established,
    // while the aggregate below already forbids concluding absence for anything any pass missed.
    let staging = scan_plists(STAGING_DIR);
    let enabled = scan_plists(ENABLED_DIR);
    let restaged = scan_plists(STAGING_DIR);

    let mut ids = std::collections::BTreeSet::new();
    for scan in [&staging, &enabled, &restaged] {
        ids.extend(scan.ids.iter().cloned());
    }
    IdScan {
        ids,
        // One aggregate per directory, not per pass: the first staging aggregate already says that
        // directory was not enumerated to the end, and a second entry repeating it would double the
        // count a reader takes for "how much of this host went unread".
        incomplete: [staging.incomplete.or(restaged.incomplete), enabled.incomplete]
            .into_iter()
            .flatten()
            .collect(),
    }
}

/// What one pass over one of `list`'s two directories found: the ids the `<id>.plist` files it
/// reached name, and — when the pass stopped early — the entry standing for whatever it never did.
#[derive(Debug)]
struct PlistScan {
    ids: Vec<String>,
    incomplete: Option<Installed>,
}

/// Enumerate `dir`. A pass that cannot start, or cannot finish, is *reported* rather than
/// propagated: an `Err` out of `list` throws away everything the same call already classified —
/// including, here, a whole other directory that was scanned successfully — and reaches the CLI as
/// an empty document on exit `1`, which is `list` saying the host has no daemons.
///
/// `dir` not existing is left as it was, a silent skip: the staging directory does not exist until
/// the first `install` ever creates it, and that establishes that nothing is staged rather than
/// leaving it unread.
fn scan_plists(dir: &str) -> PlistScan {
    match fs::read_dir(dir) {
        Ok(entries) => collect_plists(dir, entries.map(|entry| entry.map(|entry| entry.path()))),
        Err(e) if e.kind() == io::ErrorKind::NotFound => PlistScan {
            ids: Vec::new(),
            incomplete: None,
        },
        Err(e) => PlistScan {
            ids: Vec::new(),
            incomplete: Some(Installed::scan_incomplete(
                dir,
                &format!("failed to read directory: {e}"),
            )),
        },
    }
}

/// The pass itself, over an iterator of paths rather than [`fs::read_dir`] directly — which is what
/// makes the mid-pass fault reachable from a test, since no `readdir` fails on request. That fault
/// is the half that matters: everything already collected comes back alongside the aggregate,
/// rather than being discarded because a *later* dirent could not be read.
fn collect_plists(dir: &str, entries: impl Iterator<Item = io::Result<PathBuf>>) -> PlistScan {
    let mut ids = Vec::new();
    for entry in entries {
        let path = match entry {
            Ok(path) => path,
            Err(e) => {
                return PlistScan {
                    ids,
                    incomplete: Some(Installed::scan_incomplete(
                        dir,
                        &format!("failed to read a directory entry: {e}"),
                    )),
                };
            }
        };
        if path.extension().and_then(|e| e.to_str()) != Some("plist") {
            continue;
        }
        let Some(id) = path.file_stem().and_then(|s| s.to_str()).map(str::to_string) else {
            continue;
        };
        ids.push(id);
    }
    PlistScan { ids, incomplete: None }
}

#[cfg(test)]
#[path = "manager_tests.rs"]
mod manager_tests;
