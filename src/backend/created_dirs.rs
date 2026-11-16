//! The directories an install makes for a spec's `cwd` and the parent of its `logs`, and takes back
//! when the install does not leave the definition that names them in place.
//!
//! Root makes them, so what root makes is judged first, and only where root's own are expected:
//!
//! - **Roots.** A missing directory is made only under one of [`SYSTEM_ROOTS`], the places systemd
//!   itself makes directories in (`LogsDirectory=` and its kin). Anywhere else it is an error that
//!   says to create it first: goetia makes nothing in a user's home or a prefix of its own. A path
//!   that exists whole is used as it is, wherever it is and whoever owns it. A root that is not there
//!   (`ENOENT`, `ENOTDIR`, `ELOOP`) holds nothing and is skipped. The roots are read in order and the
//!   first that holds the path decides it; the roots after it are not read. One that cannot be read
//!   for any other reason fails the install, naming the root and the path, only when none that was
//!   read holds it.
//! - **Walk.** Where something is missing, [`walk()`] resolves the path on descriptors, and every
//!   directory it passed through must be owned by root: root does not create inside a directory an
//!   account owns. A symlink is judged by the directory that holds it, not by its owner: the owner of
//!   a link cannot change where it leads, and only whoever can write the holder can replace it. The
//!   exception is a holder that is sticky and that a group or everyone can write (`/tmp`), where the
//!   owner of an entry can delete it and make another: a link there is refused unless root owns it.
//!   Modes are otherwise not judged: a root-owned directory others can write is accepted (the
//!   administrator's exposure). Where writability matters the caller judges it: for a log systemd
//!   opens as root, `refuse_writable_log_dir` (Linux).
//! - **Making.** The first missing component of a path is made beside where it will be, under a
//!   temporary name (`.goetia-` and a random one), and the rest inside it under their final names. A
//!   later path whose missing part starts at the same component of the same directory is made in that
//!   tree. Each directory is made `0700` by `mkdirat` under the descriptor of the one above, and
//!   recorded at once, so that one that is then refused is removed too. It must be root's, a
//!   directory, and empty (a link count or a mode says nothing a file system keeps the same), and is
//!   then made `0755`.
//! - **Handing over.** Only the deepest directory of a path, and only where the daemon's account has
//!   to write it ([`Owner`]), is given to the account, once everything is made.
//! - **Placing.** Last, each tree is renamed to its final name by a rename that replaces nothing
//!   (`renameat2` with `RENAME_NOREPLACE`; on macOS `renameatx_np` with `RENAME_EXCL`), before
//!   [`make`](CreatedDirs::make) returns. A directory therefore appears at its name only in its final
//!   state, whenever the install stops: the definition can go live as soon as the write that follows
//!   starts, and a run finds every directory made, owned and moded as it is to be. (Where the holder
//!   is a root-owned directory others can write, they can swap what is at the temporary name before
//!   the rename, as they can write the final name directly: the accepted exposure of such a
//!   holder.) A name taken in the meantime is a refusal, whatever is there, and a file system that
//!   cannot make that rename is an error: there is no fallback to one that replaces.
//!
//! Every refusal that reading can show comes first. [`plan`](CreatedDirs::plan) reads and judges a
//! path and changes nothing; a caller that has more to judge (the log directory, which only the
//! caller knows is one) does it on the plans, and only then does [`make`](CreatedDirs::make) change
//! anything. A failure after that point is an I/O error, and undoes what was made.
//!
//! An install that fails, or leaves a definition other than the one it wrote, removes what it made,
//! deepest first, by descriptor (see [`undo`](CreatedDirs::undo)): an entry is removed only if it is
//! still the directory that was made and is empty, and a symlink is never followed. What stays is
//! reported: a directory that could not be opened when it was made, which cannot be told from what is
//! at its name later; one that holds content, the account's if it was handed over; and the
//! directories above any of these. An install stopped abruptly (power loss, `SIGKILL`) leaves at most
//! a temporary tree beside the path, which nothing uses.

#[cfg(target_os = "linux")]
mod log_dir;
mod walk;

