//! Writing the fragment itself, with the non-clobbering guarantees obligation 1 requires: a brand-new
//! unit must never replace something it never classified, and an update/regenerate must never replace
//! a fragment that changed underneath it since classification.

use std::fs;
use std::io::{self, Write as _};
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use crate::error::{Error, Result};

use super::discover::{RawState, classify_and_read};
use super::{UNIT_DIR, io_err, remove_file_if_present, unit_path};

// CreateOutcome / create_unit =========================================================================================

pub(super) enum CreateOutcome {
    Created,
    /// Something now exists at the target path that wasn't there when `install` classified it —
    /// obligation 1. The caller re-discovers and re-decides rather than clobbering it.
    Raced,
}

/// Write `text` as a brand-new unit at `id`, never replacing an existing file — see the module doc
/// comment's obligation 1. [`tempfile::NamedTempFile::persist_noclobber`] is `linkat(2)` without
/// replace semantics, so this fails rather than clobbers if anything now occupies `unit_path(id)`.
pub(super) fn create_unit(id: &str, text: &str) -> Result<CreateOutcome> {
    let final_path = unit_path(id);
    let tmp = write_temp_unit(id, text)?;
    match tmp.persist_noclobber(&final_path) {
        Ok(_file) => {
            fsync_unit_dir()?;
            Ok(CreateOutcome::Created)
        }
        Err(e) if e.error.kind() == io::ErrorKind::AlreadyExists => Ok(CreateOutcome::Raced),
        Err(e) => Err(io_err("create", &final_path, e.error)),
    }
}

// ReplaceOutcome / replace_unit_verified ==============================================================================

pub(super) enum ReplaceOutcome {
    Replaced,
    /// The fragment at `id` is no longer the one `install` classified — see
    /// `quarantine_if_still_ours`. The caller re-discovers and re-decides.
    Raced,
}

/// Overwrite `id`'s unit with `text`, but only if the fragment currently at `unit_path(id)` still has
/// exactly the content `expected_text` names — closing the same TOCTOU obligation 1 already closes
/// for `create_unit`, reached here from the `Update`/`Stale` arm instead: the gap between
/// classification and this write spans `decide`, `ensure_parent_dirs` (which can spawn a `chown`
/// subprocess), and building the replacement's contents, wide enough for something else to land at
/// this exact path in between (e.g. a concurrent `systemctl mask` on this same, already-managed id).
///
/// There is no kernel-level "replace only if identity X" primitive, so this is done in two safe
/// `rename`s: quarantine the current occupant under a private name and verify its content
/// (`quarantine_if_still_ours`), then place the new content and either discard the quarantined file
/// (verified match) or put it back (mismatch, reported as a race to retry).
pub(super) fn replace_unit_verified(id: &str, text: &str, expected_text: &str) -> Result<ReplaceOutcome> {
    let Some(backup_path) = quarantine_if_still_ours(id, expected_text)? else {
        return Ok(ReplaceOutcome::Raced);
    };
    let final_path = unit_path(id);

    // Every failure from here on restores the quarantined original before reporting, except the one
    // case where doing so would itself clobber a third file — see that arm's own comment. A transient
    // write failure (ENOSPC, EIO, a permission error) must never turn a routine `install` over an
    // already-installed service into a silent uninstall.
    let tmp = match write_temp_unit(id, text) {
        Ok(tmp) => tmp,
        Err(e) => return Err(restore_or_chain(&backup_path, &final_path, e)),
    };

    match tmp.persist_noclobber(&final_path) {
        Ok(_file) => {
            fsync_unit_dir()?;
            remove_file_if_present(&backup_path)?;
            Ok(ReplaceOutcome::Replaced)
        }
        Err(e) if e.error.kind() == io::ErrorKind::AlreadyExists => {
            // Someone else placed a *third* file at `final_path` in the moment it was empty between
            // the quarantine above and this write. Restoring the quarantined original here would
            // clobber that third file — exactly the bug this function exists to prevent — so this
            // reports the situation (naming the quarantine path) instead of guessing which side
            // should win.
            Err(Error::Other(format!(
                "install for `{id}` raced twice: the original fragment is safely quarantined at {}, \
                 but a different file has since appeared at {}; resolve manually and re-run install",
                backup_path.display(),
                final_path.display(),
            )))
        }
        Err(e) => Err(restore_or_chain(
            &backup_path,
            &final_path,
            io_err("replace", &final_path, e.error),
        )),
    }
}

/// Restore the quarantined original before reporting `primary` — unless the restore *itself* fails,
/// in which case both failures are folded into one message rather than letting the restore's own
/// error silently replace `primary` (a real, reachable double failure: an out-of-space device can
/// fail both the original write and the subsequent `link`-based restore in the same call).
fn restore_or_chain(backup_path: &Path, final_path: &Path, primary: Error) -> Error {
    match restore_quarantine(backup_path, final_path) {
        Ok(()) => primary,
        Err(restore_err) => Error::Other(format!(
            "{primary}; additionally, restoring the quarantined original at {} failed: {restore_err}",
            backup_path.display()
        )),
    }
}

// quarantine_if_still_ours / restore_quarantine =======================================================================

