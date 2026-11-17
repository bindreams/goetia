//! The record the shim writes for each run of a task, and that `run` and status read.
//!
//! One file per run, `<ProgramData>\Goetia\runs\<id>\<instance>.json`, named by the 32 hex digits of
//! the instance GUID Task Scheduler gave the run. It is written to `<instance>.json.tmp` and renamed
//! over its name, so a reader sees a whole record or none.
//!
//! **No clock decides anything.** A record is pruned when a later run writes, and only when nobody
//! waits for what it holds: a waiting `run` takes a [`Pin`] before it asks Task Scheduler to start the
//! task, and while any pin is live a write prunes nothing. With none, a write removes every record
//! but its own. A pin is a file held open with no sharing: it goes when its holder does, however it
//! ends, and while it is held nothing can delete it, whoever tries. One left by a power cut is held by
//! nothing, and the next write finds it by deleting it: a sharing violation says it is live. What a
//! pin's holder did not see yet is whatever [`Runs::appeared`] lists: the records written after the
//! pin's directory listing, which was taken after the pin. **No operation deletes a record while a pin
//! is held:** a write prunes nothing, and [`Runs::remove_all`] waits for the pins first. A pin made
//! after that wait began is refused, by the marker the removal places first ([`Runs::hold`]).
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
use std::thread;
use std::time::Duration;

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
/// The marker file of a removal under way, which a new pin looks for ([`Runs::remove_all`]).
const REMOVING: &str = ".removing";
/// The pace of [`Runs::remove_all`]'s re-checks while a pin is held. Each re-check decides; the pace only
/// keeps the loop from spinning (the shim's re-reads of Task Scheduler's listing run at the same pace).
const PIN_PACE: Duration = Duration::from_millis(100);

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

/// A reader's claim on the records of one task, and usually what the directory held when it was made.
/// While it is held no write prunes; dropping it, or the holder ending, releases it.
pub(crate) struct Pin {
    /// Held open, and deleted when closed.
    _file: fs::File,
    /// The records that were there already, when the pin was made by [`Runs::pin`].
    listing: Option<BTreeSet<InstanceId>>,
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
    ///
    /// It also takes away a removal's marker that an uninstall left when it died ([`Runs::remove_all`]):
    /// the task is installed again, and its `run` must not back off.
    pub(crate) fn ensure(&self, principal: &Sid) -> io::Result<()> {
        let permissions = Descriptor::for_runs(principal).map_err(io::Error::other)?;
        match permissions.create_dir(&self.dir) {
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => permissions.set_dacl(&self.dir),
            made => made,
        }?;
        match fs::remove_file(self.dir.join(REMOVING)) {
            Err(e) if e.kind() != io::ErrorKind::NotFound => Err(e),
            Err(_) | Ok(()) => Ok(()),
        }
    }

    /// Writes the record of a run, replacing one of the same instance, then prunes. The record is
    /// whole once this returns `Ok`; a prune that cannot remove a file leaves it for the next write.
    /// What the prune could not do, and a failure of its own (not a file that is gone or held), is
    /// returned as one line each for the caller to log: it never fails the write.
    pub(crate) fn write(&self, record: &RunRecord) -> io::Result<Vec<String>> {
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
        Ok(self.prune(&record.instance))
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
        let mut pin = self.hold()?;
        let listing = self
            .entries()?
            .into_iter()
            .filter_map(|entry| match entry {
                Entry::Record(instance) => Some(instance),
                Entry::Temp(_) | Entry::Pin(_) => None,
            })
            .collect();
        pin.listing = Some(listing);
        Ok(pin)
    }

