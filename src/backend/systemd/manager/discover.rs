//! Classifying what's currently at an id: reading the fragment, folding in any drop-in drift, and
//! turning that into the [`Ownership`] `decide` needs.
//!
//! # What counts as this id's artifact
//!
//! `<id>.service` and `<id>.service.d`, and nothing else. The drop-in directory counts under *every*
//! root of systemd's system unit search path ([`DROPIN_SEARCH_DIRS`]) — one under
//! `/etc/systemd/system.control`, where `systemctl set-property` writes, is as much this id's as one
//! under `/etc/systemd/system`.
//!
//! `systemd.unit(5)` reads a strict superset. For `my-daemon.service` it also reads the
//! dash-truncated `my-.service.d`, and for every service unit it reads the top-level `service.d`.
//! Those are deliberately **not** scanned, and the omission is a stated limitation rather than an
//! oversight: they are named for a family of units rather than for this id, they exist whether or
//! not this id does, and folding them into the text `decide` compares makes `Outcome::Conflict` —
//! whose published meaning is "an installed artifact was modified outside goetia; re-run with
//! `--force` to overwrite" — both untrue and unfixable. A host-wide policy modified nothing, and
//! `--force` rewrites the fragment without touching a directory that governs unrelated units, so the
//! operator is told to force, forces, and is told to force again.
//!
//! The cost is a real false negative: an administrator deliberately aiming `my-.service.d` at
//! goetia's `my-daemon` is not reported. Closing it takes a different question than drift detection
//! asks — "what will actually run here", resolved the way systemd resolves it and reported with no
//! notion of an artifact goetia owns. That is a `doctor`-style check goetia does not have.

use std::ffi::OsStr;
use std::fs;
use std::io;
use std::io::Read as _;
use std::os::fd::OwnedFd;
use std::path::{Path, PathBuf};

use nix::errno::Errno;
use nix::fcntl::{OFlag, openat};
use nix::sys::stat::Mode;

use crate::backend::systemd::generate;
use crate::decide::{Overlay, Ownership};
use crate::error::{Error, Result};

use super::{UNIT_DIR, identity_for, io_err, unit_path};

// ReadFailure =========================================================================================================

/// A read that did not complete, carried as facts — which operation, which path, which errno — with
/// no error class attached to it yet.
///
/// One failure, two meanings, and only the caller knows which applies. On the path where no fragment
/// was found, a failed read leaves goetia unable to say whether *anything* is installed
/// ([`Error::Undetermined`]). On the path where the fragment has already been opened, read and had
/// its `[X-Goetia]` marker decoded, the same failure leaves an id goetia demonstrably owns merely
/// impossible to report on ([`Ownership::OursUnreadable`]). A reader that picked one class for all
/// its callers would put that claim on the other's id — which is exactly the rule
/// [`Error::Undetermined`] exists to enforce: choose by what was established, never by what failed.
#[derive(Debug)]
pub(super) struct ReadFailure {
    op: &'static str,
    path: PathBuf,
    source: io::Error,
}

type ReadResult<T> = std::result::Result<T, ReadFailure>;

impl ReadFailure {
    fn new(op: &'static str, path: &Path, source: io::Error) -> Self {
        Self {
            op,
            path: path.to_path_buf(),
            source,
        }
    }

    /// The operation and the path it failed on, with no claim about the id attached.
    pub(super) fn detail(&self) -> String {
        format!(
            "failed to {op} {path}: {source}",
            op = self.op,
            path = self.path.display(),
            source = self.source
        )
    }

    /// The absence-path reading — see [`undetermined`].
    fn undetermined(&self, id: &str) -> Error {
        undetermined(id, self.op, &self.path, &self.source)
    }

    /// The reading for a caller that already knows goetia owns the id, so "unreadable" is a true
    /// statement about it: `super::write::quarantine_if_still_ours`, which reaches this only after
    /// `discover`/`require_installed` classified the fragment as ours.
    pub(super) fn into_io_error(self) -> Error {
        io_err(self.op, &self.path, self.source)
    }
}

// RawState / raw_state / classify_and_read ============================================================================