use std::collections::hash_map::RandomState;
use std::ffi::{OsStr, OsString};
use std::hash::BuildHasher as _;
use std::io::{self, Write as _};
use std::os::fd::{AsFd as _, AsRawFd as _, BorrowedFd, OwnedFd};
use std::path::{Path, PathBuf};

use nix::NixPath as _;
use nix::dir::Dir;
use nix::errno::Errno;
use nix::fcntl::{AtFlags, OFlag};
use nix::sys::stat::{FileStat, Mode, SFlag, fchmod, fstat, fstatat, mkdirat};
use nix::unistd::{Gid, Uid, UnlinkatFlags, fchown, unlinkat};

#[cfg(target_os = "linux")]
pub(crate) use log_dir::refuse_writable_log_dir;
use walk::{End, RootsRead, Step, Walk, open_beneath, walk};

use super::account::describe_uid;
use super::{Settle, Standing};
use crate::error::{Error, Result, escape};

// The platform's rename that replaces nothing, with the arguments of `renameat2`.
#[cfg(target_os = "macos")]
use libc::{RENAME_EXCL as NO_REPLACE, renameatx_np as rename_flagged};
#[cfg(target_os = "linux")]
use libc::{RENAME_NOREPLACE as NO_REPLACE, renameat2 as rename_flagged};

/// Where a missing `cwd` or log directory is made: the directories systemd's own `LogsDirectory=`,
/// `StateDirectory=` and `CacheDirectory=` are under. `/run` is left out, since it is emptied at boot.
#[cfg(not(target_os = "macos"))]
pub(crate) const SYSTEM_ROOTS: &[&str] = &["/var/log", "/var/lib", "/var/cache"];
/// Where a missing `cwd` or log directory is made on macOS: the system's log and application-data
/// directories, and `/var/log` (`/var` is a link to `/private/var`, which the walk resolves).
#[cfg(target_os = "macos")]
pub(crate) const SYSTEM_ROOTS: &[&str] = &["/var/log", "/Library/Logs", "/Library/Application Support"];

/// The directories one install has made, removed when it is dropped unless [`settle`](Settle::settle)
/// kept them.
///
/// A descriptor stays open per directory made until the drop.
#[derive(Debug)]
pub(crate) struct CreatedDirs {
    roots: Vec<PathBuf>,
    made: Vec<Made>,
}

impl Default for CreatedDirs {
    /// Directories are made under [`SYSTEM_ROOTS`].
    fn default() -> Self {
        Self::under(SYSTEM_ROOTS.iter().map(PathBuf::from).collect())
    }
}

/// Whom a created directory is handed to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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

/// What an install would do about one path, found by reading alone: see [`CreatedDirs::plan`].
pub(crate) struct Plan {
    found: Walk,
}

/// The plans of one install and whom each plan's deepest directory is handed to: what
/// [`make`](CreatedDirs::make) takes.
pub(crate) type Planned = Vec<(Plan, Option<Owner>)>;

impl Plan {
    /// Whether `make` would make any of it: the path does not exist.
    #[cfg(target_os = "linux")]
    pub(crate) fn makes(&self) -> bool {
        matches!(self.found.end, End::Missing { .. })
    }
}

/// One directory made. It is in the list from its `mkdirat`, before anything is judged of it.
#[derive(Debug)]
struct Made {
    /// Where it is to be, whatever it is named until then.
    path: PathBuf,
    /// Its name in `parent` now.
    name: OsString,
    /// The name it is to be renamed to by [`CreatedDirs::place`], if it is the top of what a plan made and
    /// has not been yet: `name` is then a temporary one. Only such a directory has an existing `parent`.
    pending: Option<OsString>,
    parent: Parent,
    /// The directory itself, opened without following a symlink straight after its `mkdirat`; `None`
    /// if that open failed, in which case it cannot be told from what is at its name later, and the
    /// undo leaves it.
    held: Option<Held>,
    /// Whom it is to be handed to, if anyone.
    hand: Option<Owner>,
    /// Whom the plan that made it hands its deepest directory to: `None` for a plan that hands nothing
    /// over, which another plan does not hand this to either.
    plan_owner: Option<Owner>,
    /// Whether it has been: the account may write in it from then on.
    handed: bool,
    /// Whether a plan that hands nothing over has passed through it or ended at it: it is root's for that
    /// path, so no later plan may hand it over.
    passed: bool,
    /// Whether the undo could not remove it. What a directory above it then holds is that.
    left: bool,
}

