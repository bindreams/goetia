//! Classifying what's currently at an id: reading the fragment, folding in any drop-in drift, and
//! turning that into the [`Ownership`] `decide` needs.

use std::fs;
use std::io;
use std::path::Path;

use crate::backend::systemd::generate;
use crate::decide::Ownership;
use crate::error::{Error, Result};

use super::{UNIT_DIR, identity_for, io_err, unit_path};

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
pub(super) fn classify_and_read(path: &Path) -> Result<RawState> {
    use std::os::unix::fs::OpenOptionsExt as _;

    match fs::OpenOptions::new().read(true).custom_flags(O_NOFOLLOW).open(path) {
        Ok(file) => {
            let meta = file.metadata().map_err(|e| io_err("stat", path, e))?;
            if !meta.is_file() {
                // Some other non-regular file `O_NOFOLLOW` still let through (a FIFO, a device
                // node): never read through it, same as a masked unit.
                return Ok(RawState::NonRegular);
            }
            let text = std::io::read_to_string(&file).map_err(|e| io_err("read", path, e))?;
            Ok(RawState::Regular(text))
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(RawState::Absent),
        Err(_) => match fs::symlink_metadata(path) {
            Ok(_) => Ok(RawState::NonRegular),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(RawState::Absent),
            Err(e) => Err(io_err("stat", path, e)),
        },
    }
}

pub(super) fn raw_state(id: &str) -> Result<RawState> {
    classify_and_read(&unit_path(id))
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
    /// Whether any of systemd's drop-in search directories currently hold a `<id>.service.d/*.conf`
    /// file for this id — obligation 3. `decide` cannot see this: its vocabulary is artifact *text*,
    /// and this is filesystem structure alongside it.
    pub(super) dropin_present: bool,
}

pub(super) fn discover(id: &str) -> Result<Discovery> {
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
            // doesn't (a stale version, checked before any text comparison) is `decide::decide`'s own
            // `foreign_overlay` parameter's job.
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

// dropin_marker =======================================================================================================

/// The three directories systemd's unit load path searches for `<id>.service.d/*.conf` drop-ins
/// (`systemd.unit(5)`): `/etc` overrides `/run` overrides `/usr/lib`, but all three apply
/// simultaneously — a unit's *effective* configuration is the merge of every one of them, regardless
/// of which directory holds the fragment itself. `systemctl edit --runtime` — one flag away from the
/// plain `systemctl edit` the design cites — writes into the `/run` copy, not `/etc`. Goetia only
/// ever writes into the first of these (`UNIT_DIR`); the other two are read-only from this backend's
/// point of view, so a drop-in found there is detected (folded into `on_disk`, so `decide` reports
/// drift) but never removed by a successful write — only `UNIT_DIR`'s own copy is goetia's to clear.
const DROPIN_SEARCH_DIRS: [&str; 3] = [UNIT_DIR, "/run/systemd/system", "/usr/lib/systemd/system"];

/// A deterministic representation of `<id>.service.d`'s `*.conf` files — the only ones
/// `systemd.unit(5)` reads as drop-ins — across every directory systemd's unit load path searches, or
/// `None` if none of them exist or hold any.
pub(super) fn dropin_marker(id: &str) -> Result<Option<String>> {
    let mut marker = String::new();
    for search_dir in DROPIN_SEARCH_DIRS {
        let dir = Path::new(search_dir).join(format!("{id}.service.d"));
        marker.push_str(&dropin_marker_in(&dir)?);
    }
    Ok(if marker.is_empty() { None } else { Some(marker) })
}

fn dropin_marker_in(dir: &Path) -> Result<String> {
    let mut entries = match fs::read_dir(dir) {
        Ok(rd) => rd.collect::<io::Result<Vec<_>>>().map_err(|e| io_err("read", dir, e))?,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(String::new()),
        Err(e) => return Err(io_err("read", dir, e)),
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
        let content = match fs::read_to_string(&path) {
            Ok(c) => c,
            // Removed between the stat above and this read — the same benign race the stat itself
            // already tolerates.
            Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
            Err(e) => return Err(io_err("read", &path, e)),
        };
        marker.push_str(&format!("\n# --- drop-in: {} ---\n{content}", path.display()));
    }
    Ok(marker)
}

// require_installed ===================================================================================================

/// The narrower "is this even ours" gate every verb but `install` needs: the marker alone is proof of
/// ownership, matching [`crate::manager::fake::Fake`]'s `require_ours` (an undecodable blob still
/// passes — `uninstall`'s recovery text names exactly that verb as the way out). Returns the
/// fragment's own text for a caller that goes on to remove or replace it — see
/// `super::write::quarantine_if_still_ours`.
pub(super) fn require_installed(id: &str) -> Result<String> {
    match raw_state(id)? {
        RawState::Absent => Err(Error::NotInstalled { id: id.to_string() }),
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