/// What's at a path, in the only two terms that settle ownership: bytes goetia can look for its
/// marker in, or a positive identification that there is no point looking.
pub(super) enum RawState {
    Absent,
    /// Present, and positively **not** an artifact goetia wrote. Two ways to establish that, one
    /// class because it is one claim — and `Ownership::Foreign` is the answer to both:
    ///
    /// - Not a regular file: a symlink (`systemctl mask` points the fragment at `/dev/null`), a
    ///   FIFO, a device node, a directory — obligation 2. Contents are never read; reading through
    ///   a masked unit's target would look identical to "nothing here".
    /// - A regular file whose bytes are not UTF-8. `generate::unit` writes UTF-8 ini and nothing
    ///   else, so bytes that will not decode are a positive identification of a format goetia never
    ///   emits — the same claim launchd's `Classified::NotOurs` makes about a plist, and it gets the
    ///   same answer.
    ///
    /// The accepted cost of the second: a fragment goetia *did* write, corrupted after the fact,
    /// now reads as a stranger's rather than as ours-but-broken. Ownership cannot be established
    /// without reading the marker, and the marker is in the bytes that would not decode. The
    /// alternative is a permanent, unclearable `undetermined` entry — and a host-wide exit `4` —
    /// for every non-UTF-8 unit file on the host that was never goetia's business.
    NotOurs,
    Regular(String),
}

/// Classify `path`, and read it only once it is known to be a regular file. Two opens of one final
/// component *under one directory descriptor*, each answered by `fstat` on the descriptor that open
/// returned rather than by a second name lookup — a separate `lstat` is its own TOCTOU gap, since
/// the file it classified need not be the file a later open resolves the same name to.
///
/// The directory is opened once and both steps `openat` through it. That is not a micro-optimisation
/// but what makes step 2's errnos readable: a name with no `/` in it, resolved under a descriptor,
/// cannot fail for anything above the artifact itself, so `ELOOP` there means "this component is a
/// symlink" and nothing else.
///
/// What that is worth, and what it costs, stated exactly rather than as a general claim about
/// ambiguity. A parent that is *already* a symlink loop is refused by step 1's own open — measured,
/// and true of the pathname-twice shape as well, since that open resolves the whole pathname too —
/// so step 2's map is never reached and no unit is misreported as foreign either way. The case this
/// closes is the narrow one: a parent turned into a loop *between* the two opens, where a second
/// resolution of the pathname would answer `ELOOP` about something above the artifact and the map
/// would read it as the artifact being a symlink. The cost is the mirror image — a parent
/// *replaced* between the two opens is resolved in the old inode, so step 2 reads what was there
/// rather than observing the swap. That is a file that really was at this path, the same benign
/// content race [`read_regular`] documents for the artifact itself, and it is untested: forcing it
/// takes a rename landing between two adjacent syscalls.
///
/// Step 1 opens `O_PATH | O_NOFOLLOW`. That needs no read permission, never blocks, and is the
/// documented case that yields a descriptor for the *symlink itself*, so a masked unit (`systemctl
/// mask` points the fragment at `/dev/null`), a FIFO, a device node and a `.d` directory are all
/// classified [`RawState::NotOurs`] without ever being opened for reading — obligation 2. It is
/// also what keeps `list` answerable at all: opening for reading first means `open(FIFO, O_RDONLY)`
/// blocks until a writer arrives, and one `mkfifo x.service` would wedge the listing for every
/// daemon on the host.
///
/// Step 2 ([`read_regular`]) re-opens for reading and settles what it got from `fstat` on *that*
/// descriptor, so no verdict is ever derived from a name looked up twice.
///
/// Shared by `raw_state` (the fragment) and `super::write::quarantine_if_still_ours` (the
/// quarantined former occupant), which both need this identical classify-before-read discipline.
/// Every failure is a [`ReadFailure`], which states what did not complete and leaves what that
/// means about the id to the caller.
pub(super) fn classify_and_read(path: &Path) -> ReadResult<RawState> {
    let Some(name) = path.file_name() else {
        debug_assert!(
            false,
            "classify_and_read needs an artifact path, got {}",
            path.display()
        );
        return Err(ReadFailure::new(
            "classify",
            path,
            io::Error::from(io::ErrorKind::InvalidInput),
        ));
    };
    let Some(dir) = open_parent(path)? else {
        return Ok(RawState::Absent);
    };

    let classified = match openat(&dir, name, OFlag::O_PATH | OFlag::O_NOFOLLOW, Mode::empty()) {
        Ok(fd) => fs::File::from(fd)
            .metadata()
            .map_err(|e| ReadFailure::new("stat", path, e))?,
        Err(Errno::ENOENT) => return Ok(RawState::Absent),
        // `O_PATH | O_NOFOLLOW` is the one open that yields a descriptor for a symlink instead of
        // refusing it, so nothing reaching here has established a type: `EACCES` on the fragment's
        // own directory entry, `EIO`. Presence itself is unsettled, which is neither `Absent` nor
        // `NotOurs`.
        Err(e) => return Err(ReadFailure::new("classify", path, errno_io(e))),
    };
    if !classified.is_file() {
        return Ok(RawState::NotOurs);
    }
    read_regular(&dir, name, path)
}