    /// Takes a pin and lists nothing, for a holder that only keeps the records there are from being
    /// pruned and reads them by name. [`Runs::appeared`] needs the listing [`Runs::pin`] takes.
    ///
    /// The pin is made first and the removal's marker looked for after, the opposite order of
    /// [`Runs::remove_all`]'s: so either the removal sees this pin and waits, or this finds the marker
    /// and drops the pin. A pin that finds the marker is refused with an [`io::ErrorKind::NotFound`]
    /// error, as one in a directory already removed is.
    pub(crate) fn hold(&self) -> io::Result<Pin> {
        let mut names = Builder::new();
        names.prefix("pin-").suffix(PIN).disable_cleanup(true);
        let (file, _path) = names.make_in(&self.dir, files::create_pin)?.into_parts();
        let pin = Pin {
            _file: file,
            listing: None,
        };
        if self.dir.join(REMOVING).try_exists()? {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                "its run records are being removed: the task is being uninstalled",
            ));
        }
        Ok(pin)
    }

    /// The records that are not in `pin`'s listing, the earliest `started` first (ties by instance).
    /// The directory is there: `pin` is a file in it that nothing can delete while it is held.
    ///
    /// # Panics
    ///
    /// If `pin` was made by [`Runs::hold`], which has no listing to tell what is new by.
    pub(crate) fn appeared(&self, pin: &Pin) -> io::Result<Vec<RunRecord>> {
        let listing = pin
            .listing
            .as_ref()
            .expect("`appeared` needs a pin made by `Runs::pin`, which lists");
        let mut appeared = Vec::new();
        for entry in self.entries()? {
            let Entry::Record(instance) = entry else { continue };
            if listing.contains(&instance) {
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
    /// that is not there is already removed.
    ///
    /// **Nothing is deleted while a pin is held.** A `run` that waits for a record holds one, and
    /// uninstalling stops the task first, so that `run` returns soon after; the removal waits for it,
    /// re-checking the pins every [`PIN_PACE`] with no cap, and only then deletes.
    ///
    /// **A pin cannot be taken in the moment between the check and the delete.** The removal places
    /// a marker file (`.removing`) before it looks at the pins; a pin is made before it looks for the
    /// marker ([`Runs::hold`]). Either the pin is there when the removal looks, or the marker is there
    /// when the pin looks and the pin is dropped. The marker is deleted last, after every other file,
    /// so that a pin taken during the delete finds it. A file another program holds open (a sharing
    /// violation) is left, the rest deleted, and the removal goes back to the wait. A marker left by a
    /// removal that died is placed again and deleted with the directory; an install clears it
    /// ([`Runs::ensure`]).
    ///
    /// `waiting` is called once, before the first pause, so the caller can say what is awaited. A pin
    /// whose state cannot be told is not waited for: the delete reports its own failure.
    pub(crate) fn remove_all(&self, waiting: impl FnOnce()) -> io::Result<()> {
        let gone = |e: &io::Error| e.kind() == io::ErrorKind::NotFound;
        let mut waiting = Some(waiting);
        loop {
            // The marker first, then the pins: see [`Runs::hold`].
            match fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(false)
                .open(self.dir.join(REMOVING))
            {
                Ok(_) => {}
                Err(e) if gone(&e) => return Ok(()),
                Err(e) => return Err(e),
            }
            let held = match self.pin_held() {
                Ok(held) => held,
                Err(e) if gone(&e) => return Ok(()),
                Err(e) => return Err(e),
            };
            if !held {
                match self.delete_all_but_marker() {
                    Ok(false) => match self.remove_marker_and_dir() {
                        Ok(true) => return Ok(()),
                        Ok(false) => {}
                        Err(e) if gone(&e) => return Ok(()),
                        Err(e) => return Err(e),
                    },
                    Ok(true) => {}
                    Err(e) if gone(&e) => return Ok(()),
                    Err(e) => return Err(e),
                }
            }
            if let Some(note) = waiting.take() {
                note();
            }
            thread::sleep(PIN_PACE);
        }
    }

    /// Deletes every entry of the directory but the marker, and returns whether one is left because a
    /// handle holds it (a sharing violation). The marker stays until the rest is gone, so that no pin
    /// is taken while the records are being deleted ([`Runs::hold`]).
    fn delete_all_but_marker(&self) -> io::Result<bool> {
        let mut left = false;
        for entry in fs::read_dir(&self.dir)? {
            let entry = entry?;
            if entry.file_name() == REMOVING {
                continue;
            }
            let path = entry.path();
            let deleted = if entry.file_type().is_ok_and(|kind| kind.is_dir()) {
                fs::remove_dir_all(&path)
            } else {
                fs::remove_file(&path)
            };
            match deleted {
                Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                Err(e) if files::is_sharing_violation(&e) => left = true,
                Err(e) => return Err(e),
                Ok(()) => {}
            }
        }
        Ok(left)
    }

    /// Removes the marker and then the directory, which holds nothing else. Returns `false` when
    /// something was put in it since (a pin taken in the moment after the marker went), for the removal
    /// to go back to its wait.
    fn remove_marker_and_dir(&self) -> io::Result<bool> {
        match fs::remove_file(self.dir.join(REMOVING)) {
            Err(e) if e.kind() != io::ErrorKind::NotFound => return Err(e),
            Err(_) | Ok(()) => {}
        }
        match fs::remove_dir(&self.dir) {
            Ok(()) => Ok(true),
            Err(e) if e.kind() == io::ErrorKind::DirectoryNotEmpty || files::is_sharing_violation(&e) => Ok(false),
            Err(e) => Err(e),
        }
    }

    /// Whether some pin in the directory is held. A pin that nothing holds is deleted by the asking
    /// ([`files::is_held`]); one that cannot be told about is not held.
    fn pin_held(&self) -> io::Result<bool> {
        Ok(self
            .entries()?
            .iter()
            .any(|entry| matches!(entry, Entry::Pin(path) if matches!(files::is_held(path), Ok(true)))))
    }

    /// After a write of `own`: deletes the pins nothing holds, and when no pin is held, every record
    /// but `own` and every temp file. A file that cannot be removed now (a write in flight holds its
    /// temp file) is for the next write. Returns what went wrong that is not that.
    fn prune(&self, own: &InstanceId) -> Vec<String> {
        self.prune_listing(own, || self.entries())
    }

    /// [`Runs::prune`] over `list`, which is called twice: the first listing decides what may be
    /// deleted, the second what is pinned. **That order keeps a waiting reader's records.** A record
    /// written after a pin was taken is in the first listing only if the pin already existed, so the
    /// second shows it; a pin taken after the first listing has every record of the first in its own
    /// listing. One listing would rest on the order a directory read returns a pin and a record in,
    /// which is not promised; pins read first would miss a pin taken between the two.
    ///
    /// Returns what went wrong, one line each, besides a file that is gone or held. A pin that cannot be
    /// deleted for another reason than a sharing violation says nothing about a handle: it is taken as
    /// not held, since reading it as held would stop every later prune for good.
    fn prune_listing(&self, own: &InstanceId, list: impl Fn() -> io::Result<Vec<Entry>>) -> Vec<String> {
        let listing = |e: io::Error| {
            vec![format!(
                "list the run records in {} to prune them: {e}",
                self.dir.display()
            )]
        };
        let entries = match list() {
            Ok(entries) => entries,
            Err(e) => return listing(e),
        };
        let pins = match list() {
            Ok(pins) => pins,
            Err(e) => return listing(e),
        };
        let mut problems = Vec::new();
        let mut held = false;
        for entry in &pins {
            if let Entry::Pin(path) = entry {
                match files::is_held(path) {
                    Ok(pinned) => held |= pinned,
                    // Taken as not held, whatever the failure was.
                    Err(e) => problems.push(format!(
                        "delete the pin {} to tell whether a run holds it: {e}",
                        path.display()
                    )),
                }
            }
        }
        if held {
            return problems;
        }
        for entry in entries {
            let doomed = match entry {
                Entry::Record(instance) if instance != *own => self.dir.join(format!("{}{RECORD}", instance.hex())),
                Entry::Temp(path) => path,
                Entry::Record(_) | Entry::Pin(_) => continue,
            };
            if let Err(e) = fs::remove_file(&doomed)
                && e.kind() != io::ErrorKind::NotFound
                && !files::is_sharing_violation(&e)
            {
                problems.push(format!("remove {}: {e}", doomed.display()));
            }
        }
        problems
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
