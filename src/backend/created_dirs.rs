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

use super::{Settle, Standing};
use crate::error::{Error, Result};

/// The directories one `write` has created, removed when it is dropped unless
/// [`settle`](Settle::settle) kept them.
///
/// Root creates these directories and the daemon's account, which they are handed to, can swap one
/// for a symlink. So every step on a directory goes by descriptor, never by path ([`walk`]
/// resolves the path to one), and an account never owns a directory root is still working in:
/// [`create`](Self::create) makes every directory root-owned and private, and only
/// [`hand_over`](Self::hand_over), once all are made, gives them their mode and owner. A directory
/// that is kept is handed over whether or not `hand_over` was reached, so none stays private. A drop
/// takes a directory back from the account only to remove something from it, and puts it back as it
/// was if that does not remove it.
///
/// The resolution follows systemd's `CHASE_SAFE` (see [`refuse_unsafe`]) on every step, plain
/// directories included. A path that steps between owners is therefore refused, not created in;
/// the directories this guard made are the exception, as it knows whose they are.
///
/// A descriptor stays open per directory until the drop; running out of them fails the install and
/// leaves the directory being made.
#[derive(Debug, Default)]
pub(crate) struct CreatedDirs(Vec<Created>);

/// Whom a created directory is handed to.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Owner {
    uid: Uid,
    /// `None` leaves the group as the creating process's.
    gid: Option<Gid>,
}

impl Owner {
    #[cfg(all(test, target_os = "linux"))]
    pub(crate) fn uid(&self) -> Uid {
        self.uid
    }

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

#[derive(Debug)]
struct Created {
    path: PathBuf,
    name: OsString,
    parent: Parent,
    /// The directory itself, opened without following a symlink straight after its `mkdirat`.
    dir: OwnedFd,
    dev: libc::dev_t,
    ino: libc::ino_t,
    /// Who made it, and its group then: what a drop gives back.
    uid: libc::uid_t,
    gid: libc::gid_t,
    /// Whom it is handed to, if anyone.
    owner: Option<Owner>,
    state: State,
}

/// How far a created directory has come, each step set only after the call that makes it succeeds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    /// Made `0700`, and the creator's.
    Private,
    /// Given its mode, `0755`; not yet given to its owner.
    Opened,
    /// Given to its owner, if it has one: nothing is left to do.
    Handed,
}

/// What a directory was before a drop took it back from the account.
#[derive(Debug)]
struct Prior {
    uid: libc::uid_t,
    gid: libc::gid_t,
    mode: libc::mode_t,
}

impl Created {
    /// Take it from [`State::Private`] to [`State::Handed`], one call at a time.
    fn hand_over(&mut self) -> Result<()> {
        if self.state == State::Private {
            fchmod(&self.dir, Mode::from_bits_truncate(0o755)).map_err(|e| Error::os_at("chmod", &self.path, e))?;
            self.state = State::Opened;
        }
        if self.state == State::Opened {
            if let Some(owner) = self.owner {
                fchown(&self.dir, Some(owner.uid), owner.gid).map_err(|e| Error::os_at("chown", &self.path, e))?;
            }
            self.state = State::Handed;
        }
        Ok(())
    }

    /// Whether the account it was handed to owns it.
    fn account_owns(&self) -> bool {
        self.state == State::Handed && self.owner.is_some()
    }
}

/// Whether a removal went through.
enum RemoveOutcome {
    Removed,
    /// The directory is not removed: it has content, or is not the one created.
    Stays,
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
    /// Create `dir` and every missing ancestor: private, and root's until [`hand_over`](Self::hand_over)
    /// gives them to `owner`, which a directory that is kept is given too. A path that exists is left
    /// untouched, and so is a component another install creates after the walk: not ours. Whatever is
    /// at the end of the path must be a directory, or a symlink to one.
    pub(crate) fn create(&mut self, dir: &Path, owner: Option<Owner>) -> Result<()> {
        let found = walk(dir, &self.0)?;
        self.create_in(found, owner)
    }