/// The directory holding `path`, opened `O_PATH | O_DIRECTORY` so both steps resolve their final
/// component under it. `Ok(None)` for a parent that does not exist: nothing can be at a path whose
/// directory is not there, which is absence, not a failure to determine it.
///
/// Every other failure is the caller's, unchanged from resolving the whole pathname at once:
/// `ENOTDIR` for a non-directory component, `EACCES` for a directory this caller may not search,
/// `ELOOP` for a symlink loop above the artifact. Each leaves presence itself unestablished — which
/// is exactly why they must not reach step 2, where the same errnos mean something specific about
/// the artifact.
fn open_parent(path: &Path) -> ReadResult<Option<OwnedFd>> {
    use std::os::unix::fs::OpenOptionsExt as _;

    let dir = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    // `OpenOptions` insists on an access mode even where `O_PATH` makes the kernel ignore it.
    match fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_PATH | libc::O_DIRECTORY)
        .open(dir)
    {
        Ok(handle) => Ok(Some(OwnedFd::from(handle))),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(ReadFailure::new("open the directory holding", path, e)),
    }
}

/// Step 2 on its own: open `name` under `dir` for reading and settle what it is from the descriptor
/// open returned — never from what step 1 saw. `path` is `dir/name`, carried only so a failure names
/// the artifact the way every other message here does.
///
/// A replacement landing between the two opens is therefore *read*, as the file that is now there.
/// That is the honest answer: the question is "what is at this path", and something readable is one.
/// Comparing `(st_dev, st_ino)` against step 1's instead would report goetia's own update path as
/// an unanswerable question — `super::write::replace_unit_verified` replaces a fragment by
/// `rename`, as do `systemctl edit`, dpkg and ansible — and would raise `list`'s host-wide exit
/// code over a benign concurrent update, exactly as the `ENOENT` arm below refuses to do for a
/// benign concurrent uninstall. It would also abort `Systemd::install`'s race-retry loop, which
/// exists to re-classify this very state change rather than to bail out of it.
///
/// # Which errno classifies, and which is a genuine failure
///
/// The distinction this function turns on, stated once so the next change to it does not have to
/// rediscover it: **an errno that establishes what is at the path is a verdict, not a failure.**
/// `O_NOFOLLOW | O_NONBLOCK` is chosen so that the type of the thing there decides the errno, and
/// resolving `name` under `dir` is what keeps every one of them about the artifact:
///
/// - `ENOENT` — nothing is there. [`RawState::Absent`]; the uninstall race step 1 already tolerates,
///   observed one syscall later.
/// - `ELOOP` — `O_NOFOLLOW` refuses a symlink, and refusing it is how it *identifies* it. This is
///   `systemctl mask`'s own artifact arriving between the two opens, which systemd's
///   `symlink_atomic()`, `ln -sfn`, ansible and nix all install by symlink-then-`rename`.
///   [`RawState::NotOurs`], exactly as step 1 answers for the identical file with no race involved.
/// - `ENXIO`, and `ENODEV` for the kernels that return it in the same case — a socket, or a device
///   node with no device behind it. Both identify a non-regular file. [`RawState::NotOurs`].
/// - everything else — `EACCES` on a fragment that is right there but this caller may not read,
///   `EIO`, `EOVERFLOW`. These establish nothing about *whose* the artifact is, which is the whole
///   question, so they stay [`ReadFailure`] and the caller decides what that costs.
///
/// `EACCES` is the one worth naming explicitly, because it is the tempting mistake: it does prove
/// something is there, but not what wrote it — and a 0600 unit is as easily goetia's own as a
/// stranger's. That is the state an unelevated `list` meets constantly, and calling it `NotOurs`
/// would answer "not goetia's" off a read that never happened.
fn read_regular(dir: &OwnedFd, name: &OsStr, path: &Path) -> ReadResult<RawState> {
    let file = match openat(
        dir,
        name,
        OFlag::O_RDONLY | OFlag::O_NOFOLLOW | OFlag::O_NONBLOCK,
        Mode::empty(),
    ) {
        Ok(fd) => fs::File::from(fd),
        Err(Errno::ENOENT) => return Ok(RawState::Absent),
        Err(Errno::ELOOP | Errno::ENXIO | Errno::ENODEV) => return Ok(RawState::NotOurs),
        Err(e) => return Err(ReadFailure::new("open", path, errno_io(e))),
    };
    // The verdict comes from the descriptor about to be read, so a non-regular file swapped in
    // after step 1 is classified rather than read — the property the identity comparison this
    // replaced was reaching for, and the only one worth keeping.
    let opened = file.metadata().map_err(|e| ReadFailure::new("stat", path, e))?;
    if !opened.is_file() {
        return Ok(RawState::NotOurs);
    }
    let mut bytes = Vec::new();
    (&file)
        .read_to_end(&mut bytes)
        .map_err(|e| ReadFailure::new("read", path, e))?;
    // Obtained, and they will not decode: see [`RawState::NotOurs`]. Distinct from every arm above
    // it — those never got bytes — and the reason this reads to a `Vec` rather than to a `String`,
    // which would fold "not UTF-8" into the same `io::Error` an `EIO` arrives as.
    match String::from_utf8(bytes) {
        Ok(text) => Ok(RawState::Regular(text)),
        Err(_) => Ok(RawState::NotOurs),
    }
}

