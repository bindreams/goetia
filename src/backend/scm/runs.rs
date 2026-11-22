//! The record the shim writes for each run of a task, and that `run` and status read.
//!
//! One file per run, `<ProgramData>\Goetia\runs\<id>\<instance>.json`, named by the 32 hex digits of
//! the instance GUID Task Scheduler gave the run. It is written to `<instance>.json.tmp` and renamed to its
//! name (the provisional file below to `<instance>.stopping.tmp`, a temp name of its own), so a reader sees a whole
//! record or none. **Nothing is ever renamed onto a name that exists** (the
//! operating system refuses it, [`files::rename_to`]): no program that holds a file open can keep a record
//! from landing.
//!
//! A run whose stop is heard with its program alive writes a second kind of file first, the provisional
//! `<instance>.stopping` ([`Pin::write_stopping`]), which holds its `Stopped`, with its pin lent (no release,
//! no prune: the run is still alive, and "no pin held" means "no run alive"). Its record follows, to
//! `<instance>.json` as every run's does, with the pin given. A reader takes the record when there is one, and
//! the provisional file alone as `Stopped` ([`Runs::read`]).
//!
//! **No clock decides anything.** A record is pruned when a later run writes, and only when nobody
//! waits for what it holds: a waiting `run` takes a [`Pin`] before it asks Task Scheduler to start the
//! task, and while any pin is live a write prunes nothing. With none, a write removes every record
//! but its own, every provisional file (its own run's too: its record has landed) and every temp file.
//! **A run pins too:** the shim takes a pin before it reads anything, and holds it until its
//! own record is renamed into place; it releases it before its own prune, so a run's pin never stops its
//! own prune. A pin is a file held open with no sharing: it
//! goes when its holder does, however it ends, and while it is held nothing can delete it, whoever tries.
//! One left by a power cut is held by nothing, and the next write finds it by deleting it: a sharing
//! violation says it is live ([`files::delete`] reads every failed delete, here and in the removal). What a
//! pin's holder did not see yet is whatever [`Pin::appeared`] lists: the runs that have a file the pin's
//! directory listing, which was taken after the pin, did not hold, the record of each when it has one and its
//! provisional file when it has not. **No operation deletes a record or a provisional file while a pin
//! is held:** a write prunes nothing, and [`Runs::remove_all`] waits for the pins first. A pin made
//! after the marker is placed is refused ([`Runs::hold`]), but for the one pin of the uninstall that placed
//! it, taken past it ([`MarkerCheck::Ignore`]) and let go before the removal waits. The pins the removal
//! waits for are those made before the marker.
//!
//! **`started` only orders** the records of runs that overlap: no decision compares it to the time.
//!
//! The directory's permissions name the task's account ([`Runs::ensure`]) because the shim runs as it:
//! SYSTEM and Administrators have full control, and nobody else reaches it but the account, within the
//! masks of `Descriptor::for_runs`. The account can make the directory a mount point, so the directory is
//! reached only through [`Dir`] (see the note of its module); [`Runs::remove_all`] alone takes a link, and
//! deletes it. Every file in the directory is made, opened and deleted relative to that handle, never by
//! path. The permissions are written on every `ensure`, to the directory alone and never to what is in it,
//! so a reinstall that changes the account can still write and prune what the old account wrote.
//!
//! The folders above it are goetia's to make ([`Runs::ensure_roots`]): a standard user can create
//! `%ProgramData%\Goetia` first, so one that exists must be one goetia could trust, and `runs` must
//! be as goetia makes it. None is repaired. `Goetia` and its `logs` are made by an elevated install and
//! never by a shim: [`make_goetia_folders`] does it, called when an install of any daemon that runs
//! through the shim host begins (a service or a task, a change between them included) and by
//! [`Runs::ensure_roots`].

pub(in crate::backend::scm) mod dir;
mod files;

use std::collections::BTreeSet;
use std::ffi::{OsStr, OsString};
use std::fs;
use std::io::{self, Write as _};
use std::path::{Path, PathBuf};
use std::thread;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tempfile::Builder;

use self::dir::Dir;
use self::files::DeleteOutcome;
use super::folder::{ensure_as_made, goetia_folder, make_goetia_folders};
use super::shim::acl::{Descriptor, FolderCheck};
use super::sid::{Sid, distinct};
use crate::error::{Error, Result};
use crate::spec::Id;

use super::instance::InstanceId;

/// The end of a record's file name; a name's stem is the instance's [`InstanceId::hex`].
const RECORD: &str = ".json";
/// The end of the name of a run's provisional file, which holds its first `Stopped`: the same stem as the record's.
const STOPPING: &str = ".stopping";
/// The end of the file a record is written to before it is renamed.
const TEMP: &str = ".json.tmp";
/// The end of the file a provisional file is written to before it is renamed: a name of its own, so that nothing
/// that stays of a failed provisional write is in the way of the record's.
const STOPPING_TEMP: &str = ".stopping.tmp";
/// The end of a pin's file name.
const PIN: &str = ".pin";
/// The end of the name of a removal's marker file, `runs\<id>~removing`, which a new pin looks for
/// ([`Runs::remove_all`]). An id has no `~`, so no task's directory is named like it.
const REMOVING: &str = "~removing";
/// The pace of [`Runs::remove_all`]'s passes while something is held or the directory is not yet
/// empty. Each re-check decides; the pace only keeps the loop from spinning (the shim's re-reads of
/// Task Scheduler's listing run at the same pace).
const REMOVE_PACE: Duration = Duration::from_millis(100);