    /// Create what `walk` found missing, under the directory it holds open.
    fn create_in(&mut self, found: Walk, owner: Option<Owner>) -> Result<()> {
        let Walk {
            holder,
            owner: holder_uid,
            missing,
        } = found;
        let mut parent = Parent::Existing(holder);
        let mut parent_owner = holder_uid;
        for (n, (path, name)) in missing.iter().enumerate() {
            let last = n + 1 == missing.len();
            // Private until `hand_over` makes it `0755` whatever the umask.
            match mkdirat(self.fd_of(&parent), name.as_os_str(), Mode::from_bits_truncate(0o700)) {
                Ok(()) => {
                    let opened = open_beneath(self.fd_of(&parent), name, OFlag::O_RDONLY)
                        .and_then(|dir| fstat(&dir).map(|stat| (dir, stat)));
                    let (dir, stat) = opened.map_err(|e| {
                        report_stranded(path, &e);
                        Error::os_at("open", path, e)
                    })?;
                    // The parent may be writable by an account: what is held must be the directory just made.
                    let ours = fstat(self.fd_of(&parent)).map_err(|e| e.to_string()).and_then(|held| {
                        if made_here(&held, stat.st_uid, Uid::effective().as_raw()) {
                            Ok(())
                        } else {
                            Err(format!("not the directory just made: owned by uid {}", stat.st_uid))
                        }
                    });
                    if let Err(why) = ours {
                        let reason = io::Error::other(why);
                        report_stranded(path, &reason);
                        return Err(Error::os_at("open", path, reason));
                    }
                    self.0.push(Created {
                        path: path.clone(),
                        name: name.clone(),
                        parent,
                        dir,
                        dev: stat.st_dev,
                        ino: stat.st_ino,
                        uid: stat.st_uid,
                        gid: stat.st_gid,
                        owner,
                        state: State::Private,
                    });
                    parent = Parent::Created(self.0.len() - 1);
                    parent_owner = stat.st_uid;
                }
                // Another install created it first: not ours, and judged as `walk` judges one.
                Err(Errno::EEXIST) => {
                    let found = entry(self.fd_of(&parent), name)
                        .map_err(|e| Error::os_at("open", path, e))?
                        .ok_or_else(|| Error::os_at("open", path, io::Error::from(io::ErrorKind::NotFound)))?;
                    refuse_unsafe(parent_owner, found.uid(), path, None)?;
                    if last {
                        return require_directory(self.fd_of(&parent), name, &found, path);
                    }
                    let Entry::Dir(held, stat) = found else {
                        return Err(Error::NotADirectory { path: path.clone() });
                    };
                    parent = Parent::Existing(held);
                    parent_owner = stat.st_uid;
                }
                Err(e) => return Err(Error::os_at("create directory", path, e)),
            }
        }
        Ok(())
    }

    /// Give every directory made so far its mode, `0755`, and its owner. The daemon's account can
    /// enter what it owns, so this is the last thing done to them: nothing is made inside after it.
    pub(crate) fn hand_over(&mut self) -> Result<()> {
        self.0.iter_mut().try_for_each(Created::hand_over)
    }

    /// Hand over everything still private, and let go of all of it: the directories stay. A step that
    /// fails is reported, and the rest still go.
    fn keep(&mut self) {
        for created in &mut self.0 {
            if let Err(e) = created.hand_over() {
                report(format_args!(
                    "could not hand {} over, which the install created and keeps: {e}",
                    created.path.display()
                ));
            }
        }
        self.0.clear();
    }

    fn fd_of<'a>(&'a self, parent: &'a Parent) -> BorrowedFd<'a> {
        match parent {
            Parent::Created(index) => self.0[*index].dir.as_fd(),
            Parent::Existing(fd) => fd.as_fd(),
        }
    }

    /// Take `self.0[index]` back from the account: its creator's, and a mode nobody else can write.
    /// A failure leaves it as it was.
    fn reclaim(&self, index: usize) -> std::result::Result<Prior, String> {
        let created = &self.0[index];
        let held = fstat(&created.dir).map_err(|e| e.to_string())?;
        let prior = Prior {
            uid: held.st_uid,
            gid: held.st_gid,
            mode: held.st_mode & 0o7777,
        };
        let taken = fchown(
            &created.dir,
            Some(Uid::from_raw(created.uid)),
            Some(Gid::from_raw(created.gid)),
        )
        .and_then(|()| fchmod(&created.dir, Mode::from_bits_truncate(0o755)));
        match taken {
            Ok(()) => Ok(prior),
            Err(e) => Err(match self.restore(index, &prior) {
                Ok(()) => e.to_string(),
                Err(again) => format!("{e}, and it could not be put back: {again}"),
            }),
        }
    }