/// The `io::Error` a `nix` errno stands for, so one [`ReadFailure`] type carries both this module's
/// `openat` failures and its `std::fs` ones.
fn errno_io(errno: Errno) -> io::Error {
    io::Error::from_raw_os_error(errno as i32)
}

pub(super) fn raw_state(id: &str) -> Result<RawState> {
    classify_and_read(&unit_path(id)).map_err(|failure| failure.undetermined(id))
}

// Discovery / discover ================================================================================================

/// What `install`/`preview_install` classified at `id`: an [`Ownership`] plus everything specific to
/// this backend that `decide` cannot see on its own.
pub(super) struct Discovery {
    pub(super) ownership: Ownership,
    /// `None` iff `ownership` is `Ownership::Absent`.
    pub(super) on_disk: Option<String>,
    /// The fragment's own raw text — distinct from `on_disk`, which may have a drop-in marker folded
    /// in — present iff `ownership` came from a regular file (`Ours` or `OursUnreadable`). Used to
    /// verify identity before a later write/removal touches this exact fragment; see
    /// `super::write::quarantine_if_still_ours`. Content, not inode, is the identity that matters
    /// here: an inode number can be reused by the kernel moments after its file is unlinked, so two
    /// genuinely different files can share one.
    pub(super) fragment_text: Option<String>,
    /// What systemd's drop-in search directories currently hold for this id — obligation 3.
    /// `decide` cannot see this: its vocabulary is artifact *text*, and this is filesystem structure
    /// alongside it.
    pub(super) overlay: Overlay,
}

pub(super) fn discover(id: &str) -> Result<Discovery> {
    match raw_state(id)? {
        RawState::Absent => match residue(id).map_err(|failure| failure.undetermined(id))? {
            None => Ok(Discovery {
                ownership: Ownership::Absent,
                on_disk: None,
                fragment_text: None,
                overlay: Overlay::default(),
            }),
            // A drop-in directory or an enablement link with no fragment at all: never silently
            // adopt it as `Create`, or the resulting unit inherits overrides and boot-enrollment
            // goetia never wrote and cannot show — refuse it the same way any other pre-existing,
            // unmarked artifact is refused.
            Some(residue) => Ok(Discovery {
                ownership: Ownership::Foreign,
                on_disk: Some(residue.text()),
                fragment_text: None,
                overlay: dropin_overlay(id, &residue.dropin),
            }),
        },
        RawState::NotOurs => Ok(Discovery {
            ownership: Ownership::Foreign,
            on_disk: Some(String::new()),
            fragment_text: None,
            overlay: Overlay::default(),
        }),
        RawState::Regular(text) => {
            // Ownership comes from the fragment alone, and is settled before any drop-in is
            // touched: the marker is already in the text this arm was handed.
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

            // Obligation 3: fold any drop-in content into the text `decide` compares, without ever
            // writing that folded text back. Neither `desired` nor `regenerated` (both pure
            // `generate()` output) can ever contain this marker, so a non-empty drop-in forces
            // `Conflict` whenever `decide` reaches a text comparison at all — the one branch that
            // doesn't (a stale version, checked before any text comparison) is `decide::decide`'s own
            // `foreign_overlay` parameter's job.
            match dropin_dirs(id) {
                Ok(dirs) => {
                    let marker: String = dirs.iter().map(|(_, text)| text.as_str()).collect();
                    Ok(Discovery {
                        ownership,
                        on_disk: Some(format!("{text}{marker}")),
                        overlay: dropin_overlay(id, &dirs),
                        fragment_text: Some(text),
                    })
                }
                // Installation is established (the fragment is open and read) and so is ownership
                // (its marker is decoded, or demonstrably absent), so [`Error::Undetermined`] —
                // "cannot determine whether daemon `X` is installed" — would deny two facts about an
                // id goetia just decoded. What the failed read actually costs is the ability to
                // report on the id, which is what `Ownership::OursUnreadable` says.
                Err(failure) => Ok(Discovery {
                    ownership: if matches!(ownership, Ownership::Foreign) {
                        // Established by the fragment's own missing marker, and untouched by a
                        // drop-in nobody could read: `Foreign` refuses on the absent marker alone.
                        Ownership::Foreign
                    } else {
                        Ownership::OursUnreadable {
                            reason: format!(
                                "a drop-in directory could not be read, so what systemd applies to it cannot be \
                                 compared: {}",
                                failure.detail()
                            ),
                        }
                    },
                    on_disk: Some(text.clone()),
                    fragment_text: Some(text),
                    // Unknown, and unread: `decide` consults the overlay only on the `Ours` path,
                    // which neither arm above can reach.
                    overlay: Overlay::default(),
                }),
            }
        }
    }
}

