//! The directories an install makes for a spec's `cwd` and the parent of its `logs`, and takes back
//! when the install does not leave the definition that names them in place.
//!
//! Root makes them, so what root makes is judged first, and only where root's own are expected:
//!
//! - **Roots.** A missing directory is made only under one of [`SYSTEM_ROOTS`], the places systemd
//!   itself makes directories in (`LogsDirectory=` and its kin). Anywhere else it is an error that
//!   says to create it first: goetia makes nothing in a user's home or a prefix of its own. A path
//!   that exists whole is used as it is, wherever it is and whoever owns it.
//! - **Walk.** Where something is missing, [`walk()`] resolves the path on descriptors, and every
//!   directory and symlink it passed through must be owned by root: root does not create inside a
//!   directory, nor follow a link, that an account owns. Modes are not judged: a root-owned directory
//!   others can write is accepted (the administrator's exposure). Where writability matters the
//!   caller judges it: for a log systemd opens as root, `refuse_writable_log_dir` (Linux).
//! - **Making.** Each component is made `0700` by `mkdirat` under the descriptor of the one above,
//!   and recorded at once, so that one that is then refused is removed too. It must be root's, a
//!   directory, and empty (a link count or a mode says nothing a file system keeps the same), and is
//!   then made `0755`.
//! - **Handing over.** Only the deepest directory of a path, and only where the daemon's account has
//!   to write it ([`Owner`]), is given to the account, once everything is made and before
//!   [`make`](CreatedDirs::make) returns: the definition can go live as soon as the write that
//!   follows starts, and a run then finds every directory in its final state.
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
//! directories above any of these.

#[cfg(target_os = "linux")]
mod log_dir;
mod walk;

use std::ffi::OsString;
use std::io::{self, Write as _};
use std::os::fd::{AsFd as _, BorrowedFd, OwnedFd};
use std::path::{Path, PathBuf};

use nix::dir::Dir;
use nix::errno::Errno;
use nix::fcntl::{AtFlags, OFlag};
use nix::sys::stat::{FileStat, Mode, SFlag, fchmod, fstat, fstatat, mkdirat};
use nix::unistd::{Gid, Uid, UnlinkatFlags, fchown, unlinkat};

#[cfg(target_os = "linux")]
pub(crate) use log_dir::refuse_writable_log_dir;
use walk::{End, Entry, Walk, entry, open_beneath, require_directory, walk};

use super::account::describe_uid;
use super::{Settle, Standing};
use crate::error::{Error, Result};

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
    path: PathBuf,
    name: OsString,
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
    /// Whether the undo could not remove it. What a directory above it then holds is that.
    left: bool,
    /// Judged and given its mode. One that is not is left behind by a keep, and named.
    ready: bool,
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

