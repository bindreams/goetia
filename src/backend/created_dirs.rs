//! The directories an install creates for a spec's `cwd` and `logs` before it writes the
//! definition, taken back when the install does not leave that definition in place.

use std::collections::VecDeque;
use std::ffi::{OsStr, OsString};
use std::io::{self, Write as _};
use std::os::fd::{AsFd as _, BorrowedFd, OwnedFd};
use std::path::{Component, Path, PathBuf};

use nix::errno::Errno;
use nix::fcntl::readlinkat;
use nix::fcntl::{AtFlags, OFlag, open, openat};
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
///
/// The resolution follows systemd's `CHASE_SAFE`, on every step and plain directories included:
/// going on from an entry owned by a non-root uid to one owned by a different uid is refused. A path
/// that steps between owners (an account's directory holding one root or another account owns) is
/// therefore refused, not created in.
#[derive(Debug, Default)]
pub(crate) struct CreatedDirs(Vec<Created>);

/// Whom a created directory is handed to.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Owner {
    pub(crate) uid: Uid,
    /// `None` leaves the group as the creating process's.
    pub(crate) gid: Option<Gid>,
}

impl Owner {
    /// `(uid_t)-1` and `(gid_t)-1` mean "unchanged" to `fchown`: no account has either.
    pub(crate) fn new(uid: Uid, gid: Option<Gid>) -> Result<Self> {
        let unchanged = |id: u32| id == u32::MAX;
        if unchanged(uid.as_raw()) || gid.is_some_and(|gid| unchanged(gid.as_raw())) {
            return Err(Error::AccountLookup {
                detail: format!("id {} names no account: `chown` reads it as \"unchanged\"", u32::MAX),
                source: None,
            });
        }
        Ok(Self { uid, gid })
    }
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

/// How a directory is opened to be searched. On Linux an `O_PATH` descriptor needs no read
/// permission on the directory and is enough for `openat`, `mkdirat`, `fstatat` and `unlinkat`. macOS
/// has no `O_PATH`; `O_SEARCH` (`O_EXEC | O_DIRECTORY`) is its open for search only.
#[cfg(target_os = "linux")]
const SEARCH: OFlag = OFlag::O_PATH;
#[cfg(not(target_os = "linux"))]
const SEARCH: OFlag = OFlag::O_SEARCH;

/// Symlinks one walk follows before it gives up on a cycle: the kernel's own bound, Linux's
/// `MAXSYMLINKS`, and macOS's.
#[cfg(target_os = "linux")]
const SYMLINK_LIMIT: usize = 40;
#[cfg(not(target_os = "linux"))]
const SYMLINK_LIMIT: usize = 32;

/// What a directory entry is, read through a descriptor the walk then holds, so that what was judged
/// is what is used.
enum Entry {
    /// A directory, and the descriptor it was read through.
    Dir(OwnedFd, FileStat),
    /// A symlink and where it points.
    Link(FileStat, OsString),
    Other(FileStat),
}

impl Entry {
    fn uid(&self) -> libc::uid_t {
        match self {
            Entry::Dir(_, stat) | Entry::Link(stat, _) | Entry::Other(stat) => stat.st_uid,
        }
    }
}

/// `name` in `parent`, not followed: `None` if there is none. As systemd's `chase` does on Linux, the
/// entry is opened `O_PATH | O_NOFOLLOW` and judged by `fstat` of that descriptor, which for a link is
/// the link itself, and a link is read through the same descriptor: the entry that was judged is the
/// entry that is used, however the name is renamed meanwhile.
#[cfg(target_os = "linux")]
fn entry(parent: BorrowedFd<'_>, name: &OsStr) -> nix::Result<Option<Entry>> {
    let fd = match openat(
        parent,
        name,
        OFlag::O_PATH | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC,
        Mode::empty(),
    ) {
        Ok(fd) => fd,
        Err(Errno::ENOENT) => return Ok(None),
        Err(e) => return Err(e),
    };
    let stat = fstat(&fd)?;
    Ok(Some(match SFlag::from_bits_truncate(stat.st_mode) & SFlag::S_IFMT {
        SFlag::S_IFDIR => Entry::Dir(fd, stat),
        // An empty path reads the link the descriptor is on (Linux 2.6.39).
        SFlag::S_IFLNK => Entry::Link(stat, readlinkat(&fd, "")?),
        _ => Entry::Other(stat),
    }))
}

/// `name` in `parent`, not followed: `None` if there is none. macOS cannot open a symlink as a
/// descriptor to read it through (`O_SYMLINK` opens it, but it cannot be read from the descriptor),
/// so a directory is opened `O_SEARCH | O_NOFOLLOW` and judged by `fstat` of that descriptor, which
/// must be the inode the name had; a link is read by name and the name must still be the link that
/// was judged afterwards. A change in between is an error, never a guess.
#[cfg(not(target_os = "linux"))]
fn entry(parent: BorrowedFd<'_>, name: &OsStr) -> nix::Result<Option<Entry>> {
    let named = match fstatat(parent, name, AtFlags::AT_SYMLINK_NOFOLLOW) {
        Ok(stat) => stat,
        Err(Errno::ENOENT) => return Ok(None),
        Err(e) => return Err(e),
    };
    let same = |a: &FileStat, b: &FileStat| a.st_dev == b.st_dev && a.st_ino == b.st_ino;
    match SFlag::from_bits_truncate(named.st_mode) & SFlag::S_IFMT {
        SFlag::S_IFDIR => {
            let fd = openat(
                parent,
                name,
                SEARCH | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC,
                Mode::empty(),
            )?;
            let held = fstat(&fd)?;
            if !same(&named, &held) {
                return Err(Errno::EAGAIN);
            }
            Ok(Some(Entry::Dir(fd, held)))
        }
        SFlag::S_IFLNK => {
            let target = readlinkat(parent, name)?;
            match fstatat(parent, name, AtFlags::AT_SYMLINK_NOFOLLOW) {
                Ok(again) if same(&named, &again) => Ok(Some(Entry::Link(named, target))),
                _ => Err(Errno::EAGAIN),
            }
        }
        _ => Ok(Some(Entry::Other(named))),
    }
}

/// Resolve `dir`, which must be absolute, from `/` one component at a time, on descriptors,
/// following the symlinks that lead through it and none that is the last component. As in systemd's
/// `CHASE_SAFE`, going on from an entry owned by a non-root uid to one owned by a different uid is
/// refused at every step, plain directories and the last existing component included, and a link
/// carries its own owner onto whatever it leads to: so a symlink the daemon's account plants cannot
/// lead root into a directory the account does not own. An absolute link is judged against `/`,
/// which root owns, and is followed only from root's.
fn walk(dir: &Path) -> Result<Walk> {
    if !dir.has_root() {
        return Err(Error::os_at(
            "create directory",
            dir,
            io::Error::other("not an absolute path"),
        ));
    }
    let search = OFlag::O_DIRECTORY | SEARCH | OFlag::O_CLOEXEC;
    let root = || open("/", search, Mode::empty()).map_err(|e| Error::os_at("open", "/", e));
    let mut cur = root()?;
    let mut owner = fstat(&cur).map_err(|e| Error::os_at("stat", "/", e))?.st_uid;
    // The link `owner` was carried from, if the walk has not entered a directory since.
    let mut carried: Option<PathBuf> = None;
    // Where `cur` is, as far as the walk can tell, for naming what it refuses.
    let mut shown = PathBuf::from("/");
    // The requested path up to the component being taken, which is what is created.
    let mut so_far = PathBuf::from("/");
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
                    &so_far,
                    io::Error::other("`..` below a component that does not exist"),
                ));
            }
            missing.push((so_far.clone(), name));
            continue;
        }
        let at = shown.join(&name);
        if name == ".." {
            let up = open_beneath(cur.as_fd(), &name, SEARCH).map_err(|e| Error::os_at("open", &at, e))?;
            let up_owner = fstat(&up).map_err(|e| Error::os_at("stat", &at, e))?.st_uid;
            refuse_unsafe(owner, up_owner, &at, carried.as_deref())?;
            owner = up_owner;
            carried = None;
            shown.pop();
            cur = up;
            continue;
        }
        let Some(found) = entry(cur.as_fd(), &name).map_err(|e| Error::os_at("open", &at, e))? else {
            // Below a symlink that leads nowhere nothing may be made: that would make its target.
            if from_link {
                return Err(Error::os_at("resolve", &at, io::Error::from(io::ErrorKind::NotFound)));
            }
            missing.push((so_far.clone(), name));
            continue;
        };
        let uid = found.uid();
        refuse_unsafe(owner, uid, &at, carried.as_deref())?;
        // The last component exists, of whatever kind: not ours to make.
        if queue.is_empty() {
            return Ok(Walk {
                holder: cur,
                missing: Vec::new(),
            });
        }
        match found {
            Entry::Dir(fd, _) => {
                cur = fd;
                owner = uid;
                carried = None;
                shown = at;
            }
            Entry::Link(_, target) => {
                links += 1;
                if links > SYMLINK_LIMIT {
                    return Err(Error::os_at("resolve", &at, io::Error::from_raw_os_error(libc::ELOOP)));
                }
                if Path::new(&target).has_root() {
                    cur = root()?;
                    let root_owner = fstat(&cur).map_err(|e| Error::os_at("stat", "/", e))?.st_uid;
                    refuse_unsafe(uid, root_owner, &at, None)?;
                    owner = root_owner;
                    carried = None;
                    shown = PathBuf::from("/");
                } else {
                    owner = uid;
                    carried = Some(at);
                }
                for step in components(Path::new(&target), true).into_iter().rev() {
                    queue.push_front(step);
                }
            }
            Entry::Other(_) => return Err(Error::os_at("stat", &at, Errno::ENOTDIR)),
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

/// Refuse to go on from an entry owned by `from` to `at`, owned by `to`, unless `from` is root or
/// `to` itself. `carried` is the link `from` is the owner of, when the walk has not entered a
/// directory since.
fn refuse_unsafe(from: libc::uid_t, to: libc::uid_t, at: &Path, carried: Option<&Path>) -> Result<()> {
    if from == 0 || from == to {
        return Ok(());
    }
    let carried = carried
        .map(|link| format!(", carried from link {}", link.display()))
        .unwrap_or_default();
    Err(Error::os_at(
        "resolve",
        at,
        io::Error::other(format!(
            "not followed: owned by uid {to}, and the walk comes from uid {from}{carried}"
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