/// Rename `unit_path(id)` to a private, per-attempt-unique backup name, then confirm the moved file's
/// content is still exactly `expected_text` — the same content `discover`/`require_installed`
/// classified as ours. If it is not (or the path no longer exists at all), the backup is restored to
/// where it came from and this returns `Ok(None)`: something changed after classification, which the
/// caller must treat as a race, not a license to keep going. On a match, `Ok(Some(backup_path))` — the
/// verified former occupant, now safely out of the way for the caller to finish with (replace it with
/// new content, or delete it outright).
///
/// Content, not inode, is the identity checked: a file's inode number can be reused by the kernel
/// moments after it is unlinked, so two genuinely different files can share one. Comparing the actual
/// bytes is both simpler and immune to that, and it is what this whole system already treats as an
/// artifact's identity everywhere else (`decide`'s entire vocabulary is text).
///
/// The quarantined file is classified with the same `O_NOFOLLOW`-then-`fstat` discipline
/// `discover::classify_and_read` uses for the fragment itself, not a plain `read_to_string` — the
/// rename above can just as easily have moved a FIFO, a device node, a directory, or a symlink into
/// quarantine as a regular file, and each of those would otherwise be mishandled by a bare read
/// (a FIFO blocks forever waiting for a writer; a character device can grow the read buffer without
/// bound; a directory fails with `EISDIR`, stranding it under the hidden quarantine name since
/// `restore_quarantine`'s `link` then also fails; a symlink would be silently followed, unlike every
/// other read in this module). None of those is something a fragment `discover` ever classified as
/// `Ours` could have been, so any of them showing up here is itself proof of a race — treated exactly
/// like a content mismatch.
pub(super) fn quarantine_if_still_ours(id: &str, expected_text: &str) -> Result<Option<PathBuf>> {
    let final_path = unit_path(id);
    let backup_path = unique_quarantine_path(id);

    match fs::rename(&final_path, &backup_path) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(io_err("quarantine", &final_path, e)),
    }

    let actual = match classify_and_read(&backup_path) {
        Ok(RawState::Regular(text)) => text,
        Ok(RawState::Absent | RawState::NonRegular) => {
            restore_quarantine(&backup_path, &final_path)?;
            return Ok(None);
        }
        // Whatever went wrong reading it back, the file is real and must not be stranded under a
        // hidden name — restore it before reporting the read failure.
        Err(e) => return Err(restore_or_chain(&backup_path, &final_path, e)),
    };
    if actual == expected_text {
        Ok(Some(backup_path))
    } else {
        restore_quarantine(&backup_path, &final_path)?;
        Ok(None)
    }
}

/// A quarantine name unique to this attempt, so two quarantines for the same id — a prior attempt
/// left one behind, or a genuinely concurrent one — never collide. A monotonic per-process counter
/// combined with the PID is unique across both this process's own repeated attempts and any other
/// process running concurrently.
fn unique_quarantine_path(id: &str) -> PathBuf {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    Path::new(UNIT_DIR).join(format!(
        ".{id}.service.goetia-quarantine.{pid:x}-{n:x}",
        pid = std::process::id()
    ))
}

/// Move the quarantined `backup_path` back to `final_path`, without clobbering anything that might
/// now be there — the restore needs the exact same non-clobber guarantee as any other write in this
/// module (obligation 1). `link` rather than `rename`, deliberately: unlike `rename(2)`, `link(2)`
/// fails rather than replacing an existing destination. On success `backup_path` no longer exists; on
/// failure because something now occupies `final_path`, `backup_path` is left in place (still holding
/// the original content) and the returned error names it, so the situation is diagnosable rather than
/// silently destructive either way.
pub(super) fn restore_quarantine(backup_path: &Path, final_path: &Path) -> Result<()> {
    match fs::hard_link(backup_path, final_path) {
        Ok(()) => {
            remove_file_if_present(backup_path)?;
            Ok(())
        }
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => Err(Error::Other(format!(
            "could not restore the fragment quarantined at {}: a different file now occupies {}; \
             resolve manually",
            backup_path.display(),
            final_path.display(),
        ))),
        Err(e) => Err(io_err("restore", final_path, e)),
    }
}

// write_temp_unit / fsync_unit_dir ====================================================================================

/// A temp file in `UNIT_DIR` itself (so the later `persist`/`persist_noclobber` is a same-filesystem
/// link, never a cross-device copy), containing `text`, already `chmod`ed 0644 (obligation 4) and
/// `fsync`ed to stable storage — `NamedTempFile`'s `Write` impl forwards `flush()` straight to
/// `std::fs::File`, whose `flush` is a documented no-op, so without a real `sync_all()` here a crash
/// between this write and the later rename can leave the fragment on disk fully or partially
/// zero-filled: no `[X-Goetia]` marker, `extract` returns `Ok(None)`, and every verb then refuses an
/// id goetia itself created.
pub(super) fn write_temp_unit(id: &str, text: &str) -> Result<tempfile::NamedTempFile> {
    let dir = Path::new(UNIT_DIR);
    let mut tmp = tempfile::Builder::new()
        .prefix(&format!(".goetia-{id}-"))
        .suffix(".service.tmp")
        .tempfile_in(dir)
        .map_err(|e| io_err("create a temp file in", dir, e))?;
    tmp.write_all(text.as_bytes())
        .map_err(|e| io_err("write", tmp.path(), e))?;
    tmp.as_file().sync_all().map_err(|e| io_err("fsync", tmp.path(), e))?;
    tmp.as_file()
        .set_permissions(fs::Permissions::from_mode(0o644))
        .map_err(|e| io_err("chmod", tmp.path(), e))?;
    Ok(tmp)
}

/// `fsync`s `UNIT_DIR` itself, so a just-completed rename's new directory entry is durable — a file's
/// own `fsync` (see `write_temp_unit`) says nothing about the directory entry pointing to it.
pub(super) fn fsync_unit_dir() -> Result<()> {
    let dir = Path::new(UNIT_DIR);
    let f = fs::File::open(dir).map_err(|e| io_err("open", dir, e))?;
    f.sync_all().map_err(|e| io_err("fsync", dir, e))
}
