//! The stand-in unit that keeps a left run alive: see [`crate::manager::ServiceManager::uninstall`] and
//! [`generate::standin`] for what it is and why a run still in its start job needs one.
//!
//! The rules for what `uninstall` and `install` do about a run and a stand-in are in [`mod@plan`]; `Systemd::settle` (in
//! `task`) carries them out. goetia reads `/etc/systemd/system` for fragments, so a stand-in never makes an id
//! read as installed (see `discover`).

mod plan;

use std::fs;
use std::path::{Path, PathBuf};

use crate::backend::systemd::generate;
use crate::error::{Error, Result};
use crate::manager::RunInFlight;

use super::discover::{RawState, ReadFailure, classify_and_read};
use super::systemctl::Held;
use super::task::Fragment;
use super::write::{CreateOutcome, ReplaceOutcome, create_unit_in, remove_verified_with, replace_unit_verified_in};
use super::{Systemd, UnitKind, unit_name, unit_path};

pub(super) use plan::{Call, Known, Seen, Standin, plan};

/// Where systemd reads runtime unit files: a tmpfs, so a stand-in is gone at a reboot.
pub(super) const RUNTIME_UNIT_DIR: &str = "/run/systemd/system";

/// Where `id`'s stand-in is written. `/etc/systemd/system` outranks it, so an installed unit of the same
/// name is the definition systemd uses.
pub(super) fn standin_path(id: &str) -> PathBuf {
    Path::new(RUNTIME_UNIT_DIR).join(unit_name(id, UnitKind::Service))
}

/// What is at `id`'s stand-in path.
#[derive(Debug)]
pub(super) enum StandinFile {
    Absent,
    /// A stand-in of `id`'s own, with its text: the identity its replacement verifies.
    Ours(String),
    /// Anything else: somebody's runtime unit, which goetia never touches.
    NotOurs,
}

impl StandinFile {
    fn is_ours(&self) -> bool {
        matches!(self, StandinFile::Ours(_))
    }
}

pub(super) fn standin_file(id: &str) -> Result<StandinFile> {
    match classify_and_read(&standin_path(id)).map_err(ReadFailure::into_os_error)? {
        RawState::Regular(text) if generate::standin_id(&text).as_deref() == Some(id) => Ok(StandinFile::Ours(text)),
        RawState::Regular(_) | RawState::NotOurs => Ok(StandinFile::NotOurs),
        RawState::Absent => Ok(StandinFile::Absent),
    }
}

/// What a call of [`Systemd::settle`] is about.
pub(super) enum Target<'a> {
    /// `uninstall` of an installed id, whose snapshot this call removes. See [`Call`].
    Installed(&'a Fragment),
    /// See [`Call`].
    Sweep,
    /// See [`Call`].
    Install,
    /// See [`Call`].
    Update,
}

impl Target<'_> {
    /// What the call is about, for [`plan::plan`].
    pub(super) fn call(&self) -> Call {
        match self {
            Target::Installed(snapshot) if snapshot.service_text().is_some() => Call::Job,
            Target::Installed(_) => Call::TimerAlone,
            Target::Sweep => Call::Sweep,
            Target::Install => Call::Install,
            Target::Update => Call::Update,
        }
    }
}

impl Systemd {
    /// The read of `id`'s job unit a call makes before any stop, when [`Known::reads`] says it makes one. A read
    /// asks no version gate. For the sweep, whether systemd holds a definition loaded from the id's own path
    /// whose file is gone.
    pub(super) fn look(&self, id: &str, known: &Known<'_>) -> Result<Option<Seen>> {
        if !known.reads() {
            return Ok(None);
        }
        let run = self.systemctl.run_of(&unit_name(id, UnitKind::Service))?;
        let path = unit_path(id, UnitKind::Service);
        let held_gone = known.call == Call::Sweep
            && run.loaded_from(&path)
            && matches!(
                classify_and_read(&path).map_err(ReadFailure::into_os_error)?,
                RawState::Absent
            );
        Ok(Some(Seen { run, held_gone }))
    }