#[derive(Debug)]
struct Held {
    dir: OwnedFd,
    dev: libc::dev_t,
    ino: libc::ino_t,
}

/// What a directory is made in, and removed from.
#[derive(Debug)]
enum Parent {
    /// An earlier entry's directory, by index.
    Made(usize),
    /// A directory this call did not make, as `walk` held it.
    Existing(OwnedFd),
}

/// The descriptor of `parent`. A made directory whose open failed has none; nothing is made below it.
fn fd_of<'a>(made: &'a [Made], parent: &'a Parent) -> Option<BorrowedFd<'a>> {
    match parent {
        Parent::Made(index) => made[*index].held.as_ref().map(|held| held.dir.as_fd()),
        Parent::Existing(fd) => Some(fd.as_fd()),
    }
}

/// Where a directory is made, for what is said of it, from one reading of the roots: those that exist,
/// and a clause for each that could not be read, which is not among them.
fn roots_text(read: &RootsRead) -> String {
    let mut text = match read.existing.as_slice() {
        [] => "any of the places goetia makes directories in".to_string(),
        roots => roots
            .iter()
            .map(|root| root.display().to_string())
            .collect::<Vec<_>>()
            .join(", "),
    };
    for (root, errno) in &read.unreadable {
        text.push_str(&format!(
            "; {} could not be read: {}",
            root.display(),
            io::Error::from(*errno)
        ));
    }
    text
}

impl CreatedDirs {
    /// Where `self.made[index]` is on disk now, for what is said of it: at a temporary name if it is a
    /// tree's top not yet put in place, and below that name if it is inside such a tree.
    fn at(&self, index: usize) -> PathBuf {
        let made = &self.made[index];
        match (&made.pending, &made.parent) {
            (Some(_), _) => made.path.with_file_name(&made.name),
            (None, Parent::Made(parent)) => self.at(*parent).join(&made.name),
            (None, Parent::Existing(_)) => made.path.clone(),
        }
    }

    /// A guard that makes directories under `roots`.
    pub(crate) fn under(roots: Vec<PathBuf>) -> Self {
        Self {
            roots,
            made: Vec::new(),
        }
    }

    /// Read `dir` and judge it, changing nothing: what [`make`](Self::make) would have to make, and
    /// whether it may.
    pub(crate) fn plan(&self, dir: &Path) -> Result<Plan> {
        let found = walk(dir)?;
        if matches!(found.end, End::Missing { .. }) {
            let read = found.under_root(&self.roots);
            if !read.holds {
                if let Some((root, errno)) = read.unreadable.first() {
                    return Err(Error::os_at(
                        format!("open the root {} to check whether it holds", root.display()),
                        dir,
                        io::Error::from(*errno),
                    ));
                }
                return Err(Error::DirectoryRefused {
                    path: dir.to_path_buf(),
                    why: format!(
                        "it does not exist and is not under {}; create it first",
                        roots_text(&read)
                    ),
                });
            }
            if let Some(why) = found.trail.iter().find_map(account_can_change) {
                return Err(Error::DirectoryRefused {
                    path: dir.to_path_buf(),
                    why,
                });
            }
        }
        Ok(Plan { found })
    }

    /// Make what the plans say is missing, root's, each under the directory its plan holds, then hand
    /// each plan's deepest directory to the owner it names, if it names one, and last put what was made
    /// in place under its final names. Nothing is handed over until everything is made, so that root
    /// never makes a directory inside one an account owns. A plan whose path exists needs nothing. On an
    /// error, what was made is the guard's to remove.
    pub(crate) fn make(&mut self, plans: Planned) -> Result<()> {
        for (plan, hand_to) in plans {
            self.make_missing(plan.found, hand_to)?;
        }
        self.hand_over()?;
        self.place()
    }