/// How a run ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum RunOutcome {
    /// The program exited with this code.
    Exited(u32),
    /// A stop was accepted and the shim never recorded how the program ended: no program was started, its wait
    /// failed, its own teardown ended it, Windows ended the shim first, or the final write failed. For the last two
    /// the run left only its provisional file, which a reader takes as this. It does not say the program's tree has
    /// ended. A program the shim saw exit is `Exited`, stop or not (see the module doc of `shim::run::task`).
    Stopped,
    /// A stop was accepted and something of the program's tree could not be ended.
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

/// What a refusal says when the removal's marker stands: an install or an uninstall of `id` is running or did not
/// finish, and only those two finish it. One text for every refusal the marker causes.
pub(crate) fn unfinished_change(id: &str) -> String {
    format!(
        "`{id}` is being, or did not finish being, installed or uninstalled; if nothing is doing that now, run the \
         install again with `--force`, or run `goetia daemon uninstall {id}`"
    )
}

/// The directory of one task's records.
pub(crate) struct Runs {
    id: String,
    /// `<ProgramData>\Goetia\runs`, which holds the directories and the markers.
    root: PathBuf,
    dir: PathBuf,
    /// The removal's marker, beside `dir` so that it outlasts it: see [`Runs::remove_all`].
    marker: PathBuf,
}

/// A reader's claim on the records of one task, and usually what the directory held when it was made.
/// While it is held no write prunes; dropping it, or the holder ending, releases it.
pub(crate) struct Pin {
    /// Held open, and deleted when closed.
    _file: fs::File,
    /// The directory the pin was made in, opened once for the pin's life: what the pin reads is read
    /// through it, never through the path again.
    dir: Dir,
    /// The files that were there already, when the pin was made by [`Runs::pin`].
    listing: Option<BTreeSet<(InstanceId, FileKind)>>,
}

impl Pin {
    /// The runs that have a file the pin's listing did not hold, one entry per run, the earliest `started` first
    /// (ties by instance): its record when it has one, else its provisional file, which reads as `Stopped`. A
    /// listing holds each file by instance and kind, so a provisional file that was there before the pin is not
    /// new, and a record that lands after it is. The directory is there: the pin is a file in it that nothing can
    /// delete while it is held. It is read through the handle the pin was made through.
    ///
    /// # Panics
    ///
    /// If the pin was made by [`Runs::hold`], which has no listing to tell what is new by.
    pub(crate) fn appeared(&self) -> io::Result<Vec<RunRecord>> {
        let listing = self
            .listing
            .as_ref()
            .expect("`appeared` needs a pin made by `Runs::pin`, which lists");
        let new: BTreeSet<InstanceId> = entries(&self.dir)?
            .iter()
            .filter_map(Entry::file)
            .filter(|file| !listing.contains(file))
            .map(|(instance, _)| instance)
            .collect();
        let mut appeared = Vec::new();
        for instance in new {
            // Gone since the listing: a write pruned it, which only happens when nothing waits for it.
            if let Some(record) = read_in(&self.dir, &instance)? {
                appeared.push(record);
            }
        }
        appeared.sort_by_key(|record| (record.started, record.instance));
        Ok(appeared)
    }

    /// Writes the provisional file of the run this pin is the shim's own of, which is alive: its `Stopped`, which a
    /// stop that finds the program alive writes before it waits, since Windows may end the shim before anything
    /// later. It is a file of its own, `<instance>.stopping`. The pin stays held: nothing is pruned (the run is alive, and "no pin held" means "no run alive"). The
    /// file lands in the pin's directory, through its handle; the body is the one that writes a record, and a run
    /// that has a provisional file is refused ([`io::ErrorKind::AlreadyExists`]).
    pub(crate) fn write_stopping(&self, instance: &InstanceId, started: u64) -> io::Result<()> {
        let stored = Stored {
            started,
            outcome: RunOutcome::Stopped,
        };
        put(
            &self.dir,
            &stopping_temp_name(instance),
            &stopping_name(instance),
            &stored,
        )
    }
}

/// Which of a run's two files a name is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum FileKind {
    Stopping,
    Record,
}

/// What a task's directory holds, by what its name says it is. `directory` is whether it is a directory or a
/// directory link, whatever its name: only SYSTEM or an Administrator can put one there, and the removal
/// takes it with its tree ([`Runs::remove_all`]).
enum Entry {
    Record {
        instance: InstanceId,
        name: OsString,
        directory: bool,
    },
    /// The provisional file of a run's first `Stopped`.
    Stopping {
        instance: InstanceId,
        name: OsString,
        directory: bool,
    },
    Temp {
        name: OsString,
        directory: bool,
    },
    Pin {
        name: OsString,
        directory: bool,
    },
    /// A name goetia gives to nothing: only a removal touches it. The task's account can make a file of any
    /// name.
    Other {
        name: OsString,
        directory: bool,
    },
}

impl Entry {
    fn name(&self) -> &OsStr {
        match self {
            Entry::Record { name, .. }
            | Entry::Stopping { name, .. }
            | Entry::Temp { name, .. }
            | Entry::Pin { name, .. }
            | Entry::Other { name, .. } => name,
        }
    }