// undetermined ========================================================================================================

/// The error for a failed read that was supposed to tell this backend whether anything is at `id`:
/// the fragment's own open/read ([`raw_state`]), and — where that found no fragment — the drop-in
/// scan and the `.wants` link stat ([`residue`]). Never for a read that failed *after* the fragment
/// was decoded: see [`ReadFailure`] and `discover`'s `RawState::Regular` arm, which has established
/// both the installation and its ownership by then and reports `Ownership::OursUnreadable` instead.
///
/// [`Error::Undetermined`], never [`io_err`]'s `Error::Other`: `Other` reaches
/// `cli::report::status_error`'s catch-all as `Kind::Unreadable`, which *asserts* that goetia owns
/// the id — the one thing a read that never completed cannot establish. See
/// [`Error::Undetermined`]'s doc comment for why that claim is worth a variant of its own.
///
/// Every failure but `NotFound` lands here, not `PermissionDenied` alone. An `EIO` on the drop-in
/// directory leaves goetia exactly as ignorant of the id as an `EACCES` does, so a variant chosen by
/// errno would restore the false ownership claim for the narrower input while the fix looked
/// complete. What the errno does choose is `recovery`: re-running elevated is advice only a
/// permission boundary earns, and offering it for a failing disk sends the user somewhere useless.
fn undetermined(id: &str, op: &str, path: &Path, source: &io::Error) -> Error {
    let recovery = if source.kind() == io::ErrorKind::PermissionDenied {
        "re-run as root (or under sudo): that read is what tells goetia whether anything is \
         installed at this id"
    } else {
        "resolve that failure and re-run: that read is what tells goetia whether anything is \
         installed at this id"
    };
    Error::Undetermined {
        id: id.to_string(),
        reason: format!("failed to {op} {}: {source}", path.display()),
        recovery: recovery.to_string(),
    }
}

// Drop-in search path =================================================================================================

/// Every directory systemd's system unit load path searches, in the precedence order
/// `systemd.unit(5)`'s "System Unit Search Path" gives (its Table 1 is the same list annotated). All
/// of them applies simultaneously: a `<id>.service.d` under any one of them is applied to this id,
/// regardless of which directory holds the fragment itself.
///
/// The two `.control` roots outrank `/etc/systemd/system`, and are where `systemctl set-property
/// UNIT PROPERTY=VALUE` writes its `<id>.service.d/50-<Property>.conf` (verified on systemd 257:
/// `systemctl set-property x.service MemoryMax=8G` produced
/// `/etc/systemd/system.control/x.service.d/50-MemoryMax.conf`, and `systemctl show -p MemoryMax`
/// reported the new value). Omitting a root is a false *negative* in both halves of this module at
/// once: `diff` reports an id "up to date" while systemd applies a memory cap to it, and `residue`
/// finds nothing where `install` on the same state refuses — so `uninstall x && echo "confirmed
/// gone"` prints for an id systemd still holds configuration for.
///
/// Goetia only ever writes into the first `/etc/systemd/system` entry (`UNIT_DIR`); every other root
/// is read-only from this backend's point of view, so a drop-in found there is detected (folded into
/// `on_disk`, so `decide` reports drift) but never removed by a successful write.
pub(super) const DROPIN_SEARCH_DIRS: [&str; 12] = [
    "/etc/systemd/system.control",
    "/run/systemd/system.control",
    "/run/systemd/transient",
    "/run/systemd/generator.early",
    UNIT_DIR,
    "/etc/systemd/system.attached",
    "/run/systemd/system",
    "/run/systemd/system.attached",
    "/run/systemd/generator",
    "/usr/local/lib/systemd/system",
    "/usr/lib/systemd/system",
    "/run/systemd/generator.late",
];