impl CreatedDirs {
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
            if !found.under_root(&self.roots)? {
                return Err(Error::DirectoryRefused {
                    path: dir.to_path_buf(),
                    why: format!(
                        "it does not exist and is not under {}; create it first",
                        self.roots_text()
                    ),
                });
            }
            if let Some(step) = found.trail.iter().find(|step| step.stat.st_uid != 0) {
                return Err(Error::DirectoryRefused {
                    path: dir.to_path_buf(),
                    why: format!(
                        "{} {} is owned by uid {}, not root, and root does not create inside what an account can \
                         change; create it first",
                        if step.link { "the symlink" } else { "the directory" },
                        step.at.display(),
                        step.stat.st_uid
                    ),
                });
            }
        }
        Ok(Plan { found })
    }

    fn roots_text(&self) -> String {
        match self.roots.as_slice() {
            [] => "any of the places goetia makes directories in".to_string(),
            roots => roots
                .iter()
                .map(|root| root.display().to_string())
                .collect::<Vec<_>>()
                .join(", "),
        }
    }

    /// Make what the plans say is missing, root's, each under the directory its plan holds, then hand
    /// each plan's deepest directory to the owner it names, if it names one. Nothing is handed over until
    /// everything is made, so that root never makes a directory inside one an account owns. A plan whose
    /// path exists needs nothing. On an error, what was made is the guard's to remove.
    pub(crate) fn make(&mut self, plans: Planned) -> Result<()> {
        for (plan, hand_to) in plans {
            self.make_missing(plan.found, hand_to)?;
        }
        self.hand_over()
    }

    fn make_missing(&mut self, found: Walk, hand_to: Option<Owner>) -> Result<()> {
        let End::Missing { holder, missing } = found.end else {
            return Ok(());
        };
        let mut parent = Parent::Existing(holder);
        for (n, (path, name)) in missing.iter().enumerate() {
            let last = n + 1 == missing.len();
            let parent_fd = fd_of(&self.made, &parent).expect("a directory is made only below one that is held");
            // Private until it is judged: nobody else can enter it, whatever the umask.
            match mkdirat(parent_fd, name.as_os_str(), Mode::from_bits_truncate(0o700)) {
                Ok(()) => {
                    let opened = open_beneath(parent_fd, name, OFlag::O_RDONLY)
                        .and_then(|dir| fstat(&dir).map(|stat| (dir, stat)));
                    let (held, stat) = match opened {
                        Ok((dir, stat)) => (
                            Some(Held {
                                dir,
                                dev: stat.st_dev,
                                ino: stat.st_ino,
                            }),
                            Ok(stat),
                        ),
                        Err(e) => (None, Err(Error::os_at("open", path, e))),
                    };
                    let index = self.made.len();
                    self.made.push(Made {
                        path: path.clone(),
                        name: name.clone(),
                        parent,
                        held,
                        hand: if last { hand_to } else { None },
                        plan_owner: hand_to,
                        handed: false,
                        left: false,
                        ready: false,
                    });
                    self.verify(index, &stat?)?;
                    parent = Parent::Made(index);
                }
                // Another install made it first, or an earlier plan of this call did: not made by this
                // plan, and judged as the walk judges one.
                Err(Errno::EEXIST) => {
                    let found = entry(parent_fd, name)
                        .map_err(|e| Error::os_at("open", path, e))?
                        .ok_or_else(|| Error::os_at("open", path, io::Error::from(io::ErrorKind::NotFound)))?;
                    if found.uid() != 0 {
                        return Err(Error::DirectoryRefused {
                            path: path.clone(),
                            why: format!(
                                "it was made by another install, and is owned by uid {}, not root",
                                found.uid()
                            ),
                        });
                    }
                    if last {
                        let stat = require_directory(parent_fd, name, &found, path)?;
                        // One an earlier plan of this call made, by identity: names can differ and be one
                        // directory (a file system that ignores case).
                        if let Some(ours) = self.made.iter_mut().find(|made| {
                            made.held
                                .as_ref()
                                .is_some_and(|held| held.dev == stat.st_dev && held.ino == stat.st_ino)
                        }) && let Some(owner) = hand_to
                        {
                            match ours.hand {
                                // Every plan of one install names one account.
                                Some(first) => debug_assert_eq!(
                                    first,
                                    owner,
                                    "a later plan hands {} to another owner",
                                    path.display()
                                ),
                                // A plan that hands nothing over is not this one's to hand over: that
                                // directory is root's, and a path that is to be the account's cannot be it
                                // or hold it. One that hands over is.
                                None if ours.plan_owner.is_none() => {
                                    return Err(Error::DirectoryRefused {
                                        path: path.clone(),
                                        why: format!(
                                            "it is, or lies above, a directory this install made for another path of \
                                             the spec, which is not handed to an account, and this path is to be \
                                             handed to {} (two names can be one directory on a file system that \
                                             ignores case): keep the two paths apart",
                                            describe_uid(owner.uid.as_raw())
                                        ),
                                    });
                                }
                                None => ours.hand = Some(owner),
                            }
                        }
                        return Ok(());
                    }
                    let Entry::Dir(held, _) = found else {
                        return Err(Error::NotADirectory { path: path.clone() });
                    };
                    parent = Parent::Existing(held);
                }
                Err(e) => return Err(Error::os_at("create directory", path, e)),
            }
        }
        Ok(())
    }

    /// Judge the directory just made, and open it up to `0755`.
    fn verify(&mut self, index: usize, stat: &FileStat) -> Result<()> {
        let made = &mut self.made[index];
        let held = made
            .held
            .as_ref()
            .expect("a directory is judged through the descriptor it was opened by");
        let empty = is_empty(&held.dir).map_err(|e| Error::os_at("read", &made.path, e))?;
        judge(stat, empty).map_err(|why| Error::DirectoryRefused {
            path: made.path.clone(),
            why,
        })?;
        fchmod(&held.dir, Mode::from_bits_truncate(0o755)).map_err(|e| Error::os_at("chmod", &made.path, e))?;
        made.ready = true;
        Ok(())
    }

    /// Give every directory marked for an owner to it. The first that cannot be given is
    /// the error: the install fails, and the guard removes what is made.
    fn hand_over(&mut self) -> Result<()> {
        for made in self.made.iter_mut().filter(|made| !made.handed) {
            if let (Some(owner), Some(held)) = (made.hand, &made.held) {
                fchown(&held.dir, Some(owner.uid), owner.gid)
                    .map_err(|e| Error::os_at(format!("hand over to uid {}", owner.uid), &made.path, e))?;
                made.handed = true;
            }
        }
        Ok(())
    }

    /// Let go of everything made: the directories stay. One that is ready and marked for an owner and
    /// not yet handed to it (a `make` that did not finish) is handed over first, as `make` would have.
    /// Returns a note for each directory that stays other than as made (not verified, see
    /// `Made::ready`; or not handed over), for the caller to report.
    fn keep(&mut self) -> Vec<String> {
        let mut notes = Vec::new();
        for made in &self.made {
            if !made.ready {
                notes.push(format!(
                    "{}, which the install created, was never verified: it is left behind",
                    made.path.display()
                ));
            } else if let (false, Some(owner), Some(held)) = (made.handed, made.hand, &made.held)
                && let Err(e) = fchown(&held.dir, Some(owner.uid), owner.gid)
            {
                notes.push(format!(
                    "{}, which the install created, could not be handed over to uid {}: {e}; it is left \
                     owned by root",
                    made.path.display(),
                    owner.uid
                ));
            }
        }
        self.made.clear();
        notes
    }

    /// [`keep`](Self::keep), reporting what it returns.
    fn keep_and_report(&mut self) {
        for note in self.keep() {
            report(format_args!("{note}"));
        }
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
        for index in (0..self.made.len()).rev() {
            if let Err(e) = self.remove(index) {
                report(format_args!(
                    "could not remove {}, which the failed install created: {e}",
                    self.made[index].path.display()
                ));
            }
        }
    }
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
            Standing::Yes => self.keep_and_report(),
            Standing::No => {}
            Standing::Unknown(reason) => {
                report(format_args!("{reason}; keeping the directories the install created"));
                self.keep_and_report();
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
            self.keep_and_report();
        } else {
            self.undo();
        }
    }
}

/// A warning on stderr. A failed write is ignored: `eprintln!` would panic on one.
fn report(message: std::fmt::Arguments<'_>) {
    let _ = writeln!(io::stderr(), "warning: {message}");
}

#[cfg(test)]
#[path = "created_dirs_tests.rs"]
pub(crate) mod created_dirs_tests;