    /// The instance and kind of a file that is a record or a provisional file; none for a directory of such a name.
    fn file(&self) -> Option<(InstanceId, FileKind)> {
        match self {
            Entry::Record {
                instance,
                directory: false,
                ..
            } => Some((*instance, FileKind::Record)),
            Entry::Stopping {
                instance,
                directory: false,
                ..
            } => Some((*instance, FileKind::Stopping)),
            _ => None,
        }
    }

    fn directory(&self) -> bool {
        match self {
            Entry::Record { directory, .. }
            | Entry::Stopping { directory, .. }
            | Entry::Temp { directory, .. }
            | Entry::Pin { directory, .. }
            | Entry::Other { directory, .. } => *directory,
        }
    }
}

/// How [`Runs::close`] went.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Closed {
    /// The marker stands.
    Placed,
    /// `runs` is gone, so no marker was placed: there are no records.
    NoRecords,
}

/// Whether a pin looks for the removal's marker once it is made ([`Runs::hold`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MarkerCheck {
    /// A marker that stands refuses the pin: the shim's, a `run`'s, a stop's.
    Refuse,
    /// The pin is made whatever stands: the holder placed the marker itself.
    Ignore,
}

/// What a [`Runs::remove_all`] pause waits for, for the note it makes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PauseKind {
    /// A handle holds the path: a pin, a file a shim is writing or another program has open, the directory
    /// itself.
    Held,
    /// The directory is not empty and nothing is known to hold what keeps it so: a file went between the
    /// delete that found it there and the listing that was to name it.
    Emptying,
}

impl Runs {
    /// The records of task `id` under `program_data`: `<program_data>\Goetia\runs\<id>`.
    pub(crate) fn of(program_data: &std::path::Path, id: &Id) -> Runs {
        let runs = goetia_folder(program_data).join("runs");
        Runs {
            id: id.to_string(),
            dir: runs.join(id.to_string()),
            marker: runs.join(format!("{id}{REMOVING}")),
            root: runs,
        }
    }

    /// Sees to the folders above the task's directory, which a standard user can make before goetia
    /// does (`%ProgramData%` lets any user create a folder in it). None is ever adopted or repaired:
    ///
    /// - `Goetia` and `Goetia\logs` are [`make_goetia_folders`]'s, judged by `folders`: one that is
    ///   missing is made, one that is there must pass over its whole chain or it is refused
    ///   ([`Error::FolderUnsafe`]), and one that appears after it was found missing is refused too.
    /// - `runs` that is missing is made with [`Descriptor::for_folder`]. One that is there must be as
    ///   that is (owner, protection and entries), or it is refused ([`Error::FolderNotAsMade`]); so is
    ///   `logs`, which [`make_goetia_folders`] makes protected.
    ///
    /// Call it before [`Runs::ensure`] on every write of a task, since a reinstall may find them gone.
    pub(crate) fn ensure_roots(&self, folders: &FolderCheck) -> Result<()> {
        make_goetia_folders(self.program_data(), |path| folders.check(path))?;
        ensure_as_made(&self.root, &Descriptor::for_folder()?)
    }

    pub(crate) fn program_data(&self) -> &Path {
        let goetia = self.root.parent().expect("`runs` is in `Goetia`");
        goetia.parent().expect("`Goetia` is in `%ProgramData%`")
    }