    /// Make one plan's missing components: the first under a temporary name beside where it is to be,
    /// the rest inside it under their own, or inside the tree an earlier plan made for the same name in
    /// the same directory.
    fn make_missing(&mut self, found: Walk, hand_to: Option<Owner>) -> Result<()> {
        let End::Missing { holder, missing } = found.end else {
            return Ok(());
        };
        let (first_path, first_name) = &missing[0];
        let own_path = &missing[missing.len() - 1].0;
        let mut dir = match self.pending_top(&holder, first_path, first_name)? {
            Some(top) => self.reach(top, own_path, hand_to)?,
            None => self.make_top(holder, first_path, first_name, hand_to)?,
        };
        for (path, name) in missing.iter().skip(1) {
            dir = match self.child_named(dir, name) {
                Some(child) => self.reach(child, own_path, hand_to)?,
                None => self.make_inside(dir, path, name, hand_to)?,
            };
        }
        match hand_to {
            Some(owner) => self.hand_to_one_owner(dir, own_path, owner),
            None => Ok(()),
        }
    }

    /// The top of a tree an earlier plan of this call made, for this name in this directory.
    fn pending_top(&self, holder: &OwnedFd, path: &Path, name: &OsStr) -> Result<Option<usize>> {
        let holder = fstat(holder).map_err(|e| Error::os_at("stat", path, e))?;
        for (index, made) in self.made.iter().enumerate() {
            if let (Some(pending), Parent::Existing(dir)) = (&made.pending, &made.parent)
                && pending == name
            {
                let dir = fstat(dir).map_err(|e| os_while_making("stat", &made.path, &self.at(index), e))?;
                if (dir.st_dev, dir.st_ino) == (holder.st_dev, holder.st_ino) {
                    return Ok(Some(index));
                }
            }
        }
        Ok(None)
    }

    /// The directory an earlier plan made in `parent` under `name`.
    fn child_named(&self, parent: usize, name: &OsStr) -> Option<usize> {
        self.made
            .iter()
            .position(|made| matches!(made.parent, Parent::Made(of) if of == parent) && made.name == name)
    }

    /// The top of a plan's tree: made in `holder` under a temporary name, to be renamed to `name`.
    fn make_top(&mut self, holder: OwnedFd, path: &Path, name: &OsStr, plan_owner: Option<Owner>) -> Result<usize> {
        let temporary = loop {
            let temporary = temporary_name();
            match mkdirat(holder.as_fd(), temporary.as_os_str(), PRIVATE) {
                Ok(()) => break temporary,
                // Taken (by an install that was stopped, or by another): another name.
                Err(Errno::EEXIST) => {}
                Err(e) => {
                    return Err(os_while_making(
                        "create directory",
                        path,
                        &path.with_file_name(&temporary),
                        e,
                    ));
                }
            }
        };
        self.record(
            Parent::Existing(holder),
            path,
            temporary,
            Some(name.to_owned()),
            plan_owner,
        )
    }

    /// A directory inside one this call made, under its final name. Nothing else can write the tree, so
    /// a name that is taken there is the same directory under another spelling.
    fn make_inside(&mut self, parent: usize, path: &Path, name: &OsStr, plan_owner: Option<Owner>) -> Result<usize> {
        let above = parent;
        let parent = Parent::Made(parent);
        let parent_fd = fd_of(&self.made, &parent).expect("a directory is made only below one that is held");
        match mkdirat(parent_fd, name, PRIVATE) {
            Ok(()) => {}
            Err(Errno::EEXIST) => {
                return Err(Error::DirectoryRefused {
                    path: path.to_path_buf(),
                    why: "it is the same directory as one this install made for another path of the spec, under \
                          another spelling (a file system that ignores case): use one spelling"
                        .to_string(),
                });
            }
            Err(e) => {
                return Err(os_while_making("create directory", path, &self.at(above).join(name), e));
            }
        }
        self.record(parent, path, name.to_owned(), None, plan_owner)
    }