    /// The stand-in for the run of `id`, written without replacing a file that is not one of goetia's: a
    /// stand-in of ours already there is replaced, since its values are an earlier job's. A file that is
    /// not ours, which appeared after the check, is [`Error::ConcurrentChange`]. The text written is returned.
    pub(super) fn write_standin(
        &self,
        id: &str,
        file: &StandinFile,
        kill_signal: &str,
        stop_timeout: &str,
    ) -> Result<String> {
        let dir = Path::new(RUNTIME_UNIT_DIR);
        fs::create_dir_all(dir).map_err(|e| Error::os_at("create", dir, e))?;
        let text = generate::standin(id, kill_signal, stop_timeout);
        let raced = match file {
            StandinFile::Ours(old) => matches!(
                replace_unit_verified_in(dir, id, UnitKind::Service, &text, old)?,
                ReplaceOutcome::Raced
            ),
            StandinFile::Absent | StandinFile::NotOurs => {
                matches!(create_unit_in(dir, id, UnitKind::Service, &text)?, CreateOutcome::Raced)
            }
        };
        if raced {
            return Err(Error::ConcurrentChange {
                id: id.to_string(),
                path: Some(standin_path(id)),
            });
        }
        Ok(text)
    }

    /// The stand-in of `id` removed if it is still the file read as ours (`text`), the manager being reloaded by
    /// the caller. One that has vanished, or is another file now, is not goetia's to remove and not an error: it
    /// is left as it is. `failed`: the run is cleared with `reset-failed`, sent only once the stand-in has been
    /// found ours and moved aside; if that fails the stand-in is put back.
    pub(super) fn remove_standin(&self, id: &str, text: &str, failed: bool, held: &mut Held) -> Result<()> {
        let dir = Path::new(RUNTIME_UNIT_DIR);
        remove_verified_with(dir, id, UnitKind::Service, text, || {
            if failed {
                self.systemctl.reset_failed(id, UnitKind::Service, held)?;
            }
            Ok(())
        })
        .map(|_| ())
    }

    /// The stand-in this call wrote (`text`), after the verified removal of the timer or the job failed. A file at
    /// the job's path, ours or anybody's, outranks it, so it is stale and removed like any other. With the path
    /// empty (the job vanished since the read, or a restore failed) it is the unit's definition, and removing it
    /// would let the next reload end a run in its start job: it stays. So it does where presence cannot be
    /// settled, which is the safer error.
    pub(super) fn take_back_standin(&self, id: &str, text: &str, held: &mut Held) -> Result<()> {
        match classify_and_read(&unit_path(id, UnitKind::Service)) {
            Ok(RawState::Regular(_) | RawState::NotOurs) => self.remove_standin(id, text, false, held),
            Ok(RawState::Absent) | Err(_) => Ok(()),
        }
    }

    /// `install`'s part, `over_job` saying whether the id's job file is installed: a stand-in of ours is then
    /// stale and removed. With no job file it is the unit's definition: removed when its run has no process, and
    /// kept with one, as an install never stops a run. Asks nothing of systemd where there is no stand-in of
    /// ours, or where the job file is installed.
    pub(super) fn release_standin(&self, id: &str, over_job: bool, held: &mut Held) -> Result<()> {
        let target = if over_job { Target::Update } else { Target::Install };
        self.settle(id, RunInFlight::Leave, target, held)
    }

    /// `uninstall` of an id nothing is installed at: see rule 6 of [`mod@plan`]. A failed reload is the call's error,
    /// not "not installed".
    pub(super) fn uninstall_leftovers(&self, id: &str, in_flight: RunInFlight, held: &mut Held) -> Result<()> {
        self.settle(id, in_flight, Target::Sweep, held)
    }
}

#[cfg(test)]
#[path = "standin_tests.rs"]
mod standin_tests;