    /// Makes the directory, in `runs`, which [`Runs::ensure_roots`] saw to, with permissions that name
    /// `principals`, the accounts the task runs as; or, when it is there, sets its permissions to name them
    /// ([`Descriptor::set_dacl_of`]: the directory alone, never what is in it). Call it on every write of
    /// the task, so that a reinstall that changes the account can still write.
    ///
    /// A directory that is there and is a link (the account can make one: see the module's note) is
    /// refused ([`io::ErrorKind::NotADirectory`]) and nothing is written to what it points to; the
    /// uninstall removes it ([`Runs::remove_all`]).
    ///
    /// It also takes away a removal's marker that an uninstall left when it died ([`Runs::remove_all`]), and the
    /// one an account change placed ([`Runs::close`]) once it has deleted the old registration: the task is
    /// installed again, and its `run` must not back off.
    pub(crate) fn ensure(&self, principals: &[Sid]) -> io::Result<()> {
        let permissions = Descriptor::for_runs(&distinct(principals)).map_err(io::Error::other)?;
        match permissions.create_dir(&self.dir) {
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
                permissions.set_dacl_of(Dir::open_for_dacl(&self.dir)?.file())
            }
            made => made,
        }?;
        self.reopen()
    }

    /// Read-only: refuses what [`Runs::ensure`] would refuse of an existing directory (a link: NotADirectory). A
    /// missing directory passes; so does a permissions write that later fails.
    pub(crate) fn check_directory(&self) -> io::Result<()> {
        match Dir::open(&self.dir) {
            Ok(_) => Ok(()),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e),
        }
    }

    /// Places the removal's marker, beside the directory: from then on no pin is accepted ([`Runs::hold`]),
    /// except one taken with [`MarkerCheck::Ignore`] by whoever placed it. Placing it again changes nothing.
    /// With `runs` gone nothing is placed ([`Closed::NoRecords`]): there are no records to protect.
    pub(crate) fn close(&self) -> Result<Closed> {
        match fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(false)
            .open(&self.marker)
        {
            Ok(_) => Ok(Closed::Placed),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(Closed::NoRecords),
            Err(e) => Err(Error::os_at("place the removal's marker", &self.marker, e)),
        }
    }

    /// Takes the removal's marker away; one that is not there is `Ok`. A marker another program holds is an
    /// error as any: the install that calls this is run again.
    pub(crate) fn reopen(&self) -> io::Result<()> {
        self.delete_marker()
            .into_result()
            .map_err(|e| io::Error::new(e.kind(), format!("{}: {e}", self.marker.display())))
    }

    /// Whether the removal's marker stands: what a pin looks for after it is made ([`Runs::hold`]).
    pub(crate) fn is_closed(&self) -> io::Result<bool> {
        self.marker.try_exists()
    }

    /// The task's directory of records, for a message that names it.
    pub(crate) fn dir(&self) -> &Path {
        &self.dir
    }

    /// Writes the record of a run, then prunes. The record is whole once this returns `Ok`; a prune that cannot
    /// remove a file leaves it for the next write. What the prune could not do, and a failure of its own (not a
    /// file that is gone or held), is returned as one line each for the caller to log: it never fails the write. A
    /// directory that is a link is refused ([`Runs::ensure`]), and nothing is written.
    ///
    /// **Nothing is renamed onto a name that exists:** a record for an instance that has one is refused
    /// ([`io::ErrorKind::AlreadyExists`]) and the first is as it was. A run's first `Stopped` is another file
    /// ([`Pin::write_stopping`]).
    ///
    /// `own` is the writer's pin: held until the record is renamed into place, then released before the prune,
    /// which a held pin would stop. A failed write releases it too. The write goes through the pin's directory
    /// handle, not the path.
    pub(crate) fn write(&self, record: &RunRecord, own: Option<Pin>) -> io::Result<Vec<String>> {
        let (dir, own) = match own {
            Some(Pin { _file, dir, .. }) => (dir, Some(_file)),
            None => (Dir::open(&self.dir)?, None),
        };
        let stored = Stored {
            started: record.started,
            outcome: record.outcome,
        };
        let landed = put(
            &dir,
            &record_temp_name(&record.instance),
            &record_name(&record.instance),
            &stored,
        );
        drop(own);
        landed?;
        Ok(self.prune(&dir, &record.instance))
    }

    /// The record of the run `instance`: its record when it has one, else, if its provisional file is there, the
    /// run as `Stopped` with the `started` the file holds; else none. A directory that is not there, or is a link
    /// (the account can make one), holds no records.
    pub(crate) fn read(&self, instance: &InstanceId) -> io::Result<Option<RunRecord>> {
        match Dir::open(&self.dir).and_then(|dir| read_in(&dir, instance)) {
            Err(e) if matches!(e.kind(), io::ErrorKind::NotFound | io::ErrorKind::NotADirectory) => Ok(None),
            read => read,
        }
    }

    /// Takes a pin, then lists the records there are. Do it before asking for the run whose record
    /// is waited for: whatever [`Pin::appeared`] lists is then newer than the pin.
    pub(crate) fn pin(&self) -> io::Result<Pin> {
        let mut pin = self.hold_in(Dir::open(&self.dir)?, MarkerCheck::Refuse)?;
        let listing = entries(&pin.dir)?.iter().filter_map(Entry::file).collect();
        pin.listing = Some(listing);
        Ok(pin)
    }

    /// Takes a pin and lists nothing, for a holder that only keeps the records there are from being
    /// pruned and reads them by name. [`Pin::appeared`] needs the listing [`Runs::pin`] takes.
    ///
    /// The pin is made first and the removal's marker looked for after, the opposite order of
    /// [`Runs::remove_all`]'s: so either the removal sees this pin and waits, or this finds the marker
    /// and drops the pin. The marker is beside the directory and goes after it, so no pin is accepted at
    /// any point of the delete; one about to be refused can be in the directory for a moment, and is waited
    /// for as any is. A pin that finds the marker is refused with an [`io::ErrorKind::ResourceBusy`]
    /// error ([`unfinished_change`]).
    pub(crate) fn hold(&self) -> io::Result<Pin> {
        self.hold_with(MarkerCheck::Refuse)
    }

    /// [`Runs::hold`] with the marker looked for as `check` says: the removal's own pin, taken after the
    /// removal placed the marker, is [`MarkerCheck::Ignore`].
    pub(crate) fn hold_with(&self, check: MarkerCheck) -> io::Result<Pin> {
        self.hold_in(Dir::open(&self.dir)?, check)
    }

    /// [`Runs::hold`] in `dir`, which was opened for the pin's life; the marker is looked for as `check`
    /// says. A holder that placed the marker itself ([`Runs::close`]) takes its own pin past it.
    fn hold_in(&self, dir: Dir, check: MarkerCheck) -> io::Result<Pin> {
        let mut names = Builder::new();
        names.prefix("pin-").suffix(PIN).disable_cleanup(true);
        let made = names.make_in(&self.dir, |path| {
            files::create_pin(&dir, path.file_name().expect("a name was made"))
        });
        // Looked for when the pin was not made as well: a removal deleting the directory fails the create,
        // and that is no "no records".
        let removing = match check {
            MarkerCheck::Refuse => self.is_closed(),
            MarkerCheck::Ignore => Ok(false),
        };
        if matches!(removing, Ok(true)) {
            return Err(io::Error::new(
                io::ErrorKind::ResourceBusy,
                format!(
                    "the run records of `{}` are closed to a new pin: {}",
                    self.id,
                    unfinished_change(&self.id)
                ),
            ));
        }
        let (file, _path) = made?.into_parts();
        removing?;
        Ok(Pin {
            _file: file,
            dir,
            listing: None,
        })
    }

    /// The first pin a handle holds, if one does: the directory listed once, every pin asked
    /// ([`pins_of`]), and never waited for. A pin whose state cannot be told may be held, and is an error that
    /// names it. A directory that is gone, or is a link, holds none. **Not read-only:** each stale pin is
    /// removed as it is judged ([`scan_pins`]).
    pub(crate) fn held_pin(&self) -> Result<Option<PathBuf>> {
        let dir = match Dir::open_even_if_link(&self.dir) {
            Ok(dir) => dir,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(Error::os_at("open", &self.dir, e)),
        };
        if dir.is_link() {
            return Ok(None);
        }
        let entries = match entries(&dir) {
            Ok(entries) => entries,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(Error::os_at("list", &self.dir, e)),
        };
        Ok(match pins_of(&dir, &entries, "tell whether a run holds")? {
            Pins::Held(path) => Some(path),
            Pins::Free | Pins::Linked => None,
        })
    }

    /// Whether the task has records, or a removal's marker, to remove. A link where the directory was
    /// is a directory to remove, a dangling one too. A path that cannot be looked at is taken to be there
    /// (the removal then says why not). The marker is looked for as well, since a removal that died after
    /// its directory went leaves it, and a `run` of the id would back off for good
    /// ([`Runs::remove_all`]).
    pub(crate) fn is_present(&self) -> bool {
        let directory = !matches!(Dir::open_even_if_link(&self.dir), Err(e) if e.kind() == io::ErrorKind::NotFound);
        directory || !matches!(self.marker.try_exists(), Ok(false))
    }

    /// Removes the directory and everything in it, pins too: the task is uninstalled. A directory that is
    /// not there is removed already, and a link where it should be is deleted and never followed.
    ///
    /// **Nothing is deleted while a pin is held.** A `run` that waits for a record holds one, and
    /// uninstalling stops the task first, so that `run` returns soon after; the removal waits for it, with
    /// no cap. A pin cannot be taken once the removal has begun: it places the marker `runs\<id>~removing`
    /// first, and a pin looks for it ([`Runs::hold`]).
    ///
    /// Something held (a pin, a file a shim is writing, the directory), or a directory not yet empty,
    /// pauses it for [`REMOVE_PACE`], and another pass follows ([`Runs::remove_pass`]), with no cap:
    /// whatever holds it lets go in time, and the user can interrupt the command. `waiting` is called with
    /// the path being waited for, and why ([`PauseKind`]), before a pause, and again each time that path
    /// changes, so the caller can say what is awaited. A pause that is [`PauseKind::Emptying`] calls it once
    /// for the phase, with the directory; a hold that follows one, or an emptying that follows a hold, is
    /// told again ([`Said`]). Any other failure ends the removal, naming the path and why, and leaves the
    /// marker, for a `run` to keep backing off while records may be partly gone; an install clears it
    /// ([`Runs::ensure`]).
    ///
    /// **A limit, stated:** a directory someone placed in the task's directory that a program holds open
    /// (a handle on a file in it, say) fails the removal, which names that directory; once the program
    /// lets go, or the directory is removed by hand, run the removal again.
    ///
    /// **Another program acting on the directory is not guarded against.** Only SYSTEM and Administrators
    /// can delete, rename or change the permissions of the directory itself, so a program of theirs that
    /// does it while the removal runs (deleting it while it holds it open, say) is the user's own concurrent
    /// action. The removal does not tell a delete that is pending from a refusal: it fails, naming the
    /// path and the error Windows gave, and a re-run finishes once the program has let go.
    pub(crate) fn remove_all(&self, mut waiting: impl FnMut(&Path, PauseKind)) -> Result<()> {
        // The marker first, then the pins: see [`Runs::hold`]. With `runs` gone so is everything in it.
        if self.close()? == Closed::NoRecords {
            return Ok(());
        }
        let mut said = Said::Nothing;
        loop {
            let pass = self.remove_pass()?;
            if matches!(pass, PassOutcome::Done) {
                return Ok(());
            }
            said.note(&self.dir, &pass, &mut waiting);
            thread::sleep(REMOVE_PACE);
        }
    }

    /// One pass of [`Runs::remove_all`]: the pins, then the rest of the directory, then the directory,
    /// then the marker. It lists the directory once, through its handle, and deletes through
    /// [`files::delete`]: deleted (a delete that is pending counts), held by some handle, or failed.
    ///
    /// The pins come first. A pin that failed ends the removal, with every such pin named, before anything
    /// else is deleted: it may be held, and no delete may run under a pin. One that is held pauses it. When
    /// none is, every other entry is deleted; a record or a temp file that is held pauses it. A directory,
    /// whatever its name, is removed with its tree, links in it deleted and not followed, and what stops
    /// that ends the removal, naming the directory: a handle in it is not waited for, since only SYSTEM or
    /// an Administrator can have put one there. Last the directory itself, which a handle that shares no
    /// delete holds (a pause) and which goes at once for any other. Not yet empty (a pin being refused, a
    /// file whose delete is pending, what a shim wrote since the listing) pauses it, naming the smallest
    /// entry left, or the directory if none is ([`PauseKind::Emptying`]).
    ///
    /// A link where the directory should be, or one it became since the listing (an empty directory can be
    /// made one), is no failure: the first operation relative to the handle says so
    /// ([`DeleteOutcome::Linked`]), and the pass goes on to the directory's own delete, which deletes the
    /// link.
    fn remove_pass(&self) -> Result<PassOutcome> {
        let dir = match Dir::open_even_if_link(&self.dir) {
            Ok(dir) => dir,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return self.remove_marker(),
            Err(e) => return Err(Error::os_at("open", &self.dir, e)),
        };
        if !dir.is_link() {
            let entries = match entries(&dir) {
                Ok(entries) => entries,
                Err(e) if e.kind() == io::ErrorKind::NotFound => return self.remove_marker(),
                Err(e) => return Err(Error::os_at("list", &self.dir, e)),
            };
            if let Some(held) = self.delete_entries(&dir, &entries)? {
                return Ok(PassOutcome::Waiting(held));
            }
        }
        match dir.remove() {
            Ok(()) => self.remove_marker(),
            Err(e) if e.kind() == io::ErrorKind::NotFound => self.remove_marker(),
            // A handle that shares no delete holds the directory; one that shares it does not (measured).
            Err(e) if files::is_sharing_violation(&e) => Ok(PassOutcome::Waiting(self.dir.clone())),
            Err(e) if e.kind() == io::ErrorKind::DirectoryNotEmpty => self.wait_for_what_is_left(),
            Err(e) => Err(Error::os_at("remove", &self.dir, e)),
        }
    }

    /// What follows a directory that was not empty: a second listing says what is left
    /// ([`pass_after_not_empty`]). The directory can be gone by now, and is then done; a listing with
    /// nothing in it is no hold to name.
    fn wait_for_what_is_left(&self) -> Result<PassOutcome> {
        let listed = Dir::open_even_if_link(&self.dir).and_then(|dir| entries(&dir));
        match listed {
            Ok(entries) => Ok(pass_after_not_empty(&self.dir, &entries)),
            Err(e) if e.kind() == io::ErrorKind::NotFound => self.remove_marker(),
            Err(e) => Err(Error::os_at("list", &self.dir, e)),
        }
    }

    /// The marker's delete, which ends a removal whose directory is gone.
    fn remove_marker(&self) -> Result<PassOutcome> {
        match self.delete_marker() {
            DeleteOutcome::Held => Ok(PassOutcome::Waiting(self.marker.clone())),
            done => done
                .into_result()
                .map(|()| PassOutcome::Done)
                .map_err(|e| Error::os_at("remove", &self.marker, e)),
        }
    }

    /// Deletes the marker, in `runs`, through a handle on `runs` ([`files::delete`]): one that is not there
    /// is deleted, and so is one in a `runs` that is gone.
    fn delete_marker(&self) -> DeleteOutcome {
        let name = self.marker.file_name().expect("the marker has a name");
        match Dir::open(&self.root) {
            Ok(root) => files::delete(&root, name),
            Err(e) if e.kind() == io::ErrorKind::NotFound => DeleteOutcome::Deleted,
            Err(e) => DeleteOutcome::Failed(e),
        }
    }

    /// The deletes of a pass over `entries`: the pins, then, when none is held, everything else. Returns
    /// the first path that is held, if any; a failure ends the pass.
    ///
    /// A `dir` that became a link since `entries` was listed has none of them: the pass stops at the first
    /// delete that says so, with nothing held and nothing failed, and the caller deletes the link.
    fn delete_entries(&self, dir: &Dir, entries: &[Entry]) -> Result<Option<PathBuf>> {
        match pins_of(dir, entries, "remove")? {
            Pins::Linked => return Ok(None),
            Pins::Held(path) => return Ok(Some(path)),
            Pins::Free => {}
        }
        let mut held = None;
        for entry in entries {
            let name = entry.name();
            let outcome = match entry {
                // A tree is no file: whatever stops its removal is a failure naming the top directory.
                _ if entry.directory() => files::remove_tree(dir, name),
                Entry::Pin { .. } => continue,
                _ => files::delete(dir, name),
            };
            match outcome {
                DeleteOutcome::Deleted => {}
                DeleteOutcome::Held => {
                    held.get_or_insert_with(|| dir.path().join(name));
                }
                DeleteOutcome::Failed(e) => return Err(Error::os_at("remove", dir.path().join(name), e)),
                DeleteOutcome::Linked => return Ok(None),
            }
        }
        Ok(held)
    }

    /// After a write of `own`: deletes the pins nothing holds, and when no pin is held, every record but `own`,
    /// every provisional file and every temp file. A file that cannot be removed now (a write in flight holds
    /// its temp file) is for the next write. Returns what went wrong that is not that.
    fn prune(&self, dir: &Dir, own: &InstanceId) -> Vec<String> {
        self.prune_listing(dir, own, || entries(dir))
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
    fn prune_listing(&self, dir: &Dir, own: &InstanceId, list: impl Fn() -> io::Result<Vec<Entry>>) -> Vec<String> {
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
        let scan = scan_pins(dir, &pins);
        // An unclassified pin is taken as not held, whatever the failure was.
        let mut problems: Vec<String> = scan
            .unclassified
            .iter()
            .map(|(path, e)| format!("delete the pin {} to tell whether a run holds it: {e}", path.display()))
            .collect();
        if scan.held.is_some() || scan.linked {
            return problems;
        }
        for entry in entries {
            let doomed = match &entry {
                Entry::Record { instance, name, .. } if instance != own => name,
                // The writer's own run's too: its record has landed, and the record wins.
                Entry::Stopping { name, .. } | Entry::Temp { name, .. } => name,
                Entry::Record { .. } | Entry::Pin { .. } | Entry::Other { .. } => continue,
            };
            // A file that is gone, held or on its way out is for the next write.
            match files::delete(dir, doomed) {
                DeleteOutcome::Failed(e) => {
                    problems.push(format!("remove {}: {e}", dir.path().join(doomed).display()));
                }
                // The directory became a link: nothing in the listing is in it, and none of it is touched.
                DeleteOutcome::Linked => break,
                DeleteOutcome::Deleted | DeleteOutcome::Held => {}
            }
        }
        problems
    }
}