    /// Hold the directory just made in `parent` under `name`, record it, judge it and open it up.
    fn record(
        &mut self,
        parent: Parent,
        path: &Path,
        name: OsString,
        pending: Option<OsString>,
        plan_owner: Option<Owner>,
    ) -> Result<usize> {
        let index = self.made.len();
        self.made.push(Made {
            path: path.to_path_buf(),
            name,
            pending,
            parent,
            held: None,
            hand: None,
            plan_owner,
            handed: false,
            passed: false,
            left: false,
        });
        let made = &self.made[index];
        let parent_fd = fd_of(&self.made, &made.parent).expect("a directory is made only below one that is held");
        let opened =
            open_beneath(parent_fd, &made.name, OFlag::O_RDONLY).and_then(|dir| fstat(&dir).map(|stat| (dir, stat)));
        let stat = match opened {
            Ok((dir, stat)) => {
                self.made[index].held = Some(Held {
                    dir,
                    dev: stat.st_dev,
                    ino: stat.st_ino,
                });
                stat
            }
            Err(e) => return Err(os_while_making("open", &self.made[index].path, &self.at(index), e)),
        };
        self.verify(index, &stat)?;
        Ok(index)
    }

    /// A plan passes through or ends at `dir`, which an earlier plan of this call made. If this plan hands
    /// nothing over and that plan handed `dir` to an account, this path is it or lies in it: the other half
    /// of [`hand_to_one_owner`](Self::hand_to_one_owner)'s refusal, so that the two orders agree. A plan
    /// that hands nothing over is recorded as having passed, for the same refusal from the other side.
    /// `path` is the asking plan's own, which the refusal names with the directory it is or lies in.
    fn reach(&mut self, dir: usize, path: &Path, hand_to: Option<Owner>) -> Result<usize> {
        let made = &mut self.made[dir];
        match (hand_to, made.hand) {
            (None, Some(owner)) => Err(Error::DirectoryRefused {
                path: path.to_path_buf(),
                why: format!(
                    "it is, or lies in, {}, which another path of the spec hands to {}, and this path is not handed \
                     to an account: keep the two paths apart",
                    made.path.display(),
                    describe_uid(owner.uid.as_raw())
                ),
            }),
            (None, None) => {
                made.passed = true;
                Ok(dir)
            }
            _ => Ok(dir),
        }
    }

    /// A later plan names the directory at `dir`, which an earlier plan of this call made, and hands the
    /// deepest directory of its path to `owner`.
    fn hand_to_one_owner(&mut self, dir: usize, path: &Path, owner: Owner) -> Result<()> {
        let ours = &mut self.made[dir];
        match ours.hand {
            // Every plan of one install names one account.
            Some(first) if first != owner => {
                return Err(Error::DirectoryRefused {
                    path: path.to_path_buf(),
                    why: format!(
                        "it is, or lies above, a directory this install made for another path of the spec, which is \
                         to be handed to {}, and this path is to be handed to {}: one directory has one owner",
                        describe_uid(first.uid.as_raw()),
                        describe_uid(owner.uid.as_raw())
                    ),
                });
            }
            Some(_) => {}
            // A plan that hands nothing over is not this one's to hand over: that directory is root's, and
            // a path that is to be the account's cannot be it or hold it. One that hands over is, unless a
            // plan that hands nothing over has passed through it since.
            None if ours.plan_owner.is_none() || ours.passed => {
                return Err(Error::DirectoryRefused {
                    path: path.to_path_buf(),
                    why: format!(
                        "it is, or lies above, a directory that another path of the spec, which is not handed to an \
                         account, made or passes through, and this path is to be handed to {}: keep the two paths \
                         apart",
                        describe_uid(owner.uid.as_raw())
                    ),
                });
            }
            None => ours.hand = Some(owner),
        }
        Ok(())
    }

    /// Rename each tree to its final name, so that it appears there in its final state. Whatever is at the
    /// name by then is not the install's, and is not replaced.
    fn place(&mut self) -> Result<()> {
        for index in 0..self.made.len() {
            let made = &self.made[index];
            let (Some(to), Parent::Existing(dir)) = (made.pending.clone(), &made.parent) else {
                continue;
            };
            rename_new(dir.as_fd(), &made.name, dir.as_fd(), &to).map_err(|e| match e {
                Errno::EEXIST => Error::DirectoryRefused {
                    path: made.path.clone(),
                    why: self.taken_why(dir, &to),
                },
                Errno::EINVAL | Errno::ENOSYS | Errno::ENOTSUP => Error::DirectoryRefused {
                    path: made.path.clone(),
                    why: format!(
                        "the file system it is on cannot rename a directory without replacing what is at the name \
                         ({e}), which goetia needs to make it appear only in its final state"
                    ),
                },
                e => os_while_making("put in place", &made.path, &self.at(index), e),
            })?;
            let made = &mut self.made[index];
            made.name = to;
            made.pending = None;
        }
        Ok(())
    }

