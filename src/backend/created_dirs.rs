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
//!   that a group or everyone can write is accepted: that is the administrator's exposure, and
//!   what it allows here is covered under "Making". Where writability matters the caller judges it:
//!   for a log systemd opens as root, `refuse_writable_log_dir` (Linux).
//! - **Making.** Each component is made `0700` by `mkdirat` under the descriptor of the one above,
//!   and recorded at once, so that one that is then refused is removed too. It must be root's, a
//!   directory, and empty (a link count or a mode says nothing a file system keeps the same), and is
//!   then made `0755`. A parent that another can write (Debian's `/var/log` is `root:syslog 0775`) can
//!   at most swap in an empty directory of root's, which that group could already remove.
//! - **Handing over.** Only the deepest directory of a path, and only where the daemon's account has
//!   to write it ([`Owner`]), is given to the account, and only once the definition that names it
//!   stands ([`settle`](Settle::settle) or a panic's keep). Until then every directory is root's, so
//!   an install that fails leaves none an account could write into. Only a directory that was
//!   verified when it was made is handed over.
//!
//! An install that fails, or leaves a definition other than the one it wrote, removes what it made,
//! deepest first, by descriptor: an entry is unlinked only if it is still the directory that was made
//! and is empty, and a symlink is never followed. One that could not be opened when it was made cannot
//! be told from what is at its name later: it is left, and reported.

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
/// A descriptor stays open per directory made until the drop; running out of them fails the install
/// and leaves what was made to the drop.
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

/// A directory made and marked for an owner, not yet handed to it.
#[cfg(target_os = "linux")]
#[derive(Debug)]
pub(crate) struct PendingHandOver {
    pub(crate) dev: libc::dev_t,
    pub(crate) ino: libc::ino_t,
    pub(crate) path: PathBuf,
    pub(crate) owner: Owner,
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
    /// Whom it is handed to, if anyone.
    hand: Option<Owner>,
    /// Judged and given its mode: only such a directory is handed over.
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

impl Made {
    /// Give it to its owner, if it has one.
    fn hand_over(&self) -> Result<()> {
        if let (Some(owner), Some(held)) = (self.hand, &self.held) {
            fchown(&held.dir, Some(owner.uid), owner.gid)
                .map_err(|e| Error::os_at(format!("hand over to uid {}", owner.uid), &self.path, e))?;
        }
        Ok(())
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

    /// Make `dir` and every missing ancestor, root's, and have the deepest of them handed to `hand_to`
    /// by a [`settle`](Settle::settle) that keeps it. A path that exists whole is used as it is, and a directory
    /// this guard made as an ancestor of another path is the one handed over if `dir` names it. A
    /// missing component needs the path to lead through a root and every directory and link on the
    /// way to be root's.
    pub(crate) fn create(&mut self, dir: &Path, hand_to: Option<Owner>) -> Result<()> {
        self.create_for(dir, || Ok(hand_to))
    }

    /// [`create`](Self::create) for an owner that is looked up: `owner` runs, once, only when a
    /// directory is to be handed over, after the path is judged and before anything is made. An
    /// existing path, or one this guard made and an earlier call marked, needs no owner. `Ok(None)` is
    /// no hand-over.
    pub(crate) fn create_for(&mut self, dir: &Path, owner: impl FnOnce() -> Result<Option<Owner>>) -> Result<()> {
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
        self.make(found, owner)
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

    /// Make what `found` says is missing under the directory it holds, or, for a path that exists,
    /// mark the directory it names for the hand-over if this guard made it.
    fn make(&mut self, found: Walk, owner: impl FnOnce() -> Result<Option<Owner>>) -> Result<()> {
        let (holder, missing) = match found.end {
            End::Whole(stat) => {
                if let Some(made) = self.made.iter_mut().find(|made| {
                    made.held
                        .as_ref()
                        .is_some_and(|held| held.dev == stat.st_dev && held.ino == stat.st_ino)
                }) && made.hand.is_none()
                {
                    made.hand = owner()?;
                }
                return Ok(());
            }
            End::Missing { holder, missing } => (holder, missing),
        };
        let hand_to = owner()?;
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
                        ready: false,
                    });
                    self.verify(index, &stat?)?;
                    parent = Parent::Made(index);
                }
                // Another install made it first: not ours, and judged as the walk judges one.
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
                        require_directory(parent_fd, name, &found, path)?;
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

    /// The directories marked for an owner that a settle that keeps them will give it.
    #[cfg(target_os = "linux")]
    pub(crate) fn to_hand_over(&self) -> Vec<PendingHandOver> {
        self.made
            .iter()
            .filter_map(|made| {
                let held = made.held.as_ref()?;
                Some(PendingHandOver {
                    dev: held.dev,
                    ino: held.ino,
                    path: made.path.clone(),
                    owner: made.hand?,
                })
            })
            .collect()
    }

    /// Unmark every directory, so that a settle that keeps them hands nothing over: for an install the
    /// log check refused, whose definition may stand all the same when its standing cannot be read.
    #[cfg(target_os = "linux")]
    pub(crate) fn withhold_hand_over(&mut self) {
        self.made.iter_mut().for_each(|made| made.hand = None);
    }

    /// Hand over what is marked, and let go of all of it: the directories stay. Only a directory that
    /// was verified when it was made is handed over; one that was not (refused, or never opened) is left
    /// as it is. What did not go as it should is returned for the caller to report, and the rest still
    /// go.
    fn keep(&mut self) -> Vec<String> {
        let mut notes = Vec::new();
        for made in &self.made {
            if !made.ready {
                notes.push(format!(
                    "{}, which the install created, was never verified: it is left behind, and not handed over",
                    made.path.display()
                ));
            } else if let Err(e) = made.hand_over() {
                notes.push(format!(
                    "could not hand {} over, which the install created and keeps: {}",
                    made.path.display(),
                    e.report()
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
    /// made and it is empty. A directory that is gone is removed; one that has gained content stays,
    /// without a word.
    fn remove(&self, index: usize) -> std::result::Result<(), String> {
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
            Ok(()) | Err(Errno::ENOENT | Errno::ENOTEMPTY | Errno::EEXIST) => Ok(()),
            Err(e) => Err(e.to_string()),
        }
    }

    /// Remove what can be removed, deepest first, since each was made after its parent. A directory
    /// that stays keeps its ancestors, which then hold content.
    fn undo(&self) {
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
    /// Removes what was not kept: see [`undo`](CreatedDirs::undo). A panic may come after the
    /// definition was written, which leaves its standing as unknown as an unreadable one does, so the
    /// directories are kept, and kept means handed over. A step that fails is reported on stderr, and
    /// a failed report is ignored: this never panics.
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