/// Writes `stored` to `to` in `dir` through the temp file `temp`, and renames the temp onto `to`, so a
/// reader sees a whole file or none. The rename is refused if `to` exists ([`files::rename_to`]). A failure
/// removes the temp, best effort: one that stays is pruned by a later write, and a failure to remove it that is no
/// handle's hold is in the error ([`after_cleanup`]).
fn put(dir: &Dir, temp: &OsStr, to: &OsStr, stored: &Stored) -> io::Result<()> {
    let body = serde_json::to_vec(stored).map_err(io::Error::other)?;
    let mut file = files::create_temp(dir, temp)?;
    let landed = file
        .write_all(&body)
        .and_then(|()| file.sync_all())
        .and_then(|()| files::rename_to(dir, &file, to));
    drop(file);
    landed.map_err(|failed| after_cleanup(failed, temp, files::delete(dir, temp)))
}

/// `failed`, the error of a write, told with the failure to remove its temp file `temp` when `removed` says the
/// removal failed for a reason that is no handle's hold (a held file is for the next prune). The kind is the write's.
fn after_cleanup(failed: io::Error, temp: &OsStr, removed: DeleteOutcome) -> io::Error {
    match removed {
        DeleteOutcome::Failed(cleanup) => io::Error::new(
            failed.kind(),
            format!(
                "{failed}; and its temp file {} could not be removed: {cleanup}",
                Path::new(temp).display()
            ),
        ),
        DeleteOutcome::Deleted | DeleteOutcome::Held | DeleteOutcome::Linked => failed,
    }
}

