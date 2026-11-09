//! The directories an install creates for a spec's `cwd` and `logs` before it writes the
//! definition, taken back when the install does not leave that definition in place.

use std::collections::VecDeque;
use std::ffi::{OsStr, OsString};
use std::io::{self, Write as _};
use std::os::fd::{AsFd as _, BorrowedFd, OwnedFd};
use std::path::{Component, Path, PathBuf};

use nix::errno::Errno;
use nix::fcntl::{AtFlags, OFlag, open, openat, readlinkat};
use nix::sys::stat::{FileStat, Mode, SFlag, fchmod, fstat, fstatat, mkdirat};
use nix::unistd::{Gid, Uid, UnlinkatFlags, fchown, unlinkat};

use crate::error::{Error, Result};

/// The directories one `write` has created, removed when it is dropped unless
/// [`settle`](Self::settle) kept them. Make it before the first `create`.
///
/// Root creates these directories and hands them to the daemon's account, which can then swap one
/// for a symlink. So every step on a directory goes by descriptor, never by path: the path to it is
/// resolved component by component ([`walk`]), it is made with `mkdirat` under its parent's
/// descriptor, opened `O_NOFOLLOW` from there, `fchmod`ed and `fchown`ed on that descriptor, and
/// later removed by name from the parent's descriptor (`unlinkat`) only while the name still refers
/// to the same inode. A descriptor stays open per directory until the guard is dropped; running out
/// of them fails the install and leaves the directory being made.
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
    /// A directory this call did not create, as `walk` held it.
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
        self.create_in(walk(dir)?, owner)
    }

    /// Create what `walk` found missing, under the directory it holds open.
    fn create_in(&mut self, walk: Walk, owner: Option<Owner>) -> Result<()> {
        let Walk { holder, missing } = walk;
        let mut parent = Parent::Existing(holder);
        for (n, (path, name)) in missing.iter().enumerate() {
            // The mode given to `mkdirat` is masked by the umask; the `fchmod` below makes it
            // exactly `0755`. Until then it is at most that, never wider.
            match mkdirat(self.fd_of(&parent), name.as_os_str(), Mode::from_bits_truncate(0o755)) {
                Ok(()) => {
                    let opened = open_beneath(self.fd_of(&parent), name, OFlag::O_RDONLY)
                        .and_then(|dir| fstat(&dir).map(|stat| (dir, stat)));
                    let (dir, FileStat { st_dev, st_ino, .. }) = opened.map_err(|e| {
                        report(format_args!(
                            "could not remove {}, which the failed install created: it could not be opened: {e}",
                            path.display()
                        ));
                        Error::os_at("open", path, e)
                    })?;
                    self.0.push(Created {
                        path: path.clone(),
                        name: name.clone(),
                        parent,
                        dir,
                        dev: st_dev,
                        ino: st_ino,
                    });
                    let index = self.0.len() - 1;
                    parent = Parent::Created(index);
                    let dir = self.0[index].dir.as_fd();
                    fchmod(dir, Mode::from_bits_truncate(0o755)).map_err(|e| Error::os_at("chmod", path, e))?;
                    if let Some(owner) = owner {
                        fchown(dir, Some(owner.uid), owner.gid).map_err(|e| Error::os_at("chown", path, e))?;
                    }
                }
                // Another install created it first: not ours, so left as it is.
                Err(Errno::EEXIST) if n + 1 == missing.len() => return Ok(()),
                Err(Errno::EEXIST) => {
                    let held =
                        open_beneath(self.fd_of(&parent), name, SEARCH).map_err(|e| Error::os_at("open", path, e))?;
                    parent = Parent::Existing(held);
                }
                Err(e) => return Err(Error::os_at("create directory", path, e)),
            }
        }
        Ok(())
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

/// What a walk of a path found: the deepest directory that exists, held open, and the components
/// below it that do not, each with its path, outermost first.
struct Walk {
    holder: OwnedFd,
    missing: Vec<(PathBuf, OsString)>,
}

/// How a directory is opened to be searched. An `O_PATH` descriptor needs no read permission on the
/// directory and is enough for `openat`, `mkdirat`, `fstatat` and `unlinkat`; macOS has none.
#[cfg(target_os = "linux")]
const SEARCH: OFlag = OFlag::O_PATH;
#[cfg(not(target_os = "linux"))]
const SEARCH: OFlag = OFlag::O_RDONLY;

/// Symlinks one walk follows before it gives up on a cycle: the kernel's own bound.
const SYMLINK_LIMIT: usize = 40;

/// Resolve `dir` from `/` one component at a time, on descriptors, following the symlinks that
/// lead through it and none that is the last component. A step from an entry owned by a non-root
/// uid to one owned by a different uid, a symlink included and judged by its own owner, is refused:
/// the rule of systemd's `CHASE_SAFE`, so a symlink the daemon's account plants cannot lead root
/// into a directory the account does not own.
fn walk(dir: &Path) -> Result<Walk> {
    let search = OFlag::O_DIRECTORY | SEARCH | OFlag::O_CLOEXEC;
    let mut so_far = PathBuf::new();
    let mut cur = open(if dir.has_root() { "/" } else { "." }, search, Mode::empty())
        .map_err(|e| Error::os_at("open", dir, e))?;
    if dir.has_root() {
        so_far.push("/");
    }
    let mut owner = fstat(&cur).map_err(|e| Error::os_at("stat", &so_far, e))?.st_uid;
    let mut queue = components(dir, false);
    let mut missing: Vec<(PathBuf, OsString)> = Vec::new();
    let mut links = 0;
    while let Some((name, from_link)) = queue.pop_front() {
        if !from_link {
            so_far.push(&name);
        }
        if !missing.is_empty() {
            if name == ".." {
                return Err(Error::os_at(
                    "create directory",
                    dir,
                    io::Error::from(io::ErrorKind::InvalidInput),
                ));
            }
            missing.push((so_far.clone(), name));
            continue;
        }
        if name == ".." {
            let up = open_beneath(cur.as_fd(), &name, SEARCH).map_err(|e| Error::os_at("open", &so_far, e))?;
            let up_owner = fstat(&up).map_err(|e| Error::os_at("stat", &so_far, e))?.st_uid;
            refuse_unsafe(owner, up_owner, &so_far)?;
            owner = up_owner;
            cur = up;
            continue;
        }
        let entry = match fstatat(cur.as_fd(), name.as_os_str(), AtFlags::AT_SYMLINK_NOFOLLOW) {
            Ok(entry) => entry,
            // Below a symlink that leads nowhere nothing may be made: that would make its target.
            Err(Errno::ENOENT) if from_link => {
                return Err(Error::os_at(
                    "resolve",
                    &so_far,
                    io::Error::from(io::ErrorKind::NotFound),
                ));
            }
            Err(Errno::ENOENT) => {
                missing.push((so_far.clone(), name));
                continue;
            }
            Err(e) => return Err(Error::os_at("stat", &so_far, e)),
        };
        // The last component exists, of whatever kind: not ours to make, so not ours to vouch for.
        if queue.is_empty() {
            return Ok(Walk {
                holder: cur,
                missing: Vec::new(),
            });
        }
        refuse_unsafe(owner, entry.st_uid, &so_far)?;
        let kind = SFlag::from_bits_truncate(entry.st_mode) & SFlag::S_IFMT;
        if kind == SFlag::S_IFDIR {
            cur = open_beneath(cur.as_fd(), &name, SEARCH).map_err(|e| Error::os_at("open", &so_far, e))?;
            owner = entry.st_uid;
        } else if kind == SFlag::S_IFLNK {
            links += 1;
            if links > SYMLINK_LIMIT {
                return Err(Error::os_at(
                    "resolve",
                    &so_far,
                    io::Error::from_raw_os_error(libc::ELOOP),
                ));
            }
            let target =
                readlinkat(cur.as_fd(), name.as_os_str()).map_err(|e| Error::os_at("read link", &so_far, e))?;
            if Path::new(&target).has_root() {
                cur = open("/", search, Mode::empty()).map_err(|e| Error::os_at("open", &so_far, e))?;
            }
            // What the link leads to is judged from the link's owner.
            owner = entry.st_uid;
            for step in components(Path::new(&target), true).into_iter().rev() {
                queue.push_front(step);
            }
        } else {
            return Err(Error::os_at("stat", &so_far, Errno::ENOTDIR));
        }
    }
    Ok(Walk { holder: cur, missing })
}

/// The steps of `path`, as names and `..`, the root and `.` left out.
fn components(path: &Path, from_link: bool) -> VecDeque<(OsString, bool)> {
    path.components()
        .filter_map(|component| match component {
            Component::ParentDir => Some(OsString::from("..")),
            Component::Normal(name) => Some(name.to_owned()),
            Component::RootDir | Component::CurDir | Component::Prefix(_) => None,
        })
        .map(|name| (name, from_link))
        .collect()
}

/// Refuse to go on from an entry owned by `from` to one owned by `to`, unless `from` is root or
/// `to` itself.
fn refuse_unsafe(from: libc::uid_t, to: libc::uid_t, at: &Path) -> Result<()> {
    if from == 0 || from == to {
        return Ok(());
    }
    Err(Error::os_at(
        "resolve",
        at,
        io::Error::other(format!(
            "not followed: it leads from an entry owned by uid {from} to one owned by uid {to}"
        )),
    ))
}

/// `name` under `parent`, as a directory opened with `base`: a symlink there is an error, never
/// followed.
fn open_beneath(parent: BorrowedFd<'_>, name: &OsStr, base: OFlag) -> nix::Result<OwnedFd> {
    openat(
        parent,
        name,
        base | OFlag::O_DIRECTORY | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC,
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
