//! The directories an install creates for a spec's `cwd` and `logs` before it writes the
//! definition, taken back when the install does not leave that definition in place.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

/// The directories one `write` has created, removed when it is dropped unless
/// [`keep_if_in_place`](Self::keep_if_in_place) kept them. Register it before the first `mkdir`.
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

    /// Keep every recorded directory if `path` holds `definition`: the install's own definition,
    /// which needs them, stands. A `path` that is gone or holds something else (the install failed
    /// before writing, put its write back, or lost a race to another definition) leaves them
    /// recorded for removal. A `path` that cannot be read says nothing, and the directories stay.
    pub(crate) fn keep_if_in_place(&mut self, path: &Path, definition: &str) {
        let in_place = match fs::read_to_string(path) {
            Ok(text) => text == definition,
            Err(e) => !matches!(e.kind(), io::ErrorKind::NotFound | io::ErrorKind::InvalidData),
        };
        if in_place {
            self.0.clear();
        }
    }
}

impl Drop for CreatedDirs {
    /// Deepest first, since each directory was created after its parent. `remove_dir` takes only an
    /// empty directory, so one that has since gained content stays, and so does everything above it.
    fn drop(&mut self) {
        for dir in self.0.drain(..).rev() {
            match fs::remove_dir(&dir) {
                Ok(()) => {}
                Err(e) if matches!(e.kind(), io::ErrorKind::NotFound | io::ErrorKind::DirectoryNotEmpty) => {}
                Err(e) => eprintln!(
                    "warning: could not remove {}, which the failed install created: {e}",
                    dir.display()
                ),
            }
        }
    }
}

#[cfg(test)]
#[path = "created_dirs_tests.rs"]
mod created_dirs_tests;