/// The file name of the record of `instance`.
fn record_name(instance: &InstanceId) -> OsString {
    format!("{}{RECORD}", instance.hex()).into()
}

/// The file name of the provisional file of `instance`.
pub(crate) fn stopping_name(instance: &InstanceId) -> OsString {
    format!("{}{STOPPING}", instance.hex()).into()
}

/// The name of the temp file a record is written through.
fn record_temp_name(instance: &InstanceId) -> OsString {
    format!("{}{TEMP}", instance.hex()).into()
}

/// The name of the temp file a provisional file is written through.
fn stopping_temp_name(instance: &InstanceId) -> OsString {
    format!("{}{STOPPING_TEMP}", instance.hex()).into()
}

/// The record of `instance` in `dir`, else its provisional file, else none. **The record is the answer whatever
/// the provisional file's read gave:** only when there is no record does that read decide, its error included.
///
/// **The provisional file is read first,** so that a record that lands between the two reads is found by the
/// second: read the other way round, the record would be missed and the provisional file still there, and a run
/// that was recorded would read as `Stopped`. A read with no pin can still answer `Stopped` for a run whose record
/// was pruned in between.
///
/// A provisional file holds `Stopped`, as [`Pin::write_stopping`] stores it, and is taken as stored: another
/// outcome in it is a corrupt record.
fn read_in(dir: &Dir, instance: &InstanceId) -> io::Result<Option<RunRecord>> {
    let provisional = files::read(dir, &stopping_name(instance));
    if let Some(bytes) = files::read(dir, &record_name(instance))? {
        return decode(*instance, &bytes).map(Some);
    }
    let Some(bytes) = provisional? else {
        return Ok(None);
    };
    let record = decode(*instance, &bytes)?;
    if record.outcome != RunOutcome::Stopped {
        return Err(io::Error::other(format!(
            "the provisional file of run {} holds {:?}, and only `Stopped` is written to one",
            instance.hex(),
            record.outcome
        )));
    }
    Ok(Some(record))
}

