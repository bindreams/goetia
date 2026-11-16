//! The record the shim writes for each run of a task, and that `run` and status read.
//!
//! One file per run, `<ProgramData>\Goetia\runs\<id>\<instance>.json`, named by the 32 hex digits of
//! the instance GUID Task Scheduler gave the run. It is written to `<instance>.json.tmp` and renamed
//! over its name, so a reader sees a whole record or none.
//!
//! **No clock decides anything.** A record is pruned when a later run writes, and only when nobody
//! waits for what it holds: a waiting `run` takes a [`Pin`] before it asks Task Scheduler to start the
//! task, and while any pin is live a write prunes nothing. With none, a write removes every record
//! but its own. A pin is a file held open: it goes when its holder does, however it ends; one left
//! by a power cut is held by nothing, and the next write finds it by an exclusive open and deletes
//! it. What a pin's holder did not see yet is whatever [`Runs::appeared`] lists: the records written
//! after the pin's directory listing, which was taken after the pin.
//!
//! **`started` only orders** the records of runs that overlap: no decision compares it to the time.
//!
//! The directory's permissions name the task's account ([`Runs::ensure`]) because the shim runs
//! as it: SYSTEM and Administrators have full control and the account may modify the files made in
//! it (and delete entries of the directory, but not add a subdirectory), and nobody else reaches
//! it. The permissions are written on every `ensure`, to the directory alone and never to what is
//! in it, so a reinstall that changes the account can still write and prune what the old account
//! wrote.
//!
//! The folders above it are goetia's to make ([`Runs::ensure_roots`]): a standard user can create
//! `%ProgramData%\Goetia` first, so one that exists must be one goetia could trust, and `runs` must
//! be as goetia makes it. None is repaired. `Goetia` and its `logs` are made by an elevated install and
//! never by a shim: [`make_goetia_folders`] does it, called when an install of any daemon that runs
//! through the shim host begins (a service or a task, a change between them included) and by
//! [`Runs::ensure_roots`].

mod files;

use std::collections::BTreeSet;
use std::fs;
use std::io::{self, Write as _};
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use tempfile::Builder;

use super::folder::{goetia_folder, make_goetia_folders};
use super::shim::acl::{Descriptor, FolderCheck, is_as_wanted, missing_components, read_path};
use super::sid::Sid;
use crate::error::{Error, Result};
use crate::spec::Id;

use super::instance::InstanceId;

/// The end of a record's file name; a name's stem is the instance's [`InstanceId::hex`].
const RECORD: &str = ".json";
/// The end of the file a record is written to before it is renamed.
const TEMP: &str = ".json.tmp";
/// The end of a pin's file name.
const PIN: &str = ".pin";

/// How a run ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum RunOutcome {
    /// The program exited with this code.
    Exited(u32),
    /// A stop was asked for and the program's tree ended.
    Stopped,
    /// A stop was asked for and something of the program's tree could not be ended.
    IncompleteStop,
}

/// What the shim writes about one run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RunRecord {
    pub instance: InstanceId,
    /// When the run began, as a `FILETIME`. Orders the records of runs that overlap, and is compared
    /// with nothing else.
    pub started: u64,
    pub outcome: RunOutcome,
}

/// What a record file holds: the instance is its name.
#[derive(Serialize, Deserialize)]
struct Stored {
    started: u64,
    outcome: RunOutcome,
}

/// The directory of one task's records.
pub(crate) struct Runs {
    dir: PathBuf,
}

/// A reader's claim on the records of one task, and what the directory held when it was made. While
/// it is held no write prunes; dropping it, or the holder ending, releases it.
pub(crate) struct Pin {
    /// Held open, and deleted when closed.
    _file: fs::File,
    /// The records that were there already.
    listing: BTreeSet<InstanceId>,
}

/// What a task's directory holds that is goetia's.
enum Entry {
    Record(InstanceId),
    Temp(PathBuf),
    Pin(PathBuf),
}

impl Runs {
    /// The records of task `id` under `program_data`: `<program_data>\Goetia\runs\<id>`.
    pub(crate) fn of(program_data: &std::path::Path, id: &Id) -> Runs {
        Runs {
            dir: goetia_folder(program_data).join("runs").join(id.to_string()),
        }
    }