// Drop-in scan ========================================================================================================

/// What a set of drop-in directories amounts to for [`crate::decide::decide`].
///
/// Goetia writes exactly one of them — `UNIT_DIR/<id>.service.d`, which every successful
/// `Update`/`Stale` write clears (see `Systemd::install`) — so a drop-in under any other search root
/// survives a `--force` overwrite untouched, and the run after it reports the identical conflict.
/// Naming those directories here, where the scan already has them, is what keeps the CLI from
/// re-reading the filesystem to find out whether its own published remedy applies.
fn dropin_overlay(id: &str, dirs: &[(PathBuf, String)]) -> Overlay {
    let ours = Path::new(UNIT_DIR).join(format!("{id}.service.d"));
    let unclearable: Vec<&Path> = dirs
        .iter()
        .map(|(dir, _)| dir.as_path())
        .filter(|dir| *dir != ours)
        .collect();
    Overlay {
        present: !dirs.is_empty(),
        unclearable_recovery: (!unclearable.is_empty()).then(|| dropin_recovery(&unclearable)),
    }
}

/// How to resolve a conflict `--force` cannot. Deliberately parallel to [`residue_recovery`], which
/// says the same thing about the same directories on the path where the fragment is already gone.
fn dropin_recovery(unclearable: &[&Path]) -> String {
    let paths = unclearable
        .iter()
        .map(|p| format!("\n  {}", p.display()))
        .collect::<String>();
    format!(
        "systemd applies configuration goetia did not write, from outside `{UNIT_DIR}`:{paths}\n\
         `--force` rewrites `{UNIT_DIR}/<id>.service` and clears only `{UNIT_DIR}`'s own drop-in, so \
         it cannot resolve this — remove the directories above by hand, run `systemctl \
         daemon-reload`, and re-run."
    )
}

/// `<id>.service.d`'s `*.conf` files under every root in [`DROPIN_SEARCH_DIRS`], per directory so a
/// caller that has to *name* them for a human ([`residue`], [`dropin_recovery`]) and one that has to
/// *compare their content* ([`discover`]) cannot drift on which files count as drop-ins. One scan
/// for all of them: what is this id's artifact and what occupies this id are the same set of
/// directories, which is what keeps `install` and `uninstall` from describing one filesystem state
/// differently. Only that directory name: see the module doc comment for the family-wide ones
/// systemd also reads and this deliberately does not.
fn dropin_dirs(id: &str) -> ReadResult<Vec<(PathBuf, String)>> {
    let mut found = Vec::new();
    for search_dir in DROPIN_SEARCH_DIRS {
        let dir = Path::new(search_dir).join(format!("{id}.service.d"));
        let text = dropin_marker_in(&dir)?;
        if !text.is_empty() {
            found.push((dir, text));
        }
    }
    Ok(found)
}

fn dropin_marker_in(dir: &Path) -> ReadResult<String> {
    let mut entries = match fs::read_dir(dir) {
        Ok(rd) => rd
            .collect::<io::Result<Vec<_>>>()
            .map_err(|e| ReadFailure::new("read", dir, e))?,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(String::new()),
        Err(e) => return Err(ReadFailure::new("read", dir, e)),
    };
    entries.sort_by_key(std::fs::DirEntry::file_name);

    let mut marker = String::new();
    for entry in &entries {
        let file_name = entry.file_name();
        if !file_name.to_string_lossy().ends_with(".conf") {
            continue;
        }
        let path = entry.path();
        // `fs::metadata` follows symlinks, deliberately unlike `classify_and_read`'s `O_NOFOLLOW`
        // view of the fragment itself: systemd follows a drop-in symlink exactly like a regular file
        // when applying overrides (common under ansible/stow/nix-managed `/etc`), so drift detection
        // must too.
        let meta = match fs::metadata(&path) {
            Ok(m) => m,
            Err(e) if e.kind() == io::ErrorKind::NotFound => continue, // dangling symlink
            Err(e) => return Err(ReadFailure::new("stat", &path, e)),
        };
        if !meta.is_file() {
            continue;
        }
        let content = match fs::read_to_string(&path) {
            Ok(c) => c,
            // Removed between the stat above and this read — the same benign race the stat itself
            // already tolerates.
            Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
            Err(e) => return Err(ReadFailure::new("read", &path, e)),
        };
        marker.push_str(&format!("\n# --- drop-in: {} ---\n{content}", path.display()));
    }
    Ok(marker)
}