/// The instance a file name `text` is of, if it is the instance's 32 hex digits and `suffix`.
fn instance_named(text: &str, suffix: &str) -> Option<InstanceId> {
    text.strip_suffix(suffix)
        .filter(|stem| stem.len() == 32)
        .and_then(InstanceId::parse)
}

/// What the directory holds, in no order. One listing, over the names as the file system gives them: a
/// name that is not Unicode is classified by its ending all the same (the replacement character that
/// stands for what is not Unicode is no part of `.pin` or `.json.tmp`), so a pin the task's account made
/// with such a name is a pin.
fn entries(dir: &Dir) -> io::Result<Vec<Entry>> {
    Ok(dir
        .list()?
        .into_iter()
        .map(|listed| {
            let text = listed.name.to_string_lossy().into_owned();
            let (name, directory) = (listed.name, listed.directory);
            if text.ends_with(TEMP) || text.ends_with(STOPPING_TEMP) {
                Entry::Temp { name, directory }
            } else if text.ends_with(PIN) {
                Entry::Pin { name, directory }
            } else if let Some(instance) = instance_named(&text, STOPPING) {
                Entry::Stopping {
                    instance,
                    name,
                    directory,
                }
            } else if let Some(instance) = instance_named(&text, RECORD) {
                Entry::Record {
                    instance,
                    name,
                    directory,
                }
            } else {
                Entry::Other { name, directory }
            }
        })
        .collect())
}

