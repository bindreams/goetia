//! The directories an install creates for a spec's `cwd` and `logs` before it writes the
//! definition, taken back when the install does not leave that definition in place.

use std::ffi::OsString;
use std::io::{self, Write as _};
use std::os::fd::{AsFd as _, BorrowedFd, OwnedFd};
use std::path::{Path, PathBuf};

use nix::errno::Errno;
use nix::fcntl::{AtFlags, OFlag, openat};
use nix::sys::stat::{FileStat, Mode, fchmod, fstat, fstatat, mkdirat};
use nix::unistd::{Gid, Uid, UnlinkatFlags, fchown, unlinkat};

use crate::error::{Error, Result};

/// The directories one `write` has created, removed when it is dropped unless
/// [`settle`](Self::settle) kept them. Make it before the first `create`.
///
/// Root creates these directories and hands them to the daemon's account, which can then swap one
/// for a symlink. So every step on a directory goes by descriptor, never by path: it is made with
/// `mkdirat` under its parent's descriptor, opened `O_NOFOLLOW` from there, `fchmod`ed and
/// `fchown`ed on that descriptor, and later removed by name from the parent's descriptor
/// (`unlinkat`) only while the name still refers to the same inode. A descriptor stays open per
/// directory until the guard is dropped; running out of them fails the install and leaves the
/// directory being made.
#[derive(Debug, Default)]
pub(crate) struct CreatedDirs(Vec<Created>);

/// Whom a created directory is handed to.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Owner {
    pub(crate) uid: Uid,
    /// `None` leaves the group as the creating process's.
    pub(crate) gid: Option<Gid>,
}

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
    /// The directory itself, opened without following a symlink straight after its `mkdirat`.
    dir: OwnedFd,
    dev: libc::dev_t,
    ino: libc::ino_t,
}

/// What a directory is created in, and removed from.
#[derive(Debug)]
enum Parent {
    /// An earlier entry's directory, by index.
    Created(usize),
    /// A directory this install did not create.
    Existing(OwnedFd),
}

impl CreatedDirs {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Create `dir` and every missing ancestor, each `0755` whatever the umask and owned by `owner`.
    /// A path that exists, of any kind (a dangling symlink included), is left untouched, and so is
    /// one another install creates after the walk: not ours.
    pub(crate) fn create(&mut self, dir: &Path, owner: Option<Owner>) -> Result<()> {
        self.create_missing(&missing_components(dir)?, owner)
    }

    /// Create `missing`, outermost first, under the directory that holds the first of them.
    fn create_missing(&mut self, missing: &[&Path], owner: Option<Owner>) -> Result<()> {
        let Some(first) = missing.first() else {
            return Ok(());
        };
        let mut parent = self.holder_of(first)?;
        for (n, path) in missing.iter().enumerate() {
            let name = path
                .file_name()
                .ok_or_else(|| Error::os_at("create directory", *path, io::Error::from(io::ErrorKind::InvalidInput)))?;
            // The mode given to `mkdirat` is masked by the umask; the `fchmod` below makes it
            // exactly `0755`. Until then it is at most that, never wider.
            match mkdirat(self.fd_of(&parent), name, Mode::from_bits_truncate(0o755)) {
                Ok(()) => {
                    let dir = open_beneath(self.fd_of(&parent), name).map_err(|e| Error::os_at("open", *path, e))?;
                    let FileStat { st_dev, st_ino, .. } = fstat(&dir).map_err(|e| Error::os_at("stat", *path, e))?;
                    self.0.push(Created {
                        path: path.to_path_buf(),
                        name: name.to_owned(),
                        parent,
                        dir,
                        dev: st_dev,
                        ino: st_ino,
                    });
                    let index = self.0.len() - 1;
                    parent = Parent::Created(index);
                    let dir = self.0[index].dir.as_fd();
                    fchmod(dir, Mode::from_bits_truncate(0o755)).map_err(|e| Error::os_at("chmod", *path, e))?;
                    if let Some(owner) = owner {
                        fchown(dir, Some(owner.uid), owner.gid).map_err(|e| Error::os_at("chown", *path, e))?;
                    }
                }
                // Another install created it first: not ours, so left as it is.
                Err(Errno::EEXIST) if n + 1 == missing.len() => return Ok(()),
                Err(Errno::EEXIST) => {
                    let held = open_beneath(self.fd_of(&parent), name).map_err(|e| Error::os_at("open", *path, e))?;
                    parent = Parent::Existing(held);
                }
                Err(e) => return Err(Error::os_at("create directory", *path, e)),
            }
        }
        Ok(())
    }

    /// The directory that holds `first`, opened before anything is created: this install's own
    /// directory when it made it, otherwise one that existed before it ran.
    fn holder_of(&self, first: &Path) -> Result<Parent> {
        let holder = match first.parent() {
            Some(parent) if !parent.as_os_str().is_empty() => parent,
            _ => Path::new("."),
        };
        if let Some(index) = self.0.iter().position(|created| created.path == holder) {
            return Ok(Parent::Created(index));
        }
        // By path, following a final symlink: it existed before goetia ran, so it is no account's
        // doing, and `/var/lock`-style symlinked parents stay usable.
        let fd = nix::fcntl::open(
            holder,
            OFlag::O_RDONLY | OFlag::O_DIRECTORY | OFlag::O_CLOEXEC,
            Mode::empty(),
        )
        .map_err(|e| Error::os_at("open", holder, e))?;
        Ok(Parent::Existing(fd))
    }

    fn fd_of<'a>(&'a self, parent: &'a Parent) -> BorrowedFd<'a> {
        match parent {
            Parent::Created(index) => self.0[*index].dir.as_fd(),
            Parent::Existing(fd) => fd.as_fd(),
        }
    }

    /// Keep every created directory unless the definition is not in place. An unknown answer keeps
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
    fn remove(&self, index: usize) -> std::result::Result<(), String> {
        let created = &self.0[index];
        let parent = self.fd_of(&created.parent);
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

/// The components of `dir` that do not exist, outermost first. A dangling symlink exists.
pub(crate) fn missing_components(dir: &Path) -> Result<Vec<&Path>> {
    let mut missing = Vec::new();
    let mut cursor = dir;
    loop {
        match std::fs::symlink_metadata(cursor) {
            Ok(_) => break,
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                missing.push(cursor);
                match cursor.parent() {
                    Some(parent) if !parent.as_os_str().is_empty() => cursor = parent,
                    _ => break,
                }
            }
            Err(e) => return Err(Error::os_at("stat", cursor, e)),
        }
    }
    missing.reverse();
    Ok(missing)
}

/// `name` under `parent`, as a directory: a symlink there is an error, never followed.
fn open_beneath(parent: BorrowedFd<'_>, name: &std::ffi::OsStr) -> nix::Result<OwnedFd> {
    openat(
        parent,
        name,
        OFlag::O_RDONLY | OFlag::O_DIRECTORY | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC,
        Mode::empty(),
    )
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