    /// Why `name` in `dir` is taken: by a directory this install made (the spec names one directory
    /// twice, under two spellings, on a file system that ignores case), or by something that appeared
    /// while the install ran, which is left as it is and is said for what it is: only a root-owned
    /// directory is one that running the install again uses.
    fn taken_why(&self, dir: &OwnedFd, name: &OsStr) -> String {
        const APPEARED: &str = "it appeared while the install ran, and is left as it is";
        let Ok(taken) = fstatat(dir.as_fd(), name, AtFlags::AT_SYMLINK_NOFOLLOW) else {
            // It is gone again, or cannot be read: what it was is not known.
            return format!("{APPEARED}; run the install again to read it again");
        };
        let ours = self
            .made
            .iter()
            .filter_map(|made| made.held.as_ref())
            .any(|held| (held.dev, held.ino) == (taken.st_dev, taken.st_ino));
        if ours {
            return "two paths in the spec name the same directory, under two spellings (a file system that \
             ignores case): use one spelling"
                .to_string();
        }
        match SFlag::from_bits_truncate(taken.st_mode) & SFlag::S_IFMT {
            SFlag::S_IFDIR if taken.st_uid == 0 => format!("{APPEARED}; running the install again uses it"),
            SFlag::S_IFDIR => format!(
                "{APPEARED}; it is a directory owned by uid {}, not root, and root does not create inside what \
                 an account owns, so the install cannot use it: remove it, then run the install again",
                taken.st_uid
            ),
            SFlag::S_IFLNK => format!(
                "{APPEARED}; it is a symlink, which the install cannot use in place of the directory it made: \
                 remove it, then run the install again"
            ),
            SFlag::S_IFREG => format!(
                "{APPEARED}; it is a file, which the install cannot use as a directory: remove it, then run the \
                 install again"
            ),
            _ => format!(
                "{APPEARED}; it is not a directory, so the install cannot use it: remove it, then run the \
                 install again"
            ),
        }
    }

    /// Judge the directory just made, and open it up to `0755`.
    fn verify(&mut self, index: usize, stat: &FileStat) -> Result<()> {
        let at = self.at(index);
        let made = &self.made[index];
        let held = made
            .held
            .as_ref()
            .expect("a directory is judged through the descriptor it was opened by");
        let empty = is_empty(&held.dir).map_err(|e| os_while_making("read", &made.path, &at, e))?;
        judge(stat, empty).map_err(|why| Error::DirectoryRefused {
            path: made.path.clone(),
            why: format!("{why} {}", while_making(&at)),
        })?;
        fchmod(&held.dir, Mode::from_bits_truncate(0o755)).map_err(|e| os_while_making("chmod", &made.path, &at, e))?;
        Ok(())
    }

    /// Give every directory marked for an owner to it. The first that cannot be given is
    /// the error: the install fails, and the guard removes what is made.
    fn hand_over(&mut self) -> Result<()> {
        for index in 0..self.made.len() {
            let made = &self.made[index];
            if let (Some(owner), Some(held), false) = (made.hand, &made.held, made.handed) {
                fchown(&held.dir, Some(owner.uid), owner.gid).map_err(|e| {
                    os_while_making(
                        &format!("hand over to uid {}", owner.uid),
                        &made.path,
                        &self.at(index),
                        e,
                    )
                })?;
                self.made[index].handed = true;
            }
        }
        Ok(())
    }

    /// Let go of everything made: the directories stay, except a tree not yet put in place (a `make` that
    /// did not finish), which is at a name no definition can name and is removed as an undo removes it.
    /// A finished `make` has judged, handed over and placed everything.
    fn keep(&mut self) {
        let mut unplaced: Vec<bool> = Vec::with_capacity(self.made.len());
        for made in &self.made {
            unplaced.push(made.pending.is_some() || matches!(made.parent, Parent::Made(parent) if unplaced[parent]));
        }
        self.undo_where(|index| unplaced[index], Removal::Unplaced);
        self.made.clear();
    }