    /// Sees to the folders above the task's directory, which a standard user can make before goetia
    /// does (`%ProgramData%` lets any user create a folder in it). None is ever adopted or repaired:
    ///
    /// - `Goetia` and `Goetia\logs` are [`make_goetia_folders`]'s, judged by `folders`: one that is
    ///   missing is made, one that is there must pass over its whole chain or it is refused
    ///   ([`Error::FolderUnsafe`]), and one that appears after it was found missing is refused too.
    /// - `runs` that is missing is made with [`Descriptor::for_folder`]. One that is there must be as
    ///   that is (owner, protection and entries), or it is refused ([`Error::FolderNotAsMade`]).
    ///
    /// Call it before [`Runs::ensure`] on every write of a task, since a reinstall may find them gone.
    pub(crate) fn ensure_roots(&self, folders: &FolderCheck) -> Result<()> {
        let runs_root = self.dir.parent().expect("a task's directory is in `runs`");
        let goetia = runs_root.parent().expect("`runs` is in `Goetia`");
        let program_data = goetia.parent().expect("`Goetia` is in `%ProgramData%`");
        make_goetia_folders(program_data, |path| folders.check(path))?;
        let wanted = Descriptor::for_folder()?;
        let missing = missing_components(runs_root)?;
        if !missing.is_empty() {
            return wanted.create_each(&missing);
        }
        if is_as_wanted(&read_path(runs_root)?, &wanted.wanted()?) {
            Ok(())
        } else {
            Err(Error::FolderNotAsMade {
                path: runs_root.to_path_buf(),
            })
        }
    }

    /// Makes the directory, in `runs`, which [`Runs::ensure_roots`] saw to, with permissions that name
    /// `principal`, the account the task runs as; or, when it is there, sets its permissions to name it
    /// ([`Descriptor::set_dacl`]: the directory alone, never what is in it). Call it on every write of
    /// the task, so that a reinstall that changes the account can still write.
    pub(crate) fn ensure(&self, principal: &Sid) -> io::Result<()> {
        let permissions = Descriptor::for_runs(principal).map_err(io::Error::other)?;
        match permissions.create_dir(&self.dir) {
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => permissions.set_dacl(&self.dir),
            made => made,
        }
    }

    /// Writes the record of a run, replacing one of the same instance, then prunes. The record is
    /// whole once this returns `Ok`; a prune that cannot remove a file leaves it for the next write.
    pub(crate) fn write(&self, record: &RunRecord) -> io::Result<()> {
        let name = record.instance.hex();
        let temp = self.dir.join(format!("{name}{TEMP}"));
        let body = serde_json::to_vec(&Stored {
            started: record.started,
            outcome: record.outcome,
        })
        .map_err(io::Error::other)?;

        let mut file = files::create_temp(&temp)?;
        let landed = file
            .write_all(&body)
            .and_then(|()| file.sync_all())
            .and_then(|()| files::rename_over(&file, &self.dir.join(format!("{name}{RECORD}"))));
        drop(file);
        if let Err(e) = landed {
            // Best effort: a temp that stays is pruned by a later write.
            let _ = fs::remove_file(&temp);
            return Err(e);
        }
        self.prune(&record.instance);
        Ok(())
    }

