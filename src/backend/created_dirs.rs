//! The directories an install creates for a spec's `cwd` and `logs` before it writes the
//! definition, taken back when the install does not leave that definition in place.

use std::ffi::OsString;
use std::io::{self, Write as _};
use std::os::fd::{AsFd as _, OwnedFd};
use std::path::{Path, PathBuf};

use nix::errno::Errno;
use nix::fcntl::{AtFlags, OFlag, openat};
use nix::sys::stat::{FileStat, Mode, fstat, fstatat};
use nix::unistd::{UnlinkatFlags, unlinkat};

/// The directories one `write` has created, removed when it is dropped unless
/// [`settle`](Self::settle) kept them. Make it before the first `mkdir`.
///
/// The drop runs as root over directories that were handed to the daemon's account, which can swap
/// one for a symlink. So each directory is held by an open descriptor from the moment it is created,
/// and removed by name from its parent's descriptor (`unlinkat`), never by path, and only while the
/// name still refers to the directory that was created.
#[derive(Debug, Default)]
pub(crate) struct CreatedDirs(Vec<Created>);

/// Whether the definition an install wrote is in place, as its backend's guarded read found it.
#[derive(Debug)]
pub(crate) enum Standing {
    Yes,
    No,
    /// The read did not complete, for this reason.
    Unknown(String),
}

#[derive(Debug)]
struct Created {
    path: PathBuf,
    name: OsString,
    parent: Parent,
    /// The directory itself, opened without following a symlink straight after its `mkdir`.
    dir: OwnedFd,
    dev: libc::dev_t,
    ino: libc::ino_t,
}

/// What a created directory is removed from.
#[derive(Debug)]
enum Parent {
    /// An earlier entry's directory, by index.
    Created(usize),
    /// A directory that existed before the install.
    Existing(OwnedFd),
}

impl CreatedDirs {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Note `dir`, which this install just created. Called straight after the `mkdir`, so a later
    /// step on `dir` that fails (a `chmod`, a `chown`) is undone with it. A `dir` that cannot be
    /// opened as the directory it is stays, reported.
    pub(crate) fn record(&mut self, dir: &Path) {
        if let Err(e) = self.try_record(dir) {
            report(format_args!(
                "could not track {}, which the install created, so it stays: {e}",
                dir.display()
            ));
        }
    }

    fn try_record(&mut self, dir: &Path) -> io::Result<()> {
        let invalid = || io::Error::from(io::ErrorKind::InvalidInput);
        let name = dir.file_name().ok_or_else(invalid)?;
        let parent_path = dir.parent().ok_or_else(invalid)?;
        let parent = match self.0.iter().position(|created| created.path == parent_path) {
            Some(index) => Parent::Created(index),
            None => Parent::Existing(open_dir(parent_path)?),
        };
        let parent_fd = match &parent {
            Parent::Created(index) => self.0[*index].dir.as_fd(),
            Parent::Existing(fd) => fd.as_fd(),
        };
        let fd = openat(
            parent_fd,
            name,
            OFlag::O_RDONLY | OFlag::O_DIRECTORY | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC,
            Mode::empty(),
        )?;
        let FileStat { st_dev, st_ino, .. } = fstat(&fd)?;
        self.0.push(Created {
            path: dir.to_path_buf(),
            name: name.to_owned(),
            parent,
            dir: fd,
            dev: st_dev,
            ino: st_ino,
        });
        Ok(())
    }

    /// Keep every recorded directory unless the definition is not in place. An unknown answer keeps
    /// them and says so: removing them could break an installed definition.
    pub(crate) fn settle(&mut self, definition: &Path, standing: Standing) {
        match standing {
            Standing::Yes => self.0.clear(),
            Standing::No => {}
            Standing::Unknown(e) => {
                if !self.0.is_empty() {
                    report(format_args!(
                        "could not read {}: {e}; keeping the directories the install created",
                        definition.display()
                    ));
                }
                self.0.clear();
            }
        }
    }

    /// Remove `self.0[index]` from its parent if the name still refers to the directory that was
    /// created. A directory that is gone, or has gained content, is left without a word.
    fn remove(&self, index: usize) -> Result<(), String> {
        let created = &self.0[index];
        let parent = match &created.parent {
            Parent::Created(parent) => self.0[*parent].dir.as_fd(),
            Parent::Existing(fd) => fd.as_fd(),
        };
        match fstatat(parent, created.name.as_os_str(), AtFlags::AT_SYMLINK_NOFOLLOW) {
            Ok(now) if now.st_dev == created.dev && now.st_ino == created.ino => {}
            Ok(_) => return Err("it is no longer the directory the install created".to_string()),
            Err(Errno::ENOENT) => return Ok(()),
            Err(e) => return Err(e.to_string()),
        }
        match unlinkat(parent, created.name.as_os_str(), UnlinkatFlags::RemoveDir) {
            Ok(()) | Err(Errno::ENOENT | Errno::ENOTEMPTY | Errno::EEXIST) => Ok(()),
            Err(e) => Err(e.to_string()),
        }
    }
}

/// A directory that existed before the install, opened for use as a parent.
fn open_dir(path: &Path) -> io::Result<OwnedFd> {
    Ok(nix::fcntl::open(
        path,
        OFlag::O_RDONLY | OFlag::O_DIRECTORY | OFlag::O_CLOEXEC,
        Mode::empty(),
    )?)
}

impl Drop for CreatedDirs {
    /// Deepest first, since each directory was created after its parent. Does nothing while a panic
    /// unwinds: the panic may be after the definition was written, and the directories of an
    /// installed service must stay. A removal that fails is reported on stderr, and a failed report
    /// is ignored: this never panics.
    fn drop(&mut self) {
        if std::thread::panicking() {
            return;
        }
        for index in (0..self.0.len()).rev() {
            if let Err(e) = self.remove(index) {
                report(format_args!(
                    "could not remove {}, which the failed install created: {e}",
                    self.0[index].path.display()
                ));
            }
        }
    }
}

/// A warning on stderr. A failed write is ignored: `eprintln!` would panic on one.
fn report(message: std::fmt::Arguments<'_>) {
    let _ = writeln!(io::stderr(), "warning: {message}");
}

#[cfg(test)]
#[path = "created_dirs_tests.rs"]
mod created_dirs_tests;
