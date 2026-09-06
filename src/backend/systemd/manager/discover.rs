//! Classifying what's currently at an id: reading the fragment, folding in any drop-in drift, and
//! turning that into the [`Ownership`] `decide` needs.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use crate::backend::systemd::generate;
use crate::decide::Ownership;
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

/// What's physically present at a path, before any interpretation of its content.
pub(super) enum RawState {
    Absent,
    /// A symlink (a masked unit) or any other non-regular file — obligation 2. Its contents are never
    /// read: a masked unit's target is `/dev/null`, and reading through it would look identical to
    /// "nothing here".
    NonRegular,
    Regular(String),
}

/// `O_NOFOLLOW`, hardcoded rather than pulled from a dependency: this file is already
/// `#[cfg(target_os = "linux")]`-only (via its parent), and the value is part of the stable Linux
/// syscall ABI (`asm-generic/fcntl.h`), identical across every architecture Rust supports for this
/// target (confirmed: `0o400_000` == asm-generic's `00400000` == SPARC's `0x20000`).
const O_NOFOLLOW: i32 = 0o400_000;

/// Classify and read `path` from a single open file handle, rather than a separate `lstat` followed
/// by a separate open-and-read: two syscalls resolving the same path independently is its own TOCTOU
/// gap (the file the `lstat` classified need not be the file the read later opens), and a plain
/// second open would additionally disagree by silently following a symlink the `lstat` deliberately
/// did not. `O_NOFOLLOW` makes the *open itself* the classification: it fails for a symlink (a masked
/// unit — obligation 2) exactly where a plain open would have silently followed it through to
/// `/dev/null`. Shared by `raw_state` (the fragment) and `super::write::quarantine_if_still_ours`
/// (the quarantined former occupant), which both need this identical classify-before-read discipline.
///
/// The failure `open` reports for a symlink is deliberately *not* checked by its numeric `errno`
/// value: `ELOOP` is 40 on the Linux ABI most architectures share, but not on MIPS, whose errno table
/// is SysV-derived (40 is `EL3RST` there; `ELOOP` is 90) — trusting the wrong number there would
/// treat a masked unit as an unclassifiable I/O error instead of `NonRegular`. `lstat`-ing the path in
/// the catch-all arm instead is architecture-independent, and is the same rule this function already
/// applies via `O_NOFOLLOW` for the case that succeeds.
///
/// That `lstat` has to *confirm* what it found, never merely succeed: it needs only search permission
/// on the parent directory, so it succeeds for an ordinary regular file whose own mode denied the
/// open. Treating "the open failed and the `lstat` did not" as proof of a masked unit therefore
/// returned `NonRegular` — and so `Ownership::Foreign`, "demonstrably not managed by goetia" — for a
/// 0600 fragment nobody had looked inside, which is the one verdict a read that never happened
/// cannot support. Each arm below states which syscall established its answer.
pub(super) fn classify_and_read(path: &Path) -> ReadResult<RawState> {
    use std::os::unix::fs::OpenOptionsExt as _;

    match fs::OpenOptions::new().read(true).custom_flags(O_NOFOLLOW).open(path) {
        Ok(file) => {
            let meta = file.metadata().map_err(|e| ReadFailure::new("stat", path, e))?;
            if !meta.is_file() {
                // Some other non-regular file `O_NOFOLLOW` still let through (a FIFO, a device
                // node): never read through it, same as a masked unit.
                return Ok(RawState::NonRegular);
            }
            let text = std::io::read_to_string(&file).map_err(|e| ReadFailure::new("read", path, e))?;
            Ok(RawState::Regular(text))
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(RawState::Absent),
        Err(e) => match fs::symlink_metadata(path) {
            // The masked unit, confirmed as a symlink rather than inferred from the open having
            // failed for some reason or other.
            Ok(meta) if meta.is_symlink() => Ok(RawState::NonRegular),
            // Any other non-regular file, e.g. a FIFO whose open blocks or a device node whose open
            // fails outright. Same claim as above and the same evidence for it: `lstat` reports the
            // type without following and without reading, which is all `NonRegular` asserts.
            Ok(meta) if !meta.is_file() => Ok(RawState::NonRegular),
            // A regular file the open could not read — `EACCES` on a 0600 fragment is the common
            // one, and the fragment is where it is most likely, since a drop-in directory is
            // usually world-searchable while a fragment's own mode governs its readability.
            Ok(_) => Err(ReadFailure::new("open", path, e)),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(RawState::Absent),
            Err(e) => Err(ReadFailure::new("stat", path, e)),
        },
    }
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
    /// Whether any of systemd's drop-in search directories currently hold a `*.conf` drop-in for
    /// this id — obligation 3. `decide` cannot see this: its vocabulary is artifact *text*, and this
    /// is filesystem structure alongside it.
    pub(super) dropin_present: bool,
}

pub(super) fn discover(id: &str) -> Result<Discovery> {
    match raw_state(id)? {
        RawState::Absent => match residue(id)? {
            None => Ok(Discovery {
                ownership: Ownership::Absent,
                on_disk: None,
                fragment_text: None,
                dropin_present: false,
            }),
            // A drop-in directory or an enablement link with no fragment at all: never silently
            // adopt it as `Create`, or the resulting unit inherits overrides and boot-enrollment
            // goetia never wrote and cannot show — refuse it the same way any other pre-existing,
            // unmarked artifact is refused.
            Some(residue) => Ok(Discovery {
                ownership: Ownership::Foreign,
                on_disk: Some(residue.text()),
                fragment_text: None,
                dropin_present: !residue.dropin.is_empty(),
            }),
        },
        RawState::NonRegular => Ok(Discovery {
            ownership: Ownership::Foreign,
            on_disk: Some(String::new()),
            fragment_text: None,
            dropin_present: false,
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
            match dropin_marker(id) {
                Ok(dropin) => {
                    let on_disk = match &dropin {
                        Some(marker) => format!("{text}{marker}"),
                        None => text.clone(),
                    };
                    Ok(Discovery {
                        ownership,
                        on_disk: Some(on_disk),
                        fragment_text: Some(text),
                        dropin_present: dropin.is_some(),
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
                    // Unknown, and unread: `decide` consults `foreign_overlay` only on the `Ours`
                    // path, which neither arm above can reach.
                    dropin_present: false,
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
/// of them apply simultaneously: a unit's *effective* configuration is the merge of every drop-in
/// found under any of them, regardless of which directory holds the fragment itself.
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
const DROPIN_SEARCH_DIRS: [&str; 12] = [
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

/// Every drop-in directory name `systemd.unit(5)` reads for `<id>.service`, most specific first.
/// Three kinds, and a goetia id can produce all three — `^[A-Za-z0-9._-]{1,80}$` allows dashes, and
/// forbids the `@` that would add a template's and an instance's directories on top:
///
/// - `<id>.service.d`, the one `systemctl edit` writes;
/// - one per dash in the id, the unit name truncated after that dash: for `foo-bar-baz.service`,
///   `foo-bar-.service.d` and `foo-.service.d` (verified: a `.conf` under
///   `goetia-probe-trunc-.service.d` reached `goetia-probe-trunc-x.service`'s `Documentation=`);
/// - `service.d`, the top-level per-type drop-in, which applies to *every* `.service` unit on the
///   host (verified: the same file under `/etc/systemd/system/service.d` also reached
///   `cron.service`).
///
/// Only the first is attributable to this id — the other two exist independently of it and apply to
/// a whole family of units — which is why [`residue`], whose question is "is this *id* occupied",
/// asks for that one alone while drift detection asks for all of them.
fn dropin_dir_names(id: &str) -> Vec<String> {
    let mut names = vec![format!("{id}.service.d")];
    // Longest prefix first, the order the man page's "drop-in files further down the prefix
    // hierarchy override those further up" describes. A prefix equal to the whole id (an id ending
    // in a dash) is dropped rather than naming one directory twice.
    let mut truncations: Vec<String> = id
        .match_indices('-')
        .map(|(i, _)| &id[..=i])
        .filter(|prefix| *prefix != id)
        .map(|prefix| format!("{prefix}.service.d"))
        .collect();
    truncations.reverse();
    names.append(&mut truncations);
    names.push("service.d".to_string());
    names
}

// dropin_marker =======================================================================================================

/// A deterministic representation of every `*.conf` file `systemd.unit(5)` reads as a drop-in for
/// `id` — across every directory name in [`dropin_dir_names`] and every root in
/// [`DROPIN_SEARCH_DIRS`] — or `None` if none of them exist or hold any.
pub(super) fn dropin_marker(id: &str) -> ReadResult<Option<String>> {
    let dirs = dropin_dirs(&dropin_dir_names(id))?;
    if dirs.is_empty() {
        return Ok(None);
    }
    Ok(Some(dirs.iter().map(|(_, text)| text.as_str()).collect()))
}

/// The same scan [`dropin_marker`] concatenates, kept per-directory so a caller that has to *name*
/// the drop-ins for a human ([`residue`]) and one that has to *compare their content*
/// ([`dropin_marker`]) cannot drift on which *files* count as drop-ins. `names` is the one thing the
/// two deliberately differ on, and it is passed in rather than derived here so that difference is
/// visible at both call sites.
fn dropin_dirs(names: &[String]) -> ReadResult<Vec<(PathBuf, String)>> {
    let mut found = Vec::new();
    for search_dir in DROPIN_SEARCH_DIRS {
        for name in names {
            let dir = Path::new(search_dir).join(name);
            let text = dropin_marker_in(&dir)?;
            if !text.is_empty() {
                found.push((dir, text));
            }
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
        // `fs::metadata` follows symlinks, deliberately unlike `raw_state`'s `lstat` of the fragment
        // itself: systemd follows a drop-in symlink exactly like a regular file when applying
        // overrides (common under ansible/stow/nix-managed `/etc`), so drift detection must too.
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

/// What is left at `id` besides the fragment, `None` when the id is genuinely unoccupied, or
/// [`Error::Undetermined`] when a read this answer depends on failed — see [`undetermined`].
///
/// `<id>.service.d` alone of [`dropin_dir_names`]'s three kinds: the truncated-prefix and top-level
/// `service.d` directories are named for a *family* of units rather than for this id, exist whether
/// or not this id ever does, and are applied to every sibling that shares the prefix or the type. So
/// they are drift on an installed unit but not occupancy of an empty id — and [`residue_recovery`],
/// which tells a human to delete what it names, would otherwise point at a directory that is not
/// this id's to delete and whose removal changes unrelated units.
///
/// The single source of "is this id really empty" for both [`discover`] (so `install` never
/// silently adopts what it did not write) and [`require_installed`] (so `uninstall` never reports
/// [`Error::NotInstalled`] — which the CLI renders as success, exit `0` — for an id that still has
/// something on it). Two verbs answering that question from different evidence is exactly how
/// `uninstall x && echo "confirmed gone"` came to print for a unit still loaded, still running and
/// still `.wants`-linked.
fn residue(id: &str) -> Result<Option<Residue>> {
    let dropin = dropin_dirs(&[format!("{id}.service.d")]).map_err(|failure| failure.undetermined(id))?;
    let mut links = Vec::new();
    for search_dir in WANTS_SEARCH_DIRS {
        let link = Path::new(search_dir).join(WANTS_DIR).join(format!("{id}.service"));
        // `symlink_metadata`, never `metadata`: the leftover this exists to catch is precisely a
        // symlink whose target — the fragment — is already gone, which `metadata` reports as absent.
        match fs::symlink_metadata(&link) {
            Ok(_) => links.push(link),
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(undetermined(id, "stat", &link, &e)),
        }
    }
    if dropin.is_empty() && links.is_empty() {
        return Ok(None);
    }
    Ok(Some(Residue { dropin, links }))
}

/// The error for an id whose fragment [`raw_state`] found absent: [`Error::NotInstalled`] only when
/// nothing goetia-attributable is there at all, [`Error::Foreign`] otherwise — or, when the scan
/// itself could not be completed, [`residue`]'s own [`Error::Undetermined`], which claims neither.
/// Every verb that has to
/// answer "is anything at this id" — [`require_installed`] for the mutating ones, `Systemd::status`
/// for the read-only one — goes through here, so none of them can disagree with [`discover`] about
/// one filesystem state.
pub(super) fn absent_error(id: &str) -> Result<Error> {
    Ok(match residue(id)? {
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
        RawState::NonRegular => Err(Error::Foreign {
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