    /// Remove `self.made[index]` from its parent if the name still refers to the directory that was
    /// made and it is empty, and say whether it stays. It is unlinked by name, so where others can write
    /// the parent, an empty directory they put at the name between the check and the unlink is removed,
    /// as they could remove it themselves. A directory that is gone is removed. One that holds content
    /// stays, and the error says whose it is.
    fn remove(&mut self, index: usize) -> std::result::Result<(), String> {
        let result = self.unlink(index);
        self.made[index].left = result.is_err();
        result
    }

    fn unlink(&self, index: usize) -> std::result::Result<(), String> {
        let made = &self.made[index];
        let parent = fd_of(&self.made, &made.parent).ok_or("the directory above it was not held")?;
        let held = made
            .held
            .as_ref()
            .ok_or("it could not be opened when it was made, so what is at its name cannot be told from it")?;
        match fstatat(parent, made.name.as_os_str(), AtFlags::AT_SYMLINK_NOFOLLOW) {
            Ok(now) if now.st_dev == held.dev && now.st_ino == held.ino => {}
            Ok(_) => return Err("it is no longer the directory the install made".to_string()),
            Err(Errno::ENOENT) => return Ok(()),
            Err(e) => return Err(e.to_string()),
        }
        match unlinkat(parent, made.name.as_os_str(), UnlinkatFlags::RemoveDir) {
            Ok(()) | Err(Errno::ENOENT) => Ok(()),
            Err(Errno::ENOTEMPTY | Errno::EEXIST) => Err(self.holds(index)),
            Err(e) => Err(e.to_string()),
        }
    }

    /// Why the directory at `index` is left with content: a directory made below it that stays, or what
    /// the account wrote into it once it was handed over, or what is not the install's.
    fn holds(&self, index: usize) -> String {
        let below = self
            .made
            .iter()
            .find(|child| child.left && matches!(child.parent, Parent::Made(parent) if parent == index));
        match below {
            Some(child) => format!(
                "it holds {}, which the install created and could not remove",
                child.path.display()
            ),
            None if self.made[index].handed => {
                "it was handed to an account that has written into it since, and is left".to_string()
            }
            None => "it has content the install did not create, and is left".to_string(),
        }
    }

    /// Remove what can be removed, deepest first, since each was made after its parent. A directory
    /// that stays keeps its ancestors, which then hold content.
    fn undo(&mut self) {
        self.undo_where(|_| true, Removal::AfterFailure);
    }

    /// [`undo`](Self::undo) for the directories `which` selects, by index.
    fn undo_where(&mut self, which: impl Fn(usize) -> bool, removal: Removal) {
        for index in (0..self.made.len()).rev().filter(|&index| which(index)) {
            if let Err(e) = self.remove(index) {
                report(format_args!("{}", removal_failure(&self.at(index), &e, removal)));
            }
        }
    }
}

/// Why root does not create below `step`, if an account can change what it leads to or holds: a
/// directory the account owns, or a symlink it owns in a sticky directory others can write.
fn account_can_change(step: &Step) -> Option<String> {
    if let Some(uid) = step.owner_who_can_replace() {
        return Some(format!(
            "the symlink {} is owned by uid {uid}, not root, and is in a directory that others can write, where \
             the sticky bit lets its owner replace it with one that leads anywhere; create it first",
            step.at.display(),
        ));
    }
    (!step.is_link() && step.stat.st_uid != 0).then(|| {
        format!(
            "the directory {} is owned by uid {}, not root, and root does not create inside what an account \
             can change; create it first",
            step.at.display(),
            step.stat.st_uid
        )
    })
}

/// Why a directory is being removed.
#[derive(Debug, Clone, Copy)]
enum Removal {
    /// The install failed.
    AfterFailure,
    /// It is part of a tree that was never put in place.
    Unplaced,
}

/// What is said of a directory at `at` that could not be removed, for the reason `e`.
fn removal_failure(at: &Path, e: &str, removal: Removal) -> String {
    let made = match removal {
        Removal::AfterFailure => "which the failed install created",
        Removal::Unplaced => "which the install was making and never put in place",
    };
    format!("could not remove {}, {made}: {e}", at.display())
}