// Residue =============================================================================================================

/// The one `*.target.wants` directory `systemctl enable` can link this id into: `generate::unit`'s
/// `[Install]` section is always exactly `WantedBy=multi-user.target`, and goetia emits no
/// `RequiredBy=`, so there is no `.requires` counterpart.
const WANTS_DIR: &str = "multi-user.target.wants";

/// Where an enablement link for this id can be: the unit directories proper, and not the rest of
/// [`DROPIN_SEARCH_DIRS`]. `systemctl enable` writes into `/etc/systemd/system` (persistent) or
/// `/run/systemd/system` (`--runtime`), and a distribution package can ship a preset-enabled unit's
/// link under `/usr/lib` or `/usr/local/lib`; the `.control`, `transient`, `.attached` and
/// `generator` roots hold dbus-created, transient or generated configuration, where a stat for
/// `multi-user.target.wants/<id>.service` is a syscall that can find nothing to act on.
const WANTS_SEARCH_DIRS: [&str; 4] = [
    UNIT_DIR,
    "/run/systemd/system",
    "/usr/local/lib/systemd/system",
    "/usr/lib/systemd/system",
];

/// Everything goetia-attributable that systemd keeps applying at `id` after the fragment itself is
/// gone: a `<id>.service.d/*.conf` drop-in, and a `multi-user.target.wants/<id>.service` link that
/// still enrolls the id at boot. Consulted only where [`raw_state`] found no fragment.
struct Residue {
    /// Each drop-in directory holding at least one `*.conf`, with that directory's marker text.
    dropin: Vec<(PathBuf, String)>,
    links: Vec<PathBuf>,
}

impl Residue {
    /// The `on_disk` text `decide` is handed for this id. Never empty — [`residue`] returns `None`
    /// rather than an empty `Residue` — which is what keeps [`Discovery::on_disk`]'s "`None` iff
    /// `Ownership::Absent`" invariant true.
    fn text(&self) -> String {
        let mut text: String = self.dropin.iter().map(|(_, marker)| marker.as_str()).collect();
        for link in &self.links {
            text.push_str(&format!("\n# --- enablement link: {} ---\n", link.display()));
        }
        text
    }

    /// Every path a human has to deal with to empty this id, for the refusal message.
    fn paths(&self) -> Vec<&Path> {
        self.dropin
            .iter()
            .map(|(dir, _)| dir.as_path())
            .chain(self.links.iter().map(PathBuf::as_path))
            .collect()
    }
}

/// What is left at `id` besides the fragment, `None` when the id is genuinely unoccupied, or a
/// [`ReadFailure`] when a read this answer depends on did not complete. What that failure *means*
/// stays the caller's, as everywhere else in this module: [`Error::Undetermined`] for the verbs
/// (see [`undetermined`]), a named `Installed::Undetermined` entry for `Systemd::list` (see
/// [`residue_read`]).
///
/// The single source of "is this id really empty" for both [`discover`] (so `install` never
/// silently adopts what it did not write) and [`require_installed`] (so `uninstall` never reports
/// [`Error::NotInstalled`] — which the CLI renders as success, exit `0` — for an id that still has
/// something on it). Two verbs answering that question from different evidence is exactly how
/// `uninstall x && echo "confirmed gone"` came to print for a unit still loaded, still running and
/// still `.wants`-linked.
fn residue(id: &str) -> ReadResult<Option<Residue>> {
    let dropin = dropin_dirs(id)?;
    let mut links = Vec::new();
    for search_dir in WANTS_SEARCH_DIRS {
        let link = Path::new(search_dir).join(WANTS_DIR).join(format!("{id}.service"));
        // `symlink_metadata`, never `metadata`: the leftover this exists to catch is precisely a
        // symlink whose target — the fragment — is already gone, which `metadata` reports as absent.
        match fs::symlink_metadata(&link) {
            Ok(_) => links.push(link),
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(ReadFailure::new("stat", &link, e)),
        }
    }
    if dropin.is_empty() && links.is_empty() {
        return Ok(None);
    }
    Ok(Some(Residue { dropin, links }))
}

