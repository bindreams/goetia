//! Classifying what's currently at an id: reading the fragment, folding in any drop-in drift, and
//! turning that into the [`Ownership`] `decide` needs.

use std::fs;
use std::io;

use crate::backend::systemd::generate;
use crate::decide::Ownership;
use crate::error::{Error, Result};

use super::{dropin_dir, identity_for, io_err, unit_path};

// RawState / raw_state ================================================================================================

/// What's physically present at `id`'s unit path, before any interpretation of its content.
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
/// target.
const O_NOFOLLOW: i32 = 0o400_000;
/// `ELOOP`, the `errno` `open(2)` reports for `O_NOFOLLOW` against a symlink — likewise part of the
/// stable Linux ABI (`asm-generic/errno.h`).
const ELOOP: i32 = 40;

/// Classify and read `id`'s unit path from a single open file handle, rather than a separate `lstat`
/// followed by a separate open-and-read: two syscalls resolving the same path independently is its
/// own TOCTOU gap (the file the `lstat` classified need not be the file the read later opens), and
/// they disagreed on top of that — the read implicitly followed a symlink the `lstat` deliberately
/// did not. `O_NOFOLLOW` makes the *open itself* the classification: it fails with `ELOOP` for a
/// symlink (a masked unit — obligation 2) exactly where a plain open would have silently followed it
/// through to `/dev/null`.
pub(super) fn raw_state(id: &str) -> Result<RawState> {
    use std::os::unix::fs::OpenOptionsExt as _;

    let path = unit_path(id);
    let file = match fs::OpenOptions::new().read(true).custom_flags(O_NOFOLLOW).open(&path) {
        Ok(f) => f,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(RawState::Absent),
        Err(e) if e.raw_os_error() == Some(ELOOP) => return Ok(RawState::NonRegular),
        Err(e) => return Err(io_err("open", &path, e)),
    };

    let meta = file.metadata().map_err(|e| io_err("stat", &path, e))?;
    if !meta.is_file() {
        // Some other non-regular file `O_NOFOLLOW` still let through (a FIFO, a device node): never
        // read through it, same as a masked unit.
        return Ok(RawState::NonRegular);
    }

    let text = std::io::read_to_string(&file).map_err(|e| io_err("read", &path, e))?;
    Ok(RawState::Regular(text))
}

// Discovery / discover =================================================================================================

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
    /// genuinely different files can share one — verified empirically triggering the false-positive
    /// this would otherwise cause.
    pub(super) fragment_text: Option<String>,
    /// Whether `<id>.service.d/` currently holds any `*.conf` file — obligation 3. `decide` cannot
    /// see this: its vocabulary is artifact *text*, and this is filesystem structure alongside it.
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

// dropin_marker ========================================================================================================

/// A deterministic representation of `<id>.service.d`'s `*.conf` files — the only ones
/// `systemd.unit(5)` reads as drop-ins — or `None` if the directory does not exist or holds none.
pub(super) fn dropin_marker(id: &str) -> Result<Option<String>> {
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
        let content = match fs::read_to_string(&path) {
            Ok(c) => c,
            // Removed between the stat above and this read — the same benign race the stat itself
            // already tolerates.
            Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
            Err(e) => return Err(io_err("read", &path, e)),
        };
        marker.push_str(&format!(
            "\n# --- drop-in: {} ---\n{content}",
            file_name.to_string_lossy()
        ));
    }
    Ok(if marker.is_empty() { None } else { Some(marker) })
}

// require_installed ====================================================================================================

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