/// The mode a directory is made with: private until it is judged, whatever the umask.
const PRIVATE: Mode = Mode::from_bits_truncate(0o700);

/// A name for the tree a plan is made in until it is put in place.
fn temporary_name() -> OsString {
    format!(".goetia-{:016x}", RandomState::new().hash_one(())).into()
}

/// Rename `from` in `from_dir` to `to` in `to_dir`, unless something is at `to`: then `EEXIST`, and
/// nothing changes.
fn rename_new(from_dir: BorrowedFd<'_>, from: &OsStr, to_dir: BorrowedFd<'_>, to: &OsStr) -> nix::Result<()> {
    from.with_nix_path(|from| {
        to.with_nix_path(|to| {
            // SAFETY: both are NUL-terminated and outlive the call, and both descriptors are open.
            Errno::result(unsafe {
                rename_flagged(
                    from_dir.as_raw_fd(),
                    from.as_ptr(),
                    to_dir.as_raw_fd(),
                    to.as_ptr(),
                    NO_REPLACE,
                )
            })
        })
    })???;
    Ok(())
}

/// Where a directory is while it is made, which the path the spec wrote does not show: at a temporary
/// name until it is placed.
fn while_making(at: &Path) -> String {
    format!("(while it was being made at {})", at.display())
}

/// An OS call on a directory being made failed. It names the path the spec wrote, and says where the
/// directory is ([`while_making`]).
fn os_while_making(what: &str, spec: &Path, at: &Path, source: impl Into<io::Error>) -> Error {
    let source = source.into();
    Error::os_at(
        what,
        spec,
        io::Error::new(source.kind(), format!("{source} {}", while_making(at))),
    )
}

/// Whether a directory just made is root's, a directory and empty: the checks that say it is the one
/// `mkdirat` made. Not its link count (btrfs reports 1) and not its mode (a setgid parent and the
/// umask both change it).
fn judge(stat: &FileStat, empty: bool) -> std::result::Result<(), String> {
    if SFlag::from_bits_truncate(stat.st_mode) & SFlag::S_IFMT != SFlag::S_IFDIR {
        return Err("it is not a directory".to_string());
    }
    if stat.st_uid != 0 {
        return Err(format!(
            "the directory just made there is owned by uid {}, not root: it is not the one that was made",
            stat.st_uid
        ));
    }
    if !empty {
        return Err("the directory just made there is not empty: it is not the one that was made".to_string());
    }
    Ok(())
}

/// Whether the directory `dir` holds nothing. It is read through a duplicate of the descriptor, which
/// the read closes.
fn is_empty(dir: &OwnedFd) -> io::Result<bool> {
    let mut entries = Dir::from_fd(dir.try_clone()?)?;
    for entry in entries.iter() {
        if !matches!(entry?.file_name().to_bytes(), b"." | b"..") {
            return Ok(false);
        }
    }
    Ok(true)
}

impl Settle for CreatedDirs {
    /// An unknown answer keeps the directories and says so: removing them could break an installed
    /// definition.
    fn settle(&mut self, standing: impl FnOnce() -> Standing) {
        if self.made.is_empty() {
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

impl Drop for CreatedDirs {
    /// Removes what was not kept (see [`undo`](CreatedDirs::undo)). During a panic the definition's
    /// standing is unknown, so it keeps instead, as [`keep`](CreatedDirs::keep) does. Never panics: a
    /// failed report is ignored.
    fn drop(&mut self) {
        if std::thread::panicking() {
            self.keep();
        } else {
            self.undo();
        }
    }
}

/// A warning on stderr. A failed write is ignored: `eprintln!` would panic on one.
fn report(message: std::fmt::Arguments<'_>) {
    let _ = writeln!(io::stderr(), "{}", warning_line(message));
}

/// What [`report`] writes: the message, which quotes paths from the spec, escaped as the CLI escapes
/// what it quotes.
fn warning_line(message: std::fmt::Arguments<'_>) -> String {
    format!("warning: {}", escape(&message.to_string()))
}

#[cfg(test)]
#[path = "created_dirs_tests.rs"]
pub(crate) mod created_dirs_tests;