/// The [`residue`] reads on their own, for `Systemd::list`: `Err` exactly when what would have
/// settled whether anything occupies `id` did not complete.
///
/// What the scan *found* is not `list`'s business — residue under an absent fragment is
/// `Ownership::Foreign` (see [`discover`]), and `list` omits foreign ids the same as it omits an
/// unoccupied one. The *failure* is, and for the reason the whole class exists: leaving the id out
/// of the enumeration is how a listing says it is not there, and this read established no such
/// thing. `Systemd::status` answers [`Error::Undetermined`] off the identical failure, so this is
/// also what keeps the two from describing one filesystem state differently.
pub(super) fn residue_read(id: &str) -> ReadResult<()> {
    residue(id).map(drop)
}

/// The error for an id whose fragment [`raw_state`] found absent: [`Error::NotInstalled`] only when
/// nothing goetia-attributable is there at all, [`Error::Foreign`] otherwise — or, when the scan
/// itself could not be completed, [`residue`]'s own [`Error::Undetermined`], which claims neither.
/// Every verb that has to
/// answer "is anything at this id" — [`require_installed`] for the mutating ones, `Systemd::status`
/// for the read-only one — goes through here, so none of them can disagree with [`discover`] about
/// one filesystem state.
pub(super) fn absent_error(id: &str) -> Result<Error> {
    Ok(match residue(id).map_err(|failure| failure.undetermined(id))? {
        None => Error::NotInstalled { id: id.to_string() },
        Some(residue) => Error::Foreign {
            id: id.to_string(),
            recovery: residue_recovery(id, &residue),
        },
    })
}

/// How to empty an id whose fragment is gone but whose [`Residue`] is not. Goetia removes none of
/// it itself: `<id>.service.d` in `/run` or `/usr/lib` — and, for that matter, in `/etc` — is just
/// as plausibly an administrator's override of a unit *shipped elsewhere* as it is goetia's own
/// leftover, and there is nothing on disk that distinguishes the two.
fn residue_recovery(id: &str, residue: &Residue) -> String {
    let paths = residue
        .paths()
        .iter()
        .map(|p| format!("\n  {}", p.display()))
        .collect::<String>();
    format!(
        "no `{UNIT_DIR}/{id}.service`, but systemd still applies configuration attached to `{id}`:\
         {paths}\ngoetia cannot tell its own leftovers from an administrator's overrides of a unit \
         shipped elsewhere, so it removes neither — `systemctl disable {id}.service` drops the \
         enablement link and a drop-in directory has to go by hand. Then run `systemctl \
         daemon-reload` and re-run."
    )
}

// require_installed ===================================================================================================

/// The narrower "is this even ours" gate every verb but `install` needs: the marker alone is proof of
/// ownership, matching [`crate::manager::fake::Fake`]'s `require_ours` (an undecodable blob still
/// passes — `uninstall`'s recovery text names exactly that verb as the way out). Returns the
/// fragment's own text for a caller that goes on to remove or replace it — see
/// `super::write::quarantine_if_still_ours`.
///
/// [`Error::NotInstalled`] means *nothing goetia-attributable is at this id*, not merely "the
/// fragment file is missing": `cli::uninstall` maps that one variant to exit `0` and "nothing to
/// do", so anything narrower would report success over a residual artifact — and would disagree
/// with [`discover`], which refuses the identical filesystem state as `Ownership::Foreign`. See
/// [`residue`].
pub(super) fn require_installed(id: &str) -> Result<String> {
    match raw_state(id)? {
        RawState::Absent => Err(absent_error(id)?),
        RawState::NotOurs => Err(Error::Foreign {
            id: id.to_string(),
            recovery: crate::decide::foreign_recovery(id),
        }),
        RawState::Regular(text) => match generate::extract(&text) {
            Ok(None) => Err(Error::Foreign {
                id: id.to_string(),
                recovery: crate::decide::foreign_recovery(id),
            }),
            Ok(Some(_)) | Err(_) => Ok(text),
        },
    }
}

#[cfg(test)]
#[path = "discover_tests.rs"]
mod discover_tests;