    /// Give `self.0[index]` back what it was before [`reclaim`](Self::reclaim).
    fn restore(&self, index: usize, prior: &Prior) -> std::result::Result<(), String> {
        let dir = &self.0[index].dir;
        fchown(dir, Some(Uid::from_raw(prior.uid)), Some(Gid::from_raw(prior.gid))).map_err(|e| e.to_string())?;
        fchmod(dir, Mode::from_bits_truncate(prior.mode)).map_err(|e| e.to_string())
    }

    /// Remove `self.0[index]` from its parent if the name still refers to the directory that was
    /// created. A directory that is gone is removed; one that has gained content stays, without a
    /// word. While the account owns the parent it can swap the name between the look and the unlink,
    /// so the parent is taken back first, and is recorded in `taken` for the caller to put back.
    fn remove(&self, index: usize, taken: &mut [Option<Prior>]) -> std::result::Result<RemoveOutcome, String> {
        let created = &self.0[index];
        if let Parent::Created(parent) = created.parent
            && self.0[parent].account_owns()
            && taken[parent].is_none()
        {
            let prior = self.reclaim(parent).map_err(|e| {
                format!(
                    "could not take {} back from the account first: {e}",
                    self.0[parent].path.display()
                )
            })?;
            taken[parent] = Some(prior);
        }
        let parent = self.fd_of(&created.parent);
        match fstatat(parent, created.name.as_os_str(), AtFlags::AT_SYMLINK_NOFOLLOW) {
            Ok(now) if now.st_dev == created.dev && now.st_ino == created.ino => {}
            Ok(_) => return Err("it is no longer the directory the install created".to_string()),
            Err(Errno::ENOENT) => return Ok(RemoveOutcome::Removed),
            Err(e) => return Err(e.to_string()),
        }
        match unlinkat(parent, created.name.as_os_str(), UnlinkatFlags::RemoveDir) {
            Ok(()) | Err(Errno::ENOENT) => Ok(RemoveOutcome::Removed),
            Err(Errno::ENOTEMPTY | Errno::EEXIST) => Ok(RemoveOutcome::Stays),
            Err(e) => Err(e.to_string()),
        }
    }

    /// Remove what can be removed, deepest first, since each was created after its parent. A
    /// directory that stays keeps its ancestors, which then hold content, as the account had them.
    fn undo(&self) {
        let mut taken: Vec<Option<Prior>> = self.0.iter().map(|_| None).collect();
        let mut stays = vec![false; self.0.len()];
        for index in (0..self.0.len()).rev() {
            if !stays[index] {
                match self.remove(index, &mut taken) {
                    Ok(RemoveOutcome::Removed) => {}
                    Ok(RemoveOutcome::Stays) => stays[index] = true,
                    Err(e) => {
                        report(format_args!(
                            "could not remove {}, which the failed install created: {e}",
                            self.0[index].path.display()
                        ));
                        stays[index] = true;
                    }
                }
            }
            if stays[index] {
                if let Parent::Created(parent) = self.0[index].parent {
                    stays[parent] = true;
                }
                if let Some(prior) = taken[index].take()
                    && let Err(e) = self.restore(index, &prior)
                {
                    report(format_args!(
                        "could not give {} back to the account after taking it back: {e}",
                        self.0[index].path.display()
                    ));
                }
            }
        }
    }
}

impl Settle for CreatedDirs {
    /// An unknown answer keeps the directories and says so: removing them could break an installed
    /// definition.
    fn settle(&mut self, standing: impl FnOnce() -> Standing) {
        if self.0.is_empty() {
            return;
        }
        match standing() {
            Standing::Yes => self.keep(),
            Standing::No => {}
            Standing::Unknown(reason) => {
                report(format_args!("{reason}; keeping the directories the install created"));
                self.keep();
            }
        }
    }
}