    /// The record of the run `instance`, if it has one.
    pub(crate) fn read(&self, instance: &InstanceId) -> io::Result<Option<RunRecord>> {
        match fs::read(self.dir.join(format!("{}{RECORD}", instance.hex()))) {
            Ok(bytes) => decode(*instance, &bytes).map(Some),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// Takes a pin, then lists the records there are. Do it before asking for the run whose record
    /// is waited for: whatever [`Runs::appeared`] lists is then newer than the pin.
    pub(crate) fn pin(&self) -> io::Result<Pin> {
        let mut names = Builder::new();
        names.prefix("pin-").suffix(PIN).disable_cleanup(true);
        let (file, _path) = names.make_in(&self.dir, files::create_pin)?.into_parts();
        let listing = self
            .entries()?
            .into_iter()
            .filter_map(|entry| match entry {
                Entry::Record(instance) => Some(instance),
                Entry::Temp(_) | Entry::Pin(_) => None,
            })
            .collect();
        Ok(Pin { _file: file, listing })
    }

    /// The records that are not in `pin`'s listing, the earliest `started` first (ties by instance).
    ///
    /// A directory that is gone is a task uninstalled while its `run` waited ([`Runs::remove_all`]):
    /// an error of kind `NotFound` that says so.
    pub(crate) fn appeared(&self, pin: &Pin) -> io::Result<Vec<RunRecord>> {
        let entries = self.entries().map_err(|e| match e.kind() {
            io::ErrorKind::NotFound => io::Error::new(
                io::ErrorKind::NotFound,
                format!(
                    "the run records in {} are gone: the task was uninstalled while a run waited for it",
                    self.dir.display()
                ),
            ),
            _ => e,
        })?;
        let mut appeared = Vec::new();
        for entry in entries {
            let Entry::Record(instance) = entry else { continue };
            if pin.listing.contains(&instance) {
                continue;
            }
            // Gone since the listing: a write pruned it, which only happens when nothing waits for it.
            if let Some(record) = self.read(&instance)? {
                appeared.push(record);
            }
        }
        appeared.sort_by_key(|record| (record.started, record.instance));
        Ok(appeared)
    }

    /// Whether the task's directory is there to remove. A directory that cannot be looked at is taken
    /// to be (the removal then says why not).
    pub(crate) fn is_present(&self) -> bool {
        !matches!(self.dir.try_exists(), Ok(false))
    }

    /// Removes the directory and everything in it, pins too: the task is uninstalled. A directory
    /// that is not there is already removed. Uninstalling stops the task first; a `run` that still
    /// waits then is the user's own concurrent action, and does not hold the removal up: the
    /// directory goes at once (a pin's holder keeps its open handle, which no longer names a file),
    /// and that `run`'s next [`Runs::appeared`] fails saying so.
    pub(crate) fn remove_all(&self) -> io::Result<()> {
        match fs::remove_dir_all(&self.dir) {
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
            result => result,
        }
    }

    /// After a write of `own`: deletes the pins nothing holds, and when no pin is held, every record
    /// but `own` and every temp file. A file that cannot be removed now (a write in flight holds its
    /// temp file) is for the next write.
    fn prune(&self, own: &InstanceId) {
        self.prune_listing(own, || self.entries());
    }

    /// [`Runs::prune`] over `list`, which is called twice: the first listing decides what may be
    /// deleted, the second what is pinned. **That order keeps a waiting reader's records.** A record
    /// written after a pin was taken is in the first listing only if the pin already existed, so the
    /// second shows it; a pin taken after the first listing has every record of the first in its own
    /// listing. One listing would rest on the order a directory read returns a pin and a record in,
    /// which is not promised; pins read first would miss a pin taken between the two.
    fn prune_listing(&self, own: &InstanceId, list: impl Fn() -> io::Result<Vec<Entry>>) {
        let Ok(entries) = list() else { return };
        let Ok(pins) = list() else { return };
        let mut held = false;
        for entry in &pins {
            if let Entry::Pin(path) = entry {
                held |= files::is_held(path);
            }
        }
        if held {
            return;
        }
        for entry in entries {
            let doomed = match entry {
                Entry::Record(instance) if instance != *own => self.dir.join(format!("{}{RECORD}", instance.hex())),
                Entry::Temp(path) => path,
                Entry::Record(_) | Entry::Pin(_) => continue,
            };
            let _ = fs::remove_file(doomed);
        }
    }

    /// What the directory holds that is goetia's, in no order.
    fn entries(&self) -> io::Result<Vec<Entry>> {
        let mut entries = Vec::new();
        for entry in fs::read_dir(&self.dir)? {
            let entry = entry?;
            let Ok(name) = entry.file_name().into_string() else {
                continue;
            };
            if name.ends_with(TEMP) {
                entries.push(Entry::Temp(entry.path()));
            } else if name.ends_with(PIN) {
                entries.push(Entry::Pin(entry.path()));
            } else if let Some(instance) = name
                .strip_suffix(RECORD)
                .filter(|stem| stem.len() == 32)
                .and_then(InstanceId::parse)
            {
                entries.push(Entry::Record(instance));
            }
        }
        Ok(entries)
    }
}

fn decode(instance: InstanceId, bytes: &[u8]) -> io::Result<RunRecord> {
    let stored: Stored = serde_json::from_slice(bytes).map_err(io::Error::other)?;
    Ok(RunRecord {
        instance,
        started: stored.started,
        outcome: stored.outcome,
    })
}

#[cfg(test)]
#[path = "runs_tests.rs"]
mod runs_tests;
