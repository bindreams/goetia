//! Resolving a path from `/` on descriptors, one component at a time, and reporting what was passed
//! through. What is judged from it (that a directory may be made there, that a log may be written
//! there) is the caller's; the walk judges nothing itself, so that a path that exists whole is
//! resolved the same way a path that does not is.

use std::collections::VecDeque;
use std::ffi::{OsStr, OsString};
use std::io;
use std::os::fd::{AsFd as _, BorrowedFd, OwnedFd};
use std::path::{Component, Path, PathBuf};

use nix::errno::Errno;
use nix::fcntl::{AtFlags, OFlag, open, openat, readlinkat};
use nix::sys::stat::{FileStat, Mode, SFlag, fstat, fstatat};

use crate::error::{Error, Result};

/// What a walk found.
pub(super) struct Walk {
    pub(super) end: End,
    /// Every directory entered and every symlink followed, in order, with what the walk read of each.
    pub(super) trail: Vec<Step>,
    /// Whether one of the roots is among the directories the path resolved through above its first
    /// missing component, by device and inode: `..` and symlinks are resolved, never compared as text.
    pub(super) under_root: bool,
}

pub(super) enum End {
    /// The path exists and is a directory, or a symlink to one: that directory.
    Whole(FileStat),
    /// The deepest directory that exists, held open, and the components below it that do not, each
    /// with its path, outermost first.
    Missing {
        holder: OwnedFd,
        missing: Vec<(PathBuf, OsString)>,
    },
}

/// A directory the walk entered, or a symlink it followed, as it read it.
pub(super) struct Step {
    pub(super) at: PathBuf,
    pub(super) stat: FileStat,
    pub(super) link: bool,
}

/// How a directory is opened to be searched. On Linux an `O_PATH` descriptor needs no read
/// permission on the directory and is enough for `openat`, `mkdirat`, `fstatat` and `unlinkat`. macOS
/// has no `O_PATH`; `O_SEARCH` (`O_EXEC | O_DIRECTORY`) is its open for search only.
#[cfg(target_os = "linux")]
pub(super) const SEARCH: OFlag = OFlag::O_PATH;
#[cfg(not(target_os = "linux"))]
pub(super) const SEARCH: OFlag = OFlag::O_SEARCH;

/// Symlinks one walk follows before it gives up on a cycle: the kernel's own bound, Linux's
/// `MAXSYMLINKS` (macOS's is 32).
#[cfg(target_os = "linux")]
const SYMLINK_LIMIT: usize = 40;
#[cfg(not(target_os = "linux"))]
const SYMLINK_LIMIT: usize = 32;

/// What a directory entry is, read through a descriptor the walk then holds, so that what was judged
/// is what is used.
pub(super) enum Entry {
    /// A directory, and the descriptor it was read through.
    Dir(OwnedFd, FileStat),
    /// A symlink and where it points.
    Link(FileStat, OsString),
    Other(FileStat),
}

