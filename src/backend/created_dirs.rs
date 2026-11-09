//! The directories an install creates for a spec's `cwd` and `logs` before it writes the
//! definition, taken back when the install does not leave that definition in place.

use std::fs;
use std::io::{self, Write as _};
use std::path::{Path, PathBuf};

/// The directories one `write` has created, removed when it is dropped unless [`keep`](Self::keep)
/// was called. Make it before the first `mkdir`.
///
/// Whether the definition stands is the backend's to say, through its own guarded read of the
/// definition's path; this only acts on the answer.
#[derive(Debug, Default)]
pub(crate) struct CreatedDirs(Vec<PathBuf>);

impl CreatedDirs {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Note `dir`, which this install just created. Called straight after the `mkdir`, so a later
    /// step on `dir` that fails (a `chmod`, a `chown`) is undone with it.
    pub(crate) fn record(&mut self, dir: &Path) {
        self.0.push(dir.to_path_buf());
    }

    /// Keep every recorded directory: the install's definition is in place and needs them.
    pub(crate) fn keep(&mut self) {
        self.0.clear();
    }
}

impl Drop for CreatedDirs {
    /// Deepest first, since each directory was created after its parent. `remove_dir` takes only an
    /// empty directory, so one that has since gained content stays, and so does everything above it.
    ///
    /// Does nothing while a panic unwinds: the panic may be after the definition was written, and
    /// the directories of an installed service must stay. A removal that fails is reported on
    /// stderr, and a failed report is ignored: this never panics.
    fn drop(&mut self) {
        if std::thread::panicking() {
            return;
        }
        for dir in self.0.drain(..).rev() {
            match fs::remove_dir(&dir) {
                Ok(()) => {}
                Err(e) if matches!(e.kind(), io::ErrorKind::NotFound | io::ErrorKind::DirectoryNotEmpty) => {}
                Err(e) => {
                    let _ = writeln!(
                        io::stderr(),
                        "warning: could not remove {}, which the failed install created: {e}",
                        dir.display()
                    );
                }
            }
        }
    }
}

#[cfg(test)]
#[path = "created_dirs_tests.rs"]
mod created_dirs_tests;