/// How one pass of [`Runs::remove_all`] went.
enum PassOutcome {
    /// The directory and the marker are gone.
    Done,
    /// Something is left, and this is the path of what is waited for.
    Waiting(PathBuf),
    /// Nothing is known to hold the removal back: what stopped the directory's delete went before the
    /// listing that was to name it. Another pass follows the pause.
    Retry,
}

/// What the removal last told its caller about the wait, so that each pause is told once and a pause that
/// is not what was last told is told again. One note replaces the one before: a hold that comes back after
/// an emptying phase, or a second emptying phase after a hold, is a new thing to say.
#[derive(PartialEq)]
enum Said {
    Nothing,
    Held(PathBuf),
    Emptying,
}

impl Said {
    /// Calls `waiting` for the pause `pass` is, unless it is what was said last. `dir` is the directory
    /// being removed, which an emptying phase names.
    fn note(&mut self, dir: &Path, pass: &PassOutcome, waiting: &mut impl FnMut(&Path, PauseKind)) {
        let now = match pass {
            PassOutcome::Done => return,
            PassOutcome::Retry => Said::Emptying,
            PassOutcome::Waiting(path) => Said::Held(path.clone()),
        };
        if *self == now {
            return;
        }
        match &now {
            Said::Nothing => {}
            Said::Held(path) => waiting(path, PauseKind::Held),
            Said::Emptying => waiting(dir, PauseKind::Emptying),
        }
        *self = now;
    }
}

/// The outcome of a pass whose delete of the directory `dir` found it not empty, given a listing taken
/// after: a pause naming the smallest path left, by name, so that a pause on the same thing is told once;
/// or, if the listing is empty (what was left went in between), a [`PassOutcome::Retry`], since nothing is
/// held.
fn pass_after_not_empty(dir: &Path, left: &[Entry]) -> PassOutcome {
    match left.iter().map(Entry::name).min() {
        Some(name) => PassOutcome::Waiting(dir.join(name)),
        None => PassOutcome::Retry,
    }
}

/// What [`scan_pins`] found.
struct PinScan {
    /// A pin that a handle holds, if one does.
    held: Option<PathBuf>,
    /// The pins whose state could not be told, each with why.
    unclassified: Vec<(PathBuf, io::Error)>,
    /// The directory became a link since it was listed ([`DeleteOutcome::Linked`]): the scan stopped there,
    /// and no pin of the listing is in it.
    linked: bool,
}

/// Classifies every pin of `entries` by deleting it ([`files::delete`]): held; stale (deleted by the
/// asking, so no longer there, or going when the last handle on it closes); or unclassified. Every pin is
/// looked at, so that all that cannot be told about are returned, unless the directory turns out to be a
/// link now. The callers decide what an unclassified pin means: [`Runs::remove_all`] ends, since a delete
/// must not run while a pin may be held; a prune reports it and leaves the rest to the next write.
fn scan_pins<'a>(dir: &Dir, entries: impl IntoIterator<Item = &'a Entry>) -> PinScan {
    let mut scan = PinScan {
        held: None,
        unclassified: Vec::new(),
        linked: false,
    };
    for entry in entries {
        if let Entry::Pin { name, .. } = entry {
            match files::delete(dir, name) {
                DeleteOutcome::Deleted => {}
                DeleteOutcome::Held => {
                    scan.held.get_or_insert_with(|| dir.path().join(name));
                }
                DeleteOutcome::Failed(e) => scan.unclassified.push((dir.path().join(name), e)),
                DeleteOutcome::Linked => {
                    scan.linked = true;
                    break;
                }
            }
        }
    }
    scan
}

/// What the pins of a listing say, for a delete that must not run under one.
enum Pins {
    /// None is held.
    Free,
    /// This one is, the first found.
    Held(PathBuf),
    /// The directory became a link since it was listed ([`PinScan::linked`]).
    Linked,
}

/// [`scan_pins`] over the entries of `entries` that are no directory (a directory is no pin, whatever its
/// name: a removal takes it with the rest), with a pin whose state could not be told an error naming every
/// such pin: no delete may run while a pin may be held. `doing` is what the caller was doing, for the error
/// ("remove", "tell whether a run holds").
fn pins_of(dir: &Dir, entries: &[Entry], doing: &str) -> Result<Pins> {
    let scan = scan_pins(dir, entries.iter().filter(|entry| !entry.directory()));
    if scan.linked {
        return Ok(Pins::Linked);
    }
    let unclassified = scan
        .unclassified
        .into_iter()
        .map(|(pin, e)| Error::os_at(doing, pin, e));
    if let Some(failed) = Error::both_all(unclassified) {
        return Err(failed);
    }
    Ok(match scan.held {
        Some(path) => Pins::Held(path),
        None => Pins::Free,
    })
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