impl Entry {
    pub(super) fn uid(&self) -> libc::uid_t {
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
pub(super) fn entry(parent: BorrowedFd<'_>, name: &OsStr) -> nix::Result<Option<Entry>> {
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
pub(super) fn entry(parent: BorrowedFd<'_>, name: &OsStr) -> nix::Result<Option<Entry>> {
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

/// The last component of a path exists, as `found` in `parent`: it must be a directory or a symlink
/// that resolves to one, which is returned.
pub(super) fn require_directory(parent: BorrowedFd<'_>, name: &OsStr, found: &Entry, path: &Path) -> Result<FileStat> {
    let not_a_directory = || Error::NotADirectory {
        path: path.to_path_buf(),
    };
    match found {
        Entry::Dir(_, stat) => Ok(*stat),
        Entry::Link(..) => match fstatat(parent, name, AtFlags::empty()) {
            Ok(stat) if SFlag::from_bits_truncate(stat.st_mode) & SFlag::S_IFMT == SFlag::S_IFDIR => Ok(stat),
            Ok(_) => Err(not_a_directory()),
            Err(e) => Err(Error::os_at("stat", path, e)),
        },
        Entry::Other(_) => Err(not_a_directory()),
    }
}

/// Resolve `dir`, which must be absolute, from `/` one component at a time, on descriptors, following
/// every symlink that leads through it and, as the last component, one that leads to a directory. A
/// component that does not exist ends the resolution: what remains is reported as missing, and `..`
/// below it is an error. A link that leads nowhere is an error too, since what is made below it would
/// be made at its target. `roots` are opened first, so that "under a root" is a fact about the
/// directories the path resolved through.
pub(super) fn walk(dir: &Path, roots: &[PathBuf]) -> Result<Walk> {
    if !dir.has_root() {
        return Err(Error::os_at(
            "create directory",
            dir,
            io::Error::other("not an absolute path"),
        ));
    }
    let search = OFlag::O_DIRECTORY | SEARCH | OFlag::O_CLOEXEC;
    let root_ids: Vec<(libc::dev_t, libc::ino_t)> = roots
        .iter()
        .filter_map(|root| {
            let fd = open(root.as_path(), search, Mode::empty()).ok()?;
            fstat(&fd).ok().map(|stat| (stat.st_dev, stat.st_ino))
        })
        .collect();
    let open_root = || open("/", search, Mode::empty()).map_err(|e| Error::os_at("open", "/", e));
    let id_of = |stat: &FileStat| (stat.st_dev, stat.st_ino);

    let mut cur = open_root()?;
    let stat = fstat(&cur).map_err(|e| Error::os_at("stat", "/", e))?;
    // The directories from `/` to `cur`, `..` and symlinks resolved.
    let mut chain = vec![id_of(&stat)];
    let mut trail = vec![Step {
        at: PathBuf::from("/"),
        stat,
        link: false,
    }];
    // Where `cur` is, as far as the walk can tell, for naming what it passed.
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
            let stat = fstat(&up).map_err(|e| Error::os_at("stat", &at, e))?;
            if chain.len() > 1 {
                chain.pop();
            }
            shown.pop();
            trail.push(Step {
                at: shown.clone(),
                stat,
                link: false,
            });
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
        if let Entry::Link(stat, _) = &found {
            trail.push(Step {
                at: at.clone(),
                stat: *stat,
                link: true,
            });
        }
        // A link as the last component is walked to its target like one in the middle, so that every
        // directory its target is under reaches the trail.
        if queue.is_empty() && !matches!(found, Entry::Link(..)) {
            let stat = require_directory(cur.as_fd(), &name, &found, &so_far)?;
            trail.push(Step { at, stat, link: false });
            chain.push(id_of(&stat));
            let under_root = chain.iter().any(|id| root_ids.contains(id));
            return Ok(Walk {
                end: End::Whole(stat),
                trail,
                under_root,
            });
        }
        match found {
            Entry::Dir(fd, stat) => {
                trail.push(Step {
                    at: at.clone(),
                    stat,
                    link: false,
                });
                chain.push(id_of(&stat));
                cur = fd;
                shown = at;
            }
            Entry::Link(_, target) => {
                links += 1;
                if links > SYMLINK_LIMIT {
                    return Err(Error::os_at("resolve", &at, io::Error::from_raw_os_error(libc::ELOOP)));
                }
                if Path::new(&target).has_root() {
                    cur = open_root()?;
                    let stat = fstat(&cur).map_err(|e| Error::os_at("stat", "/", e))?;
                    chain = vec![id_of(&stat)];
                    shown = PathBuf::from("/");
                }
                for step in components(Path::new(&target), true).into_iter().rev() {
                    queue.push_front(step);
                }
            }
            Entry::Other(_) => return Err(Error::os_at("stat", &at, Errno::ENOTDIR)),
        }
    }
    let under_root = chain.iter().any(|id| root_ids.contains(id));
    // Nothing is missing when the path ended in `..`, or in a link to `/`, or is `/`: the directory
    // the walk is in is the one the path names.
    let end = if missing.is_empty() {
        End::Whole(fstat(&cur).map_err(|e| Error::os_at("stat", &shown, e))?)
    } else {
        End::Missing { holder: cur, missing }
    };
    Ok(Walk { end, trail, under_root })
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

/// `name` under `parent`, as a directory opened with `base`: a symlink there is an error, never
/// followed.
pub(super) fn open_beneath(parent: BorrowedFd<'_>, name: &OsStr, base: OFlag) -> nix::Result<OwnedFd> {
    openat(
        parent,
        name,
        base | OFlag::O_DIRECTORY | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC,
        Mode::empty(),
    )
}