/// What a walk of a path found: the deepest directory that exists, held open, and the uid that owns
/// it, and the components below it that do not, each with its path, outermost first.
struct Walk {
    holder: OwnedFd,
    owner: libc::uid_t,
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
/// `MAXSYMLINKS` (macOS's is 32).
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

/// The last component of a path exists, as `found` in `parent`: it must be a directory or a symlink
/// that resolves to one.
fn require_directory(parent: BorrowedFd<'_>, name: &OsStr, found: &Entry, path: &Path) -> Result<()> {
    let not_a_directory = || Error::NotADirectory {
        path: path.to_path_buf(),
    };
    match found {
        Entry::Dir(..) => Ok(()),
        Entry::Link(..) => match fstatat(parent, name, AtFlags::empty()) {
            Ok(stat) if SFlag::from_bits_truncate(stat.st_mode) & SFlag::S_IFMT == SFlag::S_IFDIR => Ok(()),
            Ok(_) => Err(not_a_directory()),
            Err(e) => Err(Error::os_at("stat", path, e)),
        },
        Entry::Other(_) => Err(not_a_directory()),
    }
}

/// Resolve `dir`, which must be absolute, from `/` one component at a time, on descriptors,
/// following the symlinks that lead through it and none that is the last component, which must
/// be a directory or lead to one. Every step is judged by [`refuse_unsafe`]; a link carries its own
/// owner onto whatever it leads to, and an absolute one is judged against `/`, which root owns.
/// `made` are the directories the guard made, which are not judged.
fn walk(dir: &Path, made: &[Created]) -> Result<Walk> {
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
        let ours =
            matches!(&found, Entry::Dir(_, stat) if made.iter().any(|c| c.dev == stat.st_dev && c.ino == stat.st_ino));
        if !ours {
            refuse_unsafe(owner, uid, &at, carried.as_deref())?;
        }
        if queue.is_empty() {
            require_directory(cur.as_fd(), &name, &found, &so_far)?;
            return Ok(Walk {
                holder: cur,
                owner,
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
    Ok(Walk {
        holder: cur,
        owner,
        missing,
    })
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
/// `to` itself: systemd's `CHASE_SAFE`, which keeps a path an account controls from leading root
/// into a directory the account does not own. `carried` is the link `from` is the owner of, when
/// the walk has not entered a directory since.
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
    /// Removes what was not kept: see [`undo`](CreatedDirs::undo). A panic may come after the
    /// definition was written, which leaves its standing as unknown as an unreadable one does, so the
    /// directories are kept, and kept means handed over. A step that fails is reported on stderr, and
    /// a failed report is ignored: this never panics.
    fn drop(&mut self) {
        if std::thread::panicking() {
            self.keep();
        } else {
            self.undo();
        }
    }
}

/// Whether `uid`, the owner of a directory just made in `parent`, is what a directory this process
/// made would have. A swap between the `mkdirat` and the open needs someone besides root who can
/// write `parent`: its owner, or anyone through group or other write permission without the sticky
/// bit. Where nobody can, the owner says nothing, since a file system may map it (NFS with
/// `root_squash` gives 65534, a vfat mount gives its `uid=`), and anything is accepted.
fn made_here(parent: &FileStat, uid: libc::uid_t, euid: libc::uid_t) -> bool {
    let others_write = parent.st_mode & (libc::S_IWGRP | libc::S_IWOTH) != 0 && parent.st_mode & libc::S_ISVTX == 0;
    (parent.st_uid == 0 && !others_write) || uid == euid
}

/// The directory `path`, made by the install, that could not be opened, so it stays.
fn report_stranded(path: &Path, why: &dyn std::fmt::Display) {
    report(format_args!(
        "could not remove {}, which the failed install created: it could not be opened: {why}",
        path.display()
    ));
}

/// A warning on stderr. A failed write is ignored: `eprintln!` would panic on one.
fn report(message: std::fmt::Arguments<'_>) {
    let _ = writeln!(io::stderr(), "warning: {message}");
}

#[cfg(test)]
#[path = "created_dirs_tests.rs"]
mod created_dirs_tests;
